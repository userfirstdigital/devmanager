use crate::domain::{
    AgentConnectionRow, AgentPresence, ConfigSidebarProviderKind,
};
use crate::providers::{ProviderAuthState, ProviderError, ProviderObservation};

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
        ProviderAuthState::AuthRequired => AgentPresence::NotSignedIn,
        ProviderAuthState::Unknown => AgentPresence::CheckFailed,
    }
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
            AgentPresence::CheckFailed
        );
    }
}
