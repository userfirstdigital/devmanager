use devmanager::browser::{
    browser_command_channel, BrowserActionTarget, BrowserCommand, BrowserError,
    BrowserInvocationContext, BrowserReplayCoordinator, BrowserReplayExecutionHandle,
    BrowserReplayInstance, BrowserReplayProjection, BrowserReplaySecretError,
    BrowserReplaySecretLease, BrowserReplaySecretStore, BrowserReplaySecretSubmission,
    BrowserReplayStatus, BrowserRisk, BrowserWorkspaceKey,
};
use static_assertions::{assert_impl_all, assert_not_impl_any};
use std::time::Duration;

assert_impl_all!(BrowserReplaySecretError: Copy, Send, Sync, std::fmt::Debug);
assert_impl_all!(BrowserReplaySecretStore: Send, Sync);
assert_impl_all!(BrowserReplaySecretLease: Send, Sync);
assert_impl_all!(BrowserReplaySecretSubmission: Send);

assert_not_impl_any!(
    BrowserReplaySecretSubmission:
        Clone,
        std::fmt::Debug,
        serde::Serialize,
        serde::de::DeserializeOwned
);
assert_not_impl_any!(
    BrowserReplaySecretStore:
        Clone,
        std::fmt::Debug,
        serde::Serialize,
        serde::de::DeserializeOwned
);
assert_not_impl_any!(
    BrowserReplaySecretLease:
        Clone,
        std::fmt::Debug,
        serde::Serialize,
        serde::de::DeserializeOwned
);

#[test]
fn replay_secret_api_keeps_submission_consuming_and_leases_exact_handle_authority() {
    let _: fn(
        &BrowserReplayCoordinator,
        &BrowserReplayInstance,
        BrowserReplaySecretSubmission,
    ) -> Result<BrowserReplayProjection, BrowserReplaySecretError> =
        BrowserReplayCoordinator::submit_secrets;
    let _: for<'a> fn(
        &'a BrowserReplayExecutionHandle,
        &str,
    ) -> Result<BrowserReplaySecretLease, BrowserReplaySecretError> =
        BrowserReplayExecutionHandle::secret_lease;
}

#[test]
fn replay_secret_errors_are_closed_and_have_only_fixed_value_free_messages() {
    let errors = [
        BrowserReplaySecretError::InvalidSubmission,
        BrowserReplaySecretError::AlreadySubmitted,
        BrowserReplaySecretError::StaleAuthority,
        BrowserReplaySecretError::ClosedStore,
        BrowserReplaySecretError::SecretUnavailable,
    ];

    for error in errors {
        let display = error.to_string();
        assert!(display.starts_with("browser replay secret "));
        assert!(!display.contains("value-sentinel"));
        assert!(!format!("{error:?}").contains("value-sentinel"));
        assert!(serde_json::to_string(&display)
            .unwrap()
            .find("value-sentinel")
            .is_none());
    }
}

#[tokio::test]
async fn secure_command_generic_request_has_no_sidecar_and_is_rejected_by_host_validation() {
    let (bridge, mut inbox) = browser_command_channel(1);
    let workspace_key = BrowserWorkspaceKey::new("project-a", "conversation-a").unwrap();
    let controller = bridge.bind(workspace_key, Duration::from_secs(1));
    let command = BrowserCommand::SecretType {
        tab_id: "tab-a".to_string(),
        target: BrowserActionTarget::default(),
        input_name: "password".to_string(),
    };
    let context = BrowserInvocationContext::agent("type replay secret", BrowserRisk::Normal)
        .expect("valid Agent context");
    let request_task =
        tokio::spawn(async move { controller.request_with_context(command, context).await });

    let request = inbox.recv().await.expect("generic marker reaches host");
    assert!(matches!(
        request.validate_secret_sidecar(),
        Err(BrowserError::InvalidInvocation { field }) if field == "secretSidecar"
    ));
    request.respond(Err(BrowserError::InvalidInvocation {
        field: "secretSidecar".to_string(),
    }));
    assert!(matches!(
        request_task.await.unwrap(),
        Err(BrowserError::InvalidInvocation { field }) if field == "secretSidecar"
    ));
}

#[test]
fn secure_command_public_wire_and_debug_surfaces_are_value_free() {
    const SENTINEL: &str = "value-sentinel-secure-command";
    let command = BrowserCommand::SecretType {
        tab_id: "tab-a".to_string(),
        target: BrowserActionTarget::default(),
        input_name: "password".to_string(),
    };
    let context = BrowserInvocationContext::new(
        devmanager::browser::BrowserInvocationActor::Agent,
        "type replay secret",
        BrowserRisk::AccountSecurity,
        "secure-operation",
    )
    .unwrap();
    let status = BrowserReplayStatus::Running;

    let command_json = serde_json::to_value(&command).expect("serialize marker");
    assert_eq!(command_json["type"], "secretType");
    assert_eq!(command_json["inputName"], "password");
    assert!(command_json.get("text").is_none());
    assert!(command_json.get("value").is_none());
    assert_eq!(
        serde_json::from_value::<BrowserCommand>(command_json.clone()).unwrap(),
        command
    );

    for safe_surface in [
        format!("{command:?}"),
        serde_json::to_string(&command_json).unwrap(),
        format!("{context:?}"),
        serde_json::to_string(&context).unwrap(),
        format!("{status:?}"),
        serde_json::to_string(&status).unwrap(),
    ] {
        assert!(!safe_surface.contains(SENTINEL));
    }
}
