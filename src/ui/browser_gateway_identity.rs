//! Pure helpers for resolving Browser dock gateway identity without circular
//! dependence on an already-bound cockpit projection.

use crate::browser::{BrowserGatewayRegistrar, BrowserWorkspaceKey};

/// Look up the exact registered process session for a workspace key.
///
/// Never synthesizes an id. Returns None when the registrar has no binding.
pub fn registered_process_session_id(
    registrar: Option<&BrowserGatewayRegistrar>,
    workspace_key: &BrowserWorkspaceKey,
) -> Option<String> {
    registrar.and_then(|registrar| registrar.process_session_id_for_workspace(workspace_key))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::browser::{browser_command_channel, BrowserGatewayHandle, BrowserWorkspaceSnapshot};

    #[test]
    fn identity_requires_registered_session_and_never_synthesizes() {
        let key = BrowserWorkspaceKey::new("task-a", "conversation").expect("key");
        assert_eq!(registered_process_session_id(None, &key), None);

        let (bridge, _inbox) = browser_command_channel(1);
        let gateway = BrowserGatewayHandle::start(bridge).expect("gateway");
        let registrar = gateway.registrar();
        assert_eq!(registered_process_session_id(Some(&registrar), &key), None);

        let registration = registrar
            .register(
                "exact-session",
                key.clone(),
                BrowserWorkspaceSnapshot::default(),
            )
            .expect("register");
        assert_eq!(
            registered_process_session_id(Some(&registrar), &key).as_deref(),
            Some("exact-session")
        );
        assert!(registrar.revoke(&registration));
    }
}
