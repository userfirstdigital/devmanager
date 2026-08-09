//! Transport-independent management and privacy policy.
//!
//! This module is deliberately not serializable. It is the closed authority
//! that later transport and projection layers must consult before they expose
//! or mutate anything.

use crate::domain::id::TaskId;

/// An active interval is split at this idle boundary before it is exported.
pub const ACTIVE_SESSION_IDLE_LIMIT_MS: u64 = 15 * 60 * 1_000;

/// Privacy classes are policy inputs, not wire or storage representations.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ManagementPrivacyClass {
    PersonalLocalOnly,
    ManagedMetadata,
    PublishedOrganization,
    RawContent,
}

/// Readable alias for callers that name these values as content classes.
pub type ContentClass = ManagementPrivacyClass;

/// Closed set of fields that may be considered for management export.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ManagedField {
    TaskState,
    TaskAttention,
    TaskAssignmentReference,
    ProviderKind,
    ProviderState,
    SourceTimestamp,
    ObservedTimestamp,
    ProviderReportedUsage,
    HumanMessageCount,
    HumanTurnCount,
    ActiveSessionInterval,
    GitSummary,
    HostHealth,
    ApprovedArtifactReference,
    ProviderQuota,
    ProviderCost,
    ProviderEstimate,
    Prompt,
    Response,
    Terminal,
    Browser,
    Recording,
    FileBody,
    FullDiff,
    Credentials,
    EnvironmentValue,
    Unknown,
}

impl ManagedField {
    /// The only fields that are eligible for managed metadata export.
    pub const ALLOWLIST: &'static [Self] = &[
        Self::TaskState,
        Self::TaskAttention,
        Self::TaskAssignmentReference,
        Self::ProviderKind,
        Self::ProviderState,
        Self::SourceTimestamp,
        Self::ObservedTimestamp,
        Self::ProviderReportedUsage,
        Self::HumanMessageCount,
        Self::HumanTurnCount,
        Self::ActiveSessionInterval,
        Self::GitSummary,
        Self::HostHealth,
        Self::ApprovedArtifactReference,
    ];

    /// Explicitly denied fields remain named so future projections cannot
    /// accidentally treat them as unknown-but-possibly-safe metadata.
    pub const DENYLIST: &'static [Self] = &[
        Self::ProviderQuota,
        Self::ProviderCost,
        Self::ProviderEstimate,
        Self::Prompt,
        Self::Response,
        Self::Terminal,
        Self::Browser,
        Self::Recording,
        Self::FileBody,
        Self::FullDiff,
        Self::Credentials,
        Self::EnvironmentValue,
    ];

    pub const fn is_allowed(self) -> bool {
        matches!(
            self,
            Self::TaskState
                | Self::TaskAttention
                | Self::TaskAssignmentReference
                | Self::ProviderKind
                | Self::ProviderState
                | Self::SourceTimestamp
                | Self::ObservedTimestamp
                | Self::ProviderReportedUsage
                | Self::HumanMessageCount
                | Self::HumanTurnCount
                | Self::ActiveSessionInterval
                | Self::GitSummary
                | Self::HostHealth
                | Self::ApprovedArtifactReference
        )
    }

    pub const fn is_explicitly_denied(self) -> bool {
        matches!(
            self,
            Self::ProviderQuota
                | Self::ProviderCost
                | Self::ProviderEstimate
                | Self::Prompt
                | Self::Response
                | Self::Terminal
                | Self::Browser
                | Self::Recording
                | Self::FileBody
                | Self::FullDiff
                | Self::Credentials
                | Self::EnvironmentValue
        )
    }

    pub const fn is_denied_content(self) -> bool {
        matches!(
            self,
            Self::Prompt
                | Self::Response
                | Self::Terminal
                | Self::Browser
                | Self::Recording
                | Self::FileBody
                | Self::FullDiff
                | Self::Credentials
                | Self::EnvironmentValue
        )
    }

    pub const fn is_unknown(self) -> bool {
        matches!(self, Self::Unknown)
    }
}

