use devmanager::browser::{
    BrowserReplayCoordinator, BrowserReplayExecutionHandle, BrowserReplayInstance,
    BrowserReplayProjection, BrowserReplaySecretError, BrowserReplaySecretLease,
    BrowserReplaySecretStore, BrowserReplaySecretSubmission,
};
use static_assertions::{assert_impl_all, assert_not_impl_any};

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
