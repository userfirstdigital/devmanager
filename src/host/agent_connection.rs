use std::time::Duration;

use crate::domain::{AgentConnectionRow, AgentPresence, ConfigSidebarProviderKind};
use crate::providers::adapter::{
    ProviderProbeError, ProviderProbeIoError, ProviderProbeRequest, ProviderProbeRunner,
};
use crate::providers::registry::{ProviderDiscoveryConfig, ProviderRegistry};
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

pub(crate) async fn probe_agents(registry: &ProviderRegistry) -> Vec<AgentConnectionRow> {
    vec![
        probe_connection_row(registry, ConfigSidebarProviderKind::Claude).await,
        probe_connection_row(registry, ConfigSidebarProviderKind::Codex).await,
    ]
}

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

async fn observe_with_trusted_auth(
    registry: &ProviderRegistry,
    kind: ProviderKind,
    discovery: &ProviderDiscoveryConfig,
) -> Result<ProviderObservation, ProviderError> {
    let invocation = registry
        .begin_auth_probe(kind, discovery, Duration::from_secs(30))
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
        .bind_request(ProviderProbeRequest::auth_status(handle).map_err(|error| {
            ProviderError::Probe(ProviderProbeError::InvalidRequest(error))
        })?)
        .map_err(ProviderError::AuthEvidence)?;
    let policy =
        ProviderExecutablePolicy::new([file_name]).map_err(ProviderError::InvalidExecutablePolicy)?;
    let result = WindowsProviderProbeRunner::new(policy)
        .run(request.clone())
        .await
        .map_err(ProviderError::Probe)?;
    let receipt = registry.accept_auth_probe_result(invocation, request, result)?;
    registry
        .observe_with_auth_receipt(kind, discovery, receipt)
        .await
}

#[cfg(test)]
mod tests {
    use super::{map_provider_observe, presence_from_auth};
    use crate::domain::{AgentPresence, ConfigSidebarProviderKind};
    use crate::providers::{ProviderAuthState, ProviderError, ProviderKind};

    #[test]
    fn missing_cli_is_not_found_and_auth_states_map_to_presence() {
        assert_eq!(
            map_provider_observe(
                ConfigSidebarProviderKind::Claude,
                Err(&ProviderError::MissingCli {
                    kind: ProviderKind::ClaudeCode,
                    requested: None,
                }),
            )
            .presence,
            AgentPresence::NotFound
        );
        assert_eq!(
            presence_from_auth(ProviderAuthState::AuthenticatedSubscription),
            AgentPresence::SignedIn
        );
        assert_eq!(
            presence_from_auth(ProviderAuthState::AuthRequired),
            AgentPresence::NotSignedIn
        );
        assert_eq!(
            presence_from_auth(ProviderAuthState::Unknown),
            AgentPresence::NotSignedIn
        );
    }
}