/// Explicitly denied raw/sensitive content classes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DeniedContentClass {
    Prompt,
    Response,
    Terminal,
    Browser,
    Recording,
    FileBody,
    FullDiff,
    Credentials,
    EnvironmentValue,
}

impl DeniedContentClass {
    pub const ALL: &'static [Self] = &[
        Self::Prompt,
        Self::Response,
        Self::Terminal,
        Self::Browser,
        Self::Recording,
        Self::FileBody,
        Self::FullDiff,
        Self::Credentials,
        Self::EnvironmentValue,
    ];
}

/// Enrollment state is deliberately closed so a caller cannot construct a
/// personal export state without naming both enrollment and consent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TaskEnrollment {
    Unmanaged,
    PersonalNotEnrolled,
    EnrolledWithoutConsent,
    EnrolledWithConsent,
}

/// Validated task policy context. It contains no task facts or event data.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TaskContext {
    task_id: TaskId,
    privacy_class: ManagementPrivacyClass,
    enrollment: TaskEnrollment,
}

impl TaskContext {
    pub const fn unmanaged(task_id: TaskId, privacy_class: ManagementPrivacyClass) -> Self {
        Self {
            task_id,
            privacy_class,
            enrollment: TaskEnrollment::Unmanaged,
        }
    }

    pub const fn personal_not_enrolled(task_id: TaskId) -> Self {
        Self {
            task_id,
            privacy_class: ManagementPrivacyClass::PersonalLocalOnly,
            enrollment: TaskEnrollment::PersonalNotEnrolled,
        }
    }

    pub const fn personal_without_consent(task_id: TaskId) -> Self {
        Self {
            task_id,
            privacy_class: ManagementPrivacyClass::PersonalLocalOnly,
            enrollment: TaskEnrollment::EnrolledWithoutConsent,
        }
    }

    pub const fn personal_with_consent(task_id: TaskId) -> Self {
        Self {
            task_id,
            privacy_class: ManagementPrivacyClass::PersonalLocalOnly,
            enrollment: TaskEnrollment::EnrolledWithConsent,
        }
    }

    pub const fn enrolled(
        task_id: TaskId,
        privacy_class: ManagementPrivacyClass,
        consent: bool,
    ) -> Self {
        Self {
            task_id,
            privacy_class,
            enrollment: if consent {
                TaskEnrollment::EnrolledWithConsent
            } else {
                TaskEnrollment::EnrolledWithoutConsent
            },
        }
    }

    pub const fn task_id(self) -> TaskId {
        self.task_id
    }

    pub const fn privacy_class(self) -> ManagementPrivacyClass {
        self.privacy_class
    }

