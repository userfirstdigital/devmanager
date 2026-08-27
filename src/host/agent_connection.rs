use std::time::Duration;

use crate::domain::{
    AgentConnectionRow, AgentConnectionSnapshot, AgentPresence, ConfigSidebarProviderKind,
};
use crate::providers::adapter::{
    ProviderProbeError, ProviderProbeIoError, ProviderProbeKind, ProviderProbeRequest,
    ProviderProbeRunner,
};
use crate::providers::registry::{ProviderDiscoveryConfig, ProviderRegistry};
use crate::providers::settings::{
    ProviderDriverKind, ProviderHealthStatus, ProviderSettingsAuthority, ProviderSettingsSnapshot,
};
use crate::providers::{
    ProviderAuthState, ProviderError, ProviderExecutablePolicy, ProviderKind, ProviderObservation,
    WindowsProviderProbeRunner,
};

pub(crate) fn map_provider_observe(
    provider: ConfigSidebarProviderKind,
    result: Result<&ProviderObservation, &ProviderError>,
) -> AgentConnectionRow {
    let presence = match result {
        Ok(observation) => presence_from_auth(observation.capabilities().auth_state),
        Err(ProviderError::MissingCli { .. }) => AgentPresence::NotFound,
        Err(_) => AgentPresence::CheckFailed,
    };
    AgentConnectionRow { provider, presence }
}

pub(crate) fn presence_from_auth(auth: ProviderAuthState) -> AgentPresence {
    match auth {
        ProviderAuthState::AuthenticatedSubscription => AgentPresence::SignedIn,
        ProviderAuthState::AuthRequired | ProviderAuthState::Unknown => AgentPresence::NotSignedIn,
    }
}

fn presence_from_health_status(status: ProviderHealthStatus) -> AgentPresence {
    match status {
        ProviderHealthStatus::Healthy => AgentPresence::SignedIn,
        ProviderHealthStatus::Degraded | ProviderHealthStatus::Unavailable => {
            AgentPresence::NotSignedIn
        }
        ProviderHealthStatus::Checking
        | ProviderHealthStatus::Unknown
        | ProviderHealthStatus::StubUnsupported => AgentPresence::NotSignedIn,
    }
}

/// Immediate AgentConnection projection from the host provider-settings cache.
/// Never runs CLI probes on the exclusive host request executor.
pub(crate) fn project_agent_connection_from_settings(
    snapshot: &ProviderSettingsSnapshot,
    restore_failed_task_ids: Vec<crate::domain::TaskId>,
) -> AgentConnectionSnapshot {
    let agents = [
        ConfigSidebarProviderKind::Claude,
        ConfigSidebarProviderKind::Codex,
    ]
    .into_iter()
    .map(|provider| {
        let driver = match provider {
            ConfigSidebarProviderKind::Claude => ProviderDriverKind::Claude,
            ConfigSidebarProviderKind::Codex => ProviderDriverKind::Codex,
        };
        let default_id = match driver {
            ProviderDriverKind::Claude => "claude",
            ProviderDriverKind::Codex => "codex",
            _ => "",
        };
        let presence = snapshot
            .health
            .iter()
            .find(|row| {
                row.instance_id == default_id
                    && snapshot
                        .document
                        .get(default_id)
                        .is_some_and(|instance| instance.enabled && !instance.driver.is_stub())
            })
            .map(|row| {
                let status = match row.status.as_str() {
                    "healthy" => ProviderHealthStatus::Healthy,
                    "checking" => ProviderHealthStatus::Checking,
                    "degraded" => ProviderHealthStatus::Degraded,
                    "unavailable" => ProviderHealthStatus::Unavailable,
                    "stub_unsupported" => ProviderHealthStatus::StubUnsupported,
                    _ => ProviderHealthStatus::Unknown,
                };
                presence_from_health_status(status)
            })
            .or_else(|| {
                snapshot
                    .document
                    .instances
                    .iter()
                    .find(|instance| instance.instance_id.as_str() == default_id)
                    .map(|instance| {
                        if !instance.enabled || instance.driver.is_stub() {
                            AgentPresence::NotFound
                        } else {
                            AgentPresence::NotSignedIn
                        }
                    })
            })
            .unwrap_or(AgentPresence::NotSignedIn);
        AgentConnectionRow { provider, presence }
    })
    .collect();
    AgentConnectionSnapshot {
        agents,
        restore_failed_task_ids,
    }
}

