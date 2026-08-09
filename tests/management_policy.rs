use static_assertions::assert_not_impl_any;
use uuid::Uuid;

use devmanager::connect::{
    ActiveSessionInterval, DeniedContentClass, ManagedField, ManagementGrant, ManagementPolicy,
    ManagementPrivacyClass, ManagementRole, PolicyOperation, PolicyPrincipal, PolicyReasonCode,
    TaskContext, ACTIVE_SESSION_IDLE_LIMIT_MS,
};
use devmanager::domain::id::TaskId;

assert_not_impl_any!(ManagementGrant: Clone);
assert_not_impl_any!(ManagementGrant: Copy);

fn task_id(tail: u8) -> TaskId {
    let mut bytes = [0_u8; 16];
    bytes[0] = 0x01;
    bytes[1] = 0x23;
    bytes[2] = 0x45;
    bytes[3] = 0x67;
    bytes[4] = 0x89;
    bytes[5] = 0xab;
    bytes[6] = 0x70;
    bytes[7] = 0xcd;
    bytes[8] = 0x80;
    bytes[9] = 0xef;
    bytes[15] = tail;
    TaskId::from_bytes(Uuid::from_bytes(bytes).into_bytes()).expect("task id")
}

fn enrolled_task(tail: u8) -> TaskContext {
    TaskContext::enrolled(
        task_id(tail),
        ManagementPrivacyClass::ManagedMetadata,
        false,
    )
}

fn grant(tail: u8, role: ManagementRole) -> ManagementGrant {
    ManagementGrant::try_new(task_id(tail), role, 10, 100).expect("valid grant")
}

fn read(field: ManagedField) -> PolicyOperation {
    PolicyOperation::ReadMetadata(field)
}

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
fn raw_content_is_off_by_default_and_unknown_fields_deny() {
    let policy = ManagementPolicy::default();
    let raw_task = TaskContext::enrolled(task_id(1), ManagementPrivacyClass::RawContent, true);
    let raw = policy.decide(
        &raw_task,
        PolicyPrincipal::Owner,
        read(ManagedField::TaskState),
        50,
    );
    assert_eq!(raw.reason_code(), PolicyReasonCode::RawContentDisabled);
    assert!(!raw.is_allowed());

    let unknown = policy.decide(
        &enrolled_task(2),
        PolicyPrincipal::Owner,
        read(ManagedField::Unknown),
        50,
    );
    assert_eq!(
        unknown.reason_code(),
        PolicyReasonCode::UnknownMetadataField
    );
    assert!(!unknown.is_allowed());
}

#[test]
fn personal_tasks_export_zero_metadata_until_enrollment_and_consent() {
    let policy = ManagementPolicy::default();
    let task = task_id(3);

    let not_enrolled = TaskContext::personal_not_enrolled(task);
    assert_eq!(
        policy
            .decide(
                &not_enrolled,
                PolicyPrincipal::Owner,
                read(ManagedField::TaskState),
                50,
            )
            .reason_code(),
        PolicyReasonCode::PersonalTaskNotEnrolled
    );

    let no_consent = TaskContext::personal_without_consent(task);
    assert_eq!(
        policy
            .decide(
                &no_consent,
                PolicyPrincipal::Owner,
                read(ManagedField::TaskState),
                50,
            )
            .reason_code(),
        PolicyReasonCode::PersonalTaskConsentRequired
    );

    let consented = TaskContext::personal_with_consent(task);
    assert_eq!(
        policy
            .decide(
                &consented,
                PolicyPrincipal::Owner,
                read(ManagedField::TaskState),
                50,
            )
            .reason_code(),
        PolicyReasonCode::Allowed
    );
}

#[test]
fn unmanaged_tasks_default_deny_even_owner() {
    let task = TaskContext::unmanaged(task_id(4), ManagementPrivacyClass::ManagedMetadata);
    let decision = ManagementPolicy::default().decide(
        &task,
        PolicyPrincipal::Owner,
        PolicyOperation::ApproveDangerous,
        50,
    );
    assert_eq!(decision.reason_code(), PolicyReasonCode::UnmanagedTask);
    assert!(!decision.is_allowed());
}

#[test]
fn missing_stale_revoked_and_wrong_task_grants_have_fixed_denials() {
    let policy = ManagementPolicy::default();
    let task = enrolled_task(5);
    let operation = read(ManagedField::TaskState);

    let not_yet_valid =
        ManagementGrant::try_new(task_id(5), ManagementRole::ManagerWatcher, 60, 100)
            .expect("grant");
    let stale = grant(5, ManagementRole::ManagerWatcher);
    let mut revoked = grant(5, ManagementRole::ManagerWatcher);
    revoked.revoke();
    let wrong_scope = grant(6, ManagementRole::ManagerWatcher);

    assert_eq!(
        policy
            .decide(&task, PolicyPrincipal::NoGrant, operation, 50)
            .reason_code(),
        PolicyReasonCode::GrantMissing
    );
    assert_eq!(
        policy
            .decide(&task, PolicyPrincipal::Grant(&not_yet_valid), operation, 50,)
            .reason_code(),
        PolicyReasonCode::GrantNotYetValid
    );
    assert_eq!(
        policy
            .decide(&task, PolicyPrincipal::Grant(&stale), operation, 100,)
            .reason_code(),
        PolicyReasonCode::GrantStale
    );
    assert_eq!(
        policy
            .decide(&task, PolicyPrincipal::Grant(&revoked), operation, 50,)
            .reason_code(),
        PolicyReasonCode::GrantRevoked
    );
    assert_eq!(
        policy
            .decide(&task, PolicyPrincipal::Grant(&wrong_scope), operation, 50,)
            .reason_code(),
        PolicyReasonCode::GrantTaskMismatch
    );
}