    pub const fn enrollment(self) -> TaskEnrollment {
        self.enrollment
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GrantError {
    ExpiryNotAfterIssue,
}

/// A time-bounded, task-scoped management grant.
///
/// Grant construction is private to this policy module until Phase 10.7 binds
/// signed grants to account/device identity, membership, tenant, task link,
/// policy revision, expiry, and allowed classes. External callers cannot use
/// the constructor:
///
/// ```compile_fail
/// use devmanager::connect::{ManagementGrant, ManagementRole};
/// use devmanager::domain::id::TaskId;
///
/// let task_id = TaskId::from_bytes([
///     0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0x70, 0xcd,
///     0x80, 0xef, 0, 0, 0, 0, 0, 1,
/// ]).expect("valid task id");
/// let _grant = ManagementGrant::try_new(task_id, ManagementRole::TaskCollaborator, 0, 1);
/// ```
///
/// They also cannot bypass the constructor with a struct literal:
///
/// ```compile_fail
/// use devmanager::connect::{ManagementGrant, ManagementRole};
/// use devmanager::domain::id::TaskId;
///
/// let task_id = TaskId::from_bytes([
///     0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0x70, 0xcd,
///     0x80, 0xef, 0, 0, 0, 0, 0, 1,
/// ]).expect("valid task id");
/// let _grant = ManagementGrant {
///     task_id,
///     role: ManagementRole::TaskCollaborator,
///     issued_at_ms: 0,
///     expires_at_ms: 1,
///     revoked: false,
/// };
/// ```
#[derive(Debug, PartialEq, Eq, Hash)]
pub struct ManagementGrant {
    task_id: TaskId,
    role: ManagementRole,
    issued_at_ms: u64,
    expires_at_ms: u64,
    revoked: bool,
}

impl ManagementGrant {
    #[allow(dead_code)]
    fn try_new(
        task_id: TaskId,
        role: ManagementRole,
        issued_at_ms: u64,
        expires_at_ms: u64,
    ) -> Result<Self, GrantError> {
        if expires_at_ms <= issued_at_ms {
            return Err(GrantError::ExpiryNotAfterIssue);
        }
        Ok(Self {
            task_id,
            role,
            issued_at_ms,
            expires_at_ms,
            revoked: false,
        })
    }

    pub fn revoke(&mut self) {
        self.revoked = true;
    }

    pub const fn task_id(&self) -> TaskId {
        self.task_id
    }

    pub const fn role(&self) -> ManagementRole {
        self.role
    }

    pub const fn is_revoked(&self) -> bool {
        self.revoked
    }

    pub const fn issued_at_ms(&self) -> u64 {
        self.issued_at_ms
    }

    pub const fn expires_at_ms(&self) -> u64 {
        self.expires_at_ms
    }
}

/// Management roles are intentionally narrower than owner authority.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ManagementRole {
    ManagerWatcher,
    TaskCollaborator,
}

#[derive(Debug, PartialEq, Eq)]
pub enum PolicyPrincipal<'grant> {
    Owner,
    Grant(&'grant ManagementGrant),
    NoGrant,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PolicyOperation {
    ReadMetadata(ManagedField),
    MutateTask,
    ApproveDangerous,
}

/// Stable, non-secret reason codes. No decision carries caller, task, field,
/// provider, or other arbitrary detail.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PolicyReasonCode {
    Allowed,
    UnmanagedTask,
    PersonalTaskNotEnrolled,
    PersonalTaskConsentRequired,
    GrantMissing,
    GrantNotYetValid,
    GrantStale,
    GrantRevoked,
    GrantTaskMismatch,
    WatcherReadOnly,
    OwnerOnlyDangerousApproval,
    MutationDenied,
    RawContentDisabled,
    DeniedMetadataField,
    DeniedContentClass,
    UnknownMetadataField,
}

impl PolicyReasonCode {
    pub const ALL: &'static [Self] = &[
        Self::Allowed,
        Self::UnmanagedTask,
        Self::PersonalTaskNotEnrolled,
        Self::PersonalTaskConsentRequired,
        Self::GrantMissing,
        Self::GrantNotYetValid,
        Self::GrantStale,
        Self::GrantRevoked,
        Self::GrantTaskMismatch,
        Self::WatcherReadOnly,
        Self::OwnerOnlyDangerousApproval,
        Self::MutationDenied,
        Self::RawContentDisabled,
        Self::DeniedMetadataField,
        Self::DeniedContentClass,
        Self::UnknownMetadataField,
    ];

