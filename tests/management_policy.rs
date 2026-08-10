use static_assertions::assert_not_impl_any;

use devmanager::connect::{
    ActiveSessionInterval, DeniedContentClass, ManagedField, ManagementGrant, PolicyReasonCode,
    ACTIVE_SESSION_IDLE_LIMIT_MS,
};

assert_not_impl_any!(ManagementGrant: Clone);
assert_not_impl_any!(ManagementGrant: Copy);

#[test]
fn managed_field_allowlist_and_explicit_denylists_are_exhaustive() {
    let allowed = [
        ManagedField::TaskState,
        ManagedField::TaskAttention,
        ManagedField::TaskAssignmentReference,
        ManagedField::ProviderKind,
        ManagedField::ProviderState,
        ManagedField::SourceTimestamp,
        ManagedField::ObservedTimestamp,
        ManagedField::ProviderReportedUsage,
        ManagedField::HumanMessageCount,
        ManagedField::HumanTurnCount,
        ManagedField::ActiveSessionInterval,
        ManagedField::GitSummary,
        ManagedField::HostHealth,
        ManagedField::ApprovedArtifactReference,
    ];
    assert_eq!(ManagedField::ALLOWLIST, allowed);
    for field in allowed {
        assert!(field.is_allowed());
        assert!(!field.is_explicitly_denied());
    }

    let denied_metadata = [
        ManagedField::ProviderQuota,
        ManagedField::ProviderCost,
        ManagedField::ProviderEstimate,
    ];
    let denied_content = [
        ManagedField::Prompt,
        ManagedField::Response,
        ManagedField::Terminal,
        ManagedField::Browser,
        ManagedField::Recording,
        ManagedField::FileBody,
        ManagedField::FullDiff,
        ManagedField::Credentials,
        ManagedField::EnvironmentValue,
    ];
    assert_eq!(
        ManagedField::DENYLIST,
        [
            denied_metadata[0],
            denied_metadata[1],
            denied_metadata[2],
            denied_content[0],
            denied_content[1],
            denied_content[2],
            denied_content[3],
            denied_content[4],
            denied_content[5],
            denied_content[6],
            denied_content[7],
            denied_content[8],
        ]
    );
    for field in denied_metadata.into_iter().chain(denied_content) {
        assert!(!field.is_allowed());
        assert!(field.is_explicitly_denied());
    }
    assert!(!ManagedField::Unknown.is_allowed());
    assert!(!ManagedField::Unknown.is_explicitly_denied());
    assert!(ManagedField::Unknown.is_unknown());

    assert_eq!(DeniedContentClass::ALL.len(), 9);
    assert_eq!(
        DeniedContentClass::ALL,
        &[
            DeniedContentClass::Prompt,
            DeniedContentClass::Response,
            DeniedContentClass::Terminal,
            DeniedContentClass::Browser,
            DeniedContentClass::Recording,
            DeniedContentClass::FileBody,
            DeniedContentClass::FullDiff,
            DeniedContentClass::Credentials,
            DeniedContentClass::EnvironmentValue,
        ]
    );
}

#[test]
fn active_session_interval_accepts_exactly_fifteen_minutes() {
    let end = 1_000 + ACTIVE_SESSION_IDLE_LIMIT_MS;
    assert!(ActiveSessionInterval::try_new(1_000, end).is_ok());
    assert!(ActiveSessionInterval::try_new(1_000, end + 1).is_err());
    assert!(ActiveSessionInterval::try_new(2_000, 1_999).is_err());
}

#[test]
fn fixed_reason_codes_are_stable_and_secret_free() {
    let expected = [
        (PolicyReasonCode::Allowed, "allowed"),
        (PolicyReasonCode::UnmanagedTask, "unmanaged_task"),
        (
            PolicyReasonCode::PersonalTaskNotEnrolled,
            "personal_task_not_enrolled",
        ),
        (
            PolicyReasonCode::PersonalTaskConsentRequired,
            "personal_task_consent_required",
        ),
        (PolicyReasonCode::GrantMissing, "grant_missing"),
        (PolicyReasonCode::GrantNotYetValid, "grant_not_yet_valid"),
        (PolicyReasonCode::GrantStale, "grant_stale"),
        (PolicyReasonCode::GrantRevoked, "grant_revoked"),
        (PolicyReasonCode::GrantReplayed, "grant_replayed"),
        (
            PolicyReasonCode::GrantConnectionMismatch,
            "grant_connection_mismatch",
        ),
        (
            PolicyReasonCode::GrantSessionMismatch,
            "grant_session_mismatch",
        ),
        (
            PolicyReasonCode::GrantClientMismatch,
            "grant_client_mismatch",
        ),
        (PolicyReasonCode::GrantTaskMismatch, "grant_task_mismatch"),
        (
            PolicyReasonCode::GrantActionMismatch,
            "grant_action_mismatch",
        ),
        (
            PolicyReasonCode::GrantActionEpochMismatch,
            "grant_action_epoch_mismatch",
        ),
        (
            PolicyReasonCode::TaskGenerationMismatch,
            "task_generation_mismatch",
        ),
        (
            PolicyReasonCode::ResourceGenerationMismatch,
            "resource_generation_mismatch",
        ),
        (PolicyReasonCode::WatcherReadOnly, "watcher_read_only"),
        (
            PolicyReasonCode::OwnerOnlyDangerousApproval,
            "owner_only_dangerous_approval",
        ),
        (PolicyReasonCode::MutationDenied, "mutation_denied"),
        (PolicyReasonCode::RawContentDisabled, "raw_content_disabled"),
        (
            PolicyReasonCode::DeniedMetadataField,
            "denied_metadata_field",
        ),
        (PolicyReasonCode::DeniedContentClass, "denied_content_class"),
        (
            PolicyReasonCode::UnknownMetadataField,
            "unknown_metadata_field",
        ),
        (PolicyReasonCode::InvalidEvidence, "invalid_evidence"),
    ];
    assert_eq!(PolicyReasonCode::ALL, expected.map(|(reason, _)| reason));
    for (reason, code) in expected {
        assert_eq!(reason.code(), code);
        assert!(!reason.code().contains(':'));
    }
}