pub(crate) fn project_agent_connection_from_authority(
    authority: &ProviderSettingsAuthority,
    restore_failed_task_ids: Vec<crate::domain::TaskId>,
) -> AgentConnectionSnapshot {
    project_agent_connection_from_settings(&authority.snapshot(), restore_failed_task_ids)
}

/// Test/helper path that still probes. Production AgentConnection must use
/// [`project_agent_connection_from_authority`] instead.
#[cfg(test)]
pub(crate) async fn probe_agents(registry: &ProviderRegistry) -> Vec<AgentConnectionRow> {
    let claude = probe_connection_row(registry, ConfigSidebarProviderKind::Claude).await;
    let codex = probe_connection_row(registry, ConfigSidebarProviderKind::Codex).await;
    vec![claude, codex]
}

#[cfg(test)]
async fn probe_connection_row(
    registry: &ProviderRegistry,
    provider: ConfigSidebarProviderKind,
) -> AgentConnectionRow {
    let kind = match provider {
        ConfigSidebarProviderKind::Claude => ProviderKind::ClaudeCode,
        ConfigSidebarProviderKind::Codex => ProviderKind::Codex,
    };
    let discovery = ProviderDiscoveryConfig::default();
    match observe_with_trusted_auth(registry, kind, &discovery).await {
        Ok(observation) => map_provider_observe(provider, Ok(&observation)),
        Err(error) => map_provider_observe(provider, Err(&error)),
    }
}

pub(crate) async fn observe_with_trusted_auth(
    registry: &ProviderRegistry,
    kind: ProviderKind,
    discovery: &ProviderDiscoveryConfig,
) -> Result<ProviderObservation, ProviderError> {
    let (invocation, observation) = registry
        .begin_auth_probe_with_observation(kind, discovery, Duration::from_secs(180))
        .await?;
    let handle = invocation.executable_handle().clone();
    let file_name = handle
        .canonical_path()
        .file_name()
        .ok_or(ProviderError::Probe(ProviderProbeError::Io(
            ProviderProbeIoError::ExecutableNotAllowed,
        )))?
        .to_string_lossy()
        .into_owned();
    let request = invocation
        .bind_request(
            ProviderProbeRequest::new(handle, ProviderProbeKind::for_auth_probe(kind))
                .map_err(|error| ProviderError::Probe(ProviderProbeError::InvalidRequest(error)))?
                .with_child_environment(discovery.child_environment.clone())
                .with_scope_fingerprint(
                    discovery
                        .instance_scope
                        .as_ref()
                        .map(|scope| scope.as_cache_key()),
                ),
        )
        .map_err(ProviderError::AuthEvidence)?;
    let policy = ProviderExecutablePolicy::new([file_name])
        .map_err(ProviderError::InvalidExecutablePolicy)?;
    let result = WindowsProviderProbeRunner::new(policy)
        .run(request.clone())
        .await
        .map_err(ProviderError::Probe)?;
    let receipt = registry.accept_auth_probe_result(invocation, request, result)?;
    registry.attach_auth_receipt(observation, receipt)
}

#[cfg(test)]
mod tests {
    use super::{map_provider_observe, presence_from_auth};
    use crate::domain::{AgentPresence, ConfigSidebarProviderKind};
    use crate::providers::{ProviderAuthState, ProviderError, ProviderKind};

    #[test]
    fn map_provider_observe_missing_cli_is_not_found() {
        let error = ProviderError::MissingCli {
            kind: ProviderKind::ClaudeCode,
            requested: None,
        };
        let row = map_provider_observe(ConfigSidebarProviderKind::Claude, Err(&error));
        assert_eq!(row.presence, AgentPresence::NotFound);
    }

    #[test]
    fn presence_from_auth_signed_in_only_for_subscription() {
        assert_eq!(
            presence_from_auth(ProviderAuthState::AuthenticatedSubscription),
            AgentPresence::SignedIn
        );
        assert_eq!(
            presence_from_auth(ProviderAuthState::AuthRequired),
            AgentPresence::NotSignedIn
        );
    }
}
