use std::time::Duration;

use crate::domain::{AgentConnectionRow, AgentPresence, ConfigSidebarProviderKind};
use crate::providers::adapter::{
    ProviderProbeError, ProviderProbeIoError, ProviderProbeKind, ProviderProbeRequest,
    ProviderProbeRunner,
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
    eprintln!(
        "devmanager-host: agent probe PATH={}",
        std::env::var("PATH").unwrap_or_else(|_| "<unset>".into())
    );
    let claude = probe_connection_row(registry, ConfigSidebarProviderKind::Claude).await;
    let codex = probe_connection_row(registry, ConfigSidebarProviderKind::Codex).await;
    vec![claude, codex]
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
    let started = std::time::Instant::now();
    match observe_with_trusted_auth(registry, kind, &discovery).await {
        Ok(observation) => {
            let row = map_provider_observe(provider, Ok(&observation));
            eprintln!(
                "devmanager-host: agent probe {provider:?} {} in {:?}",
                format!("{:?}", row.presence).to_ascii_lowercase(),
                started.elapsed()
            );
            row
        }
        Err(error) => {
            eprintln!(
                "devmanager-host: agent probe {provider:?} failed in {:?}: {error}",
                started.elapsed()
            );
            map_provider_observe(provider, Err(&error))
        }
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
                .map_err(|error| ProviderError::Probe(ProviderProbeError::InvalidRequest(error)))?,
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
    fn agent_connection_query_timeout_outlasts_generic_request_timeout() {
        assert!(
            crate::host::agent_connection_query_timeout()
                > crate::host::request_completion_timeout(),
            "Claude/Codex auth probes spawn real CLIs and cannot finish inside the generic 5s request deadline"
        );
        assert!(
            crate::host::agent_connection_query_timeout()
                > crate::providers::registry::provider_in_flight_ttl(),
            "Refresh IPC must wait for the full in-flight observe, not abort it as CheckFailed"
        );
        assert!(
            crate::providers::registry::provider_in_flight_ttl()
                >= std::time::Duration::from_secs(120),
            "native Claude identity hashing exceeds the old 30s in-flight deadline"
        );
    }

    #[test]
    fn agent_connection_client_query_uses_the_probe_timeout() {
        let source = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/src/client/host_client.rs"
        ));
        assert!(
            source.contains("query_with_timeout")
                && source.contains("agent_connection_query_timeout"),
            "agent connection must not use the generic 5s query deadline"
        );
    }

    #[test]
    fn provider_start_must_attach_trusted_auth_before_launch() {
        let source = include_str!("connection.rs");
        let start = source
            .find("async fn dispatch_provider_start(")
            .expect("dispatch_provider_start");
        let body = &source[start..];
        let end = body
            .find("async fn dispatch_agent_connection(")
            .expect("dispatch_agent_connection follows provider start");
        let body = &body[..end];
        assert!(
            body.contains("observe_with_trusted_auth"),
            "+Claude must launch from the same signed-in receipt Settings Refresh uses"
        );
        assert!(
            !body.contains(".observe("),
            "bare registry.observe() strips auth and always fails launch as AuthenticationRequired"
        );
    }

    #[test]
    fn agent_probes_accept_auth_receipts_in_generation_order() {
        let source = include_str!("agent_connection.rs");
        let start = source
            .find("pub(crate) async fn probe_agents(")
            .expect("probe_agents");
        let body = &source[start..];
        let end = body
            .find("async fn probe_connection_row(")
            .expect("probe_connection_row follows probe_agents");
        let body = &body[..end];
        assert!(
            !body.contains("tokio::join!"),
            "parallel Claude/Codex auth accept races the global generation high-water"
        );
    }

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

    #[tokio::test]
    async fn stock_path_claude_and_codex_probes_are_not_check_failed() {
        let registry = crate::providers::startup::stock_provider_registry()
            .expect("stock Claude and Codex adapters register");
        let discovery = crate::providers::registry::ProviderDiscoveryConfig::default();
        for kind in [ProviderKind::Codex, ProviderKind::ClaudeCode] {
            let started = std::time::Instant::now();
            match registry.observe(kind, &discovery).await {
                Ok(_) => {}
                Err(ProviderError::MissingCli { .. }) => continue,
                Err(error) => panic!(
                    "{kind:?} observe failed in {:?}: {error}",
                    started.elapsed()
                ),
            }
            match super::observe_with_trusted_auth(&registry, kind, &discovery).await {
                Ok(_) => {}
                Err(ProviderError::MissingCli { .. }) => {}
                Err(error) => panic!(
                    "{kind:?} trusted-auth failed in {:?}: {error}",
                    started.elapsed()
                ),
            }
        }
        let rows = super::probe_agents(&registry).await;
        for row in &rows {
            assert_ne!(row.presence, AgentPresence::CheckFailed, "{row:?}");
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "attaches to a running isolated debug host"]
    async fn live_isolated_host_agent_connection_query_is_not_check_failed() {
        let profile = crate::ui::native_shell::isolated_dev_profile(env!("CARGO_MANIFEST_DIR"))
            .expect("isolated debug profile");
        let mut client = crate::client::HostClient::connect(profile.host_client_config())
            .await
            .unwrap_or_else(|error| {
                panic!("attach live host {}: {error}", profile.named_profile())
            });
        let outcome = client.query_agent_connection().await;
        match outcome {
            Ok(Ok(snapshot)) => {
                for row in &snapshot.agents {
                    eprintln!("live host agent row: {row:?}");
                    assert_ne!(
                        row.presence,
                        AgentPresence::CheckFailed,
                        "live host returned CheckFailed: {row:?}"
                    );
                }
            }
            Ok(Err(error)) => panic!("live host QueryError: {error:?}"),
            Err(error) => panic!("live host IpcError: {error}"),
        }
    }
}