    pub const fn code(self) -> &'static str {
        match self {
            Self::Allowed => "allowed",
            Self::UnmanagedTask => "unmanaged_task",
            Self::PersonalTaskNotEnrolled => "personal_task_not_enrolled",
            Self::PersonalTaskConsentRequired => "personal_task_consent_required",
            Self::GrantMissing => "grant_missing",
            Self::GrantNotYetValid => "grant_not_yet_valid",
            Self::GrantStale => "grant_stale",
            Self::GrantRevoked => "grant_revoked",
            Self::GrantTaskMismatch => "grant_task_mismatch",
            Self::WatcherReadOnly => "watcher_read_only",
            Self::OwnerOnlyDangerousApproval => "owner_only_dangerous_approval",
            Self::MutationDenied => "mutation_denied",
            Self::RawContentDisabled => "raw_content_disabled",
            Self::DeniedMetadataField => "denied_metadata_field",
            Self::DeniedContentClass => "denied_content_class",
            Self::UnknownMetadataField => "unknown_metadata_field",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PolicyDecision {
    allowed: bool,
    reason_code: PolicyReasonCode,
}

impl PolicyDecision {
    const fn allow() -> Self {
        Self {
            allowed: true,
            reason_code: PolicyReasonCode::Allowed,
        }
    }

    const fn deny(reason_code: PolicyReasonCode) -> Self {
        Self {
            allowed: false,
            reason_code,
        }
    }

    pub const fn is_allowed(self) -> bool {
        self.allowed
    }

    pub const fn reason_code(self) -> PolicyReasonCode {
        self.reason_code
    }

    pub const fn code(self) -> &'static str {
        self.reason_code.code()
    }
}

/// Stateless authority for management export and task-scoped actions.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ManagementPolicy;

impl ManagementPolicy {
    pub const fn new() -> Self {
        Self
    }

    pub fn decide(
        &self,
        task: &TaskContext,
        principal: PolicyPrincipal<'_>,
        operation: PolicyOperation,
        now_ms: u64,
    ) -> PolicyDecision {
        if task.enrollment == TaskEnrollment::Unmanaged {
            return PolicyDecision::deny(PolicyReasonCode::UnmanagedTask);
        }

        match operation {
            PolicyOperation::ReadMetadata(field) => {
                self.decide_metadata(task, principal, field, now_ms)
            }
            PolicyOperation::MutateTask => self.decide_mutation(task, principal, now_ms),
            PolicyOperation::ApproveDangerous => self.decide_dangerous(principal),
        }
    }

    fn decide_metadata(
        &self,
        task: &TaskContext,
        principal: PolicyPrincipal<'_>,
        field: ManagedField,
        now_ms: u64,
    ) -> PolicyDecision {
        match task.privacy_class {
            ManagementPrivacyClass::RawContent => {
                return PolicyDecision::deny(PolicyReasonCode::RawContentDisabled)
            }
            ManagementPrivacyClass::PersonalLocalOnly => match task.enrollment {
                TaskEnrollment::PersonalNotEnrolled => {
                    return PolicyDecision::deny(PolicyReasonCode::PersonalTaskNotEnrolled)
                }
                TaskEnrollment::EnrolledWithoutConsent => {
                    return PolicyDecision::deny(PolicyReasonCode::PersonalTaskConsentRequired)
                }
                _ => {}
            },
            ManagementPrivacyClass::ManagedMetadata
            | ManagementPrivacyClass::PublishedOrganization => {}
        }

        if field.is_unknown() {
            return PolicyDecision::deny(PolicyReasonCode::UnknownMetadataField);
        }
        if !field.is_allowed() {
            return PolicyDecision::deny(if field.is_denied_content() {
                PolicyReasonCode::DeniedContentClass
            } else {
                PolicyReasonCode::DeniedMetadataField
            });
        }

        self.authorize_read(task, principal, now_ms)
    }

    fn decide_mutation(
        &self,
        task: &TaskContext,
        principal: PolicyPrincipal<'_>,
        now_ms: u64,
    ) -> PolicyDecision {
        match principal {
            PolicyPrincipal::Owner => PolicyDecision::allow(),
            PolicyPrincipal::NoGrant => PolicyDecision::deny(PolicyReasonCode::GrantMissing),
            PolicyPrincipal::Grant(grant) => {
                if let Some(denial) = self.validate_grant(task, grant, now_ms) {
                    return denial;
                }
                match grant.role() {
                    ManagementRole::ManagerWatcher => {
                        PolicyDecision::deny(PolicyReasonCode::WatcherReadOnly)
                    }
                    ManagementRole::TaskCollaborator => PolicyDecision::allow(),
                }
            }
        }
    }