#[test]
fn watcher_is_read_only_and_collaborator_mutation_is_task_scoped() {
    let policy = ManagementPolicy::default();
    let task = enrolled_task(7);
    let watcher_grant = grant(7, ManagementRole::ManagerWatcher);
    let collaborator_grant = grant(7, ManagementRole::TaskCollaborator);
    let wrong_scope_grant = grant(7, ManagementRole::TaskCollaborator);

    let watcher = policy.decide(
        &task,
        PolicyPrincipal::Grant(&watcher_grant),
        PolicyOperation::MutateTask,
        50,
    );
    assert_eq!(watcher.reason_code(), PolicyReasonCode::WatcherReadOnly);
    assert!(!watcher.is_allowed());

    let collaborator = policy.decide(
        &task,
        PolicyPrincipal::Grant(&collaborator_grant),
        PolicyOperation::MutateTask,
        50,
    );
    assert!(collaborator.is_allowed());
    assert_eq!(collaborator.reason_code(), PolicyReasonCode::Allowed);

    let wrong_scope = policy.decide(
        &TaskContext::enrolled(task_id(8), ManagementPrivacyClass::ManagedMetadata, false),
        PolicyPrincipal::Grant(&wrong_scope_grant),
        PolicyOperation::MutateTask,
        50,
    );
    assert_eq!(
        wrong_scope.reason_code(),
        PolicyReasonCode::GrantTaskMismatch
    );
    assert!(!wrong_scope.is_allowed());
}

#[test]
fn dangerous_approval_is_owner_only() {
    let policy = ManagementPolicy::default();
    let task = enrolled_task(9);

    let owner_decision = policy.decide(
        &task,
        PolicyPrincipal::Owner,
        PolicyOperation::ApproveDangerous,
        50,
    );
    assert!(owner_decision.is_allowed());
    assert_eq!(owner_decision.reason_code(), PolicyReasonCode::Allowed);
    for role in [
        ManagementRole::ManagerWatcher,
        ManagementRole::TaskCollaborator,
    ] {
        let grant = grant(9, role);
        let decision = policy.decide(
            &task,
            PolicyPrincipal::Grant(&grant),
            PolicyOperation::ApproveDangerous,
            50,
        );
        assert_eq!(
            decision.reason_code(),
            PolicyReasonCode::OwnerOnlyDangerousApproval
        );
        assert!(!decision.is_allowed());
    }
}

#[test]
fn provider_usage_is_not_quota_cost_or_estimate() {
    let policy = ManagementPolicy::default();
    let task = enrolled_task(10);
    let watcher_grant = grant(10, ManagementRole::ManagerWatcher);

    assert!(policy
        .decide(
            &task,
            PolicyPrincipal::Grant(&watcher_grant),
            read(ManagedField::ProviderReportedUsage),
            50
        )
        .is_allowed());
    for field in [
        ManagedField::ProviderQuota,
        ManagedField::ProviderCost,
        ManagedField::ProviderEstimate,
    ] {
        assert_eq!(
            policy
                .decide(
                    &task,
                    PolicyPrincipal::Grant(&watcher_grant),
                    read(field),
                    50
                )
                .reason_code(),
            PolicyReasonCode::DeniedMetadataField
        );
    }
}

#[test]
fn revoking_the_same_grant_denies_the_borrowed_principal_immediately() {
    let policy = ManagementPolicy::default();
    let task = enrolled_task(11);
    let mut grant = grant(11, ManagementRole::TaskCollaborator);

    assert!(policy
        .decide(
            &task,
            PolicyPrincipal::Grant(&grant),
            PolicyOperation::MutateTask,
            50,
        )
        .is_allowed());

    grant.revoke();

    let decision = policy.decide(
        &task,
        PolicyPrincipal::Grant(&grant),
        PolicyOperation::MutateTask,
        50,
    );
    assert!(!decision.is_allowed());
    assert_eq!(decision.reason_code(), PolicyReasonCode::GrantRevoked);
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
        (PolicyReasonCode::GrantTaskMismatch, "grant_task_mismatch"),
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
    ];
    assert_eq!(PolicyReasonCode::ALL, expected.map(|(reason, _)| reason));
    for (reason, code) in expected {
        assert_eq!(reason.code(), code);
        assert!(
            !reason.code().contains(':'),
            "reason codes carry no details"
        );
    }
}