    fn decide_dangerous(&self, principal: PolicyPrincipal<'_>) -> PolicyDecision {
        match principal {
            PolicyPrincipal::Owner => PolicyDecision::allow(),
            PolicyPrincipal::Grant(_) | PolicyPrincipal::NoGrant => {
                PolicyDecision::deny(PolicyReasonCode::OwnerOnlyDangerousApproval)
            }
        }
    }

    fn authorize_read(
        &self,
        task: &TaskContext,
        principal: PolicyPrincipal<'_>,
        now_ms: u64,
    ) -> PolicyDecision {
        match principal {
            PolicyPrincipal::Owner => PolicyDecision::allow(),
            PolicyPrincipal::NoGrant => PolicyDecision::deny(PolicyReasonCode::GrantMissing),
            PolicyPrincipal::Grant(grant) => self
                .validate_grant(task, grant, now_ms)
                .unwrap_or_else(PolicyDecision::allow),
        }
    }

    fn validate_grant(
        &self,
        task: &TaskContext,
        grant: &ManagementGrant,
        now_ms: u64,
    ) -> Option<PolicyDecision> {
        if grant.task_id() != task.task_id {
            return Some(PolicyDecision::deny(PolicyReasonCode::GrantTaskMismatch));
        }
        if grant.is_revoked() {
            return Some(PolicyDecision::deny(PolicyReasonCode::GrantRevoked));
        }
        if now_ms < grant.issued_at_ms() {
            return Some(PolicyDecision::deny(PolicyReasonCode::GrantNotYetValid));
        }
        if now_ms >= grant.expires_at_ms() {
            return Some(PolicyDecision::deny(PolicyReasonCode::GrantStale));
        }
        None
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActiveSessionIntervalError {
    EndBeforeStart,
    ExceedsIdleLimit,
}

/// Validated interval whose duration cannot exceed the documented idle cap.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ActiveSessionInterval {
    started_at_ms: u64,
    ended_at_ms: u64,
}

impl ActiveSessionInterval {
    pub fn try_new(
        started_at_ms: u64,
        ended_at_ms: u64,
    ) -> Result<Self, ActiveSessionIntervalError> {
        if ended_at_ms < started_at_ms {
            return Err(ActiveSessionIntervalError::EndBeforeStart);
        }
        if ended_at_ms - started_at_ms > ACTIVE_SESSION_IDLE_LIMIT_MS {
            return Err(ActiveSessionIntervalError::ExceedsIdleLimit);
        }
        Ok(Self {
            started_at_ms,
            ended_at_ms,
        })
    }

    pub const fn started_at_ms(self) -> u64 {
        self.started_at_ms
    }

    pub const fn ended_at_ms(self) -> u64 {
        self.ended_at_ms
    }
}

/// Alias retained for callers that use metadata terminology.
pub type MetadataField = ManagedField;

/// Explicit policy name that does not collide with the Phase 9 wire alias.
pub type PolicyPrivacyClass = ManagementPrivacyClass;

/// Alias for the policy authority's role-neutral name.
pub type PolicyAuthority = ManagementPolicy;

#[cfg(test)]
mod tests {
    use super::*;
    use static_assertions::assert_not_impl_any;
    use uuid::Uuid;

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
    fn private_grants_preserve_expiry_and_live_revocation_behavior() {
        let mut grant =
            ManagementGrant::try_new(task_id(1), ManagementRole::TaskCollaborator, 10, 100)
                .expect("valid grant");

        grant.revoke();

        assert!(grant.is_revoked());
        assert_eq!(
            ManagementGrant::try_new(task_id(1), ManagementRole::TaskCollaborator, 100, 100),
            Err(GrantError::ExpiryNotAfterIssue)
        );
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
}
