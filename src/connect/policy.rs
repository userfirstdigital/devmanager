//! Transport-independent management and privacy policy.
//!
//! This module is deliberately not serializable.  Policy inputs are opaque
//! records issued by the crate-only host bridge after the canonical Connect
//! [`PermissionEvaluator`](super::permission::PermissionEvaluator) has
//! admitted the exact connection, session, client, task, action, and action
//! epoch.  No public constructor can assert an owner, enrollment, consent,
//! managed field, or operation.

use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};

use uuid::Uuid;

use crate::domain::command::{Command, CommandEnvelope};
use crate::domain::id::{ClientId, CommandId, TaskId};

use super::envelope::{ConnectionId, SessionId};
use super::permission::{ActionId, ConnectRole, PermissionEvaluator, PermissionRequest};

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
///
/// The enum is descriptive only.  A value becomes policy input only after a
/// private host-reducer provenance record is created; `PolicyOperation` does
/// not expose a field-taking constructor or variant.
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

/// Validated task policy context.  It contains no task facts or event data.
///
/// The fields and all constructors are private.  A context can only be
/// issued by `HostPolicyBridge::issue_task_context` after the trusted host
/// reducer has supplied the canonical enrollment, privacy class, and task
/// generation.
///
/// ```compile_fail
/// use devmanager::connect::{ManagementPrivacyClass, TaskContext};
/// use devmanager::domain::id::TaskId;
///
/// let _ = TaskContext::enrolled(TaskId::new(), ManagementPrivacyClass::ManagedMetadata, true);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TaskContext {
    task_id: TaskId,
    task_generation: u64,
    resource_generation: u64,
    privacy_class: ManagementPrivacyClass,
    enrollment: TaskEnrollment,
}

impl TaskContext {
    #[allow(dead_code)]
    fn from_host_facts(facts: CanonicalTaskFacts) -> Self {
        Self {
            task_id: facts.task_id,
            task_generation: facts.task_generation,
            resource_generation: facts.resource_generation,
            privacy_class: facts.privacy_class,
            enrollment: facts.enrollment,
        }
    }

    pub const fn task_id(self) -> TaskId {
        self.task_id
    }

    pub const fn task_generation(self) -> u64 {
        self.task_generation
    }

    pub const fn resource_generation(self) -> u64 {
        self.resource_generation
    }

    pub const fn privacy_class(self) -> ManagementPrivacyClass {
        self.privacy_class
    }

    pub const fn enrollment(self) -> TaskEnrollment {
        self.enrollment
    }
}

/// Facts produced by a validated host reducer.  This type intentionally has
/// no public constructor; external identity/tenant/membership issuance is a
/// later gate and cannot be replaced by caller-supplied booleans.
#[derive(Debug, Clone, Copy)]
#[allow(dead_code)]
struct CanonicalTaskFacts {
    task_id: TaskId,
    task_generation: u64,
    resource_generation: u64,
    privacy_class: ManagementPrivacyClass,
    enrollment: TaskEnrollment,
}

#[allow(dead_code)]
impl CanonicalTaskFacts {
    fn is_valid(self) -> bool {
        matches!(
            (self.privacy_class, self.enrollment),
            (
                ManagementPrivacyClass::PersonalLocalOnly,
                TaskEnrollment::PersonalNotEnrolled
            ) | (
                ManagementPrivacyClass::PersonalLocalOnly,
                TaskEnrollment::EnrolledWithoutConsent
            ) | (
                ManagementPrivacyClass::PersonalLocalOnly,
                TaskEnrollment::EnrolledWithConsent
            ) | (
                ManagementPrivacyClass::ManagedMetadata,
                TaskEnrollment::EnrolledWithoutConsent
            ) | (
                ManagementPrivacyClass::ManagedMetadata,
                TaskEnrollment::EnrolledWithConsent
            ) | (
                ManagementPrivacyClass::PublishedOrganization,
                TaskEnrollment::EnrolledWithoutConsent
            ) | (
                ManagementPrivacyClass::PublishedOrganization,
                TaskEnrollment::EnrolledWithConsent
            ) | (
                ManagementPrivacyClass::RawContent,
                TaskEnrollment::EnrolledWithConsent
            ) | (_, TaskEnrollment::Unmanaged)
        )
    }
}

/// Connection/session/client/task/action identity carried by every opaque
/// policy authority.  The values are private so an external caller cannot
/// forge a matching authority by struct literal or constructor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct AuthorityBinding {
    connection_id: ConnectionId,
    session_id: SessionId,
    client_id: ClientId,
    task_id: TaskId,
    action: ActionId,
    action_epoch: u64,
}

impl AuthorityBinding {
    fn same_connection(self, other: Self) -> bool {
        self.connection_id == other.connection_id
    }

    fn same_session(self, other: Self) -> bool {
        self.session_id == other.session_id
    }

    fn same_client(self, other: Self) -> bool {
        self.client_id == other.client_id
    }

    fn same_task(self, other: Self) -> bool {
        self.task_id == other.task_id
    }

    fn same_action(self, other: Self) -> bool {
        self.action == other.action
    }

    fn same_epoch(self, other: Self) -> bool {
        self.action_epoch == other.action_epoch
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
enum CanonicalRole {
    Owner,
    ManagerWatcher,
    TaskCollaborator,
}

#[allow(dead_code)]
impl CanonicalRole {
    fn connect_role(self, task_id: TaskId) -> ConnectRole {
        match self {
            Self::Owner => ConnectRole::PairedOwner,
            Self::ManagerWatcher => ConnectRole::Watcher { task_id },
            Self::TaskCollaborator => ConnectRole::Collaborator { task_id },
        }
    }

    fn grant_role(self) -> Option<ManagementRole> {
        match self {
            Self::Owner => None,
            Self::ManagerWatcher => Some(ManagementRole::ManagerWatcher),
            Self::TaskCollaborator => Some(ManagementRole::TaskCollaborator),
        }
    }
}

/// A grant can only be issued by the host bridge after canonical permission
/// evaluation.  Its id and nonce are intentionally opaque and never exposed
/// as caller-controlled values.
///
/// ```compile_fail
/// use devmanager::connect::PolicyPrincipal;
///
/// let _ = PolicyPrincipal::Owner;
/// ```
///
/// ```compile_fail
/// use devmanager::connect::{ManagementGrant, ManagementRole};
/// use devmanager::domain::id::TaskId;
///
/// let _ = ManagementGrant::try_new(TaskId::new(), ManagementRole::TaskCollaborator, 0, 1);
/// ```
#[derive(PartialEq, Eq)]
pub struct ManagementGrant {
    id: GrantId,
    nonce: GrantNonce,
    binding: AuthorityBinding,
    role: ManagementRole,
    issued_at_ms: u64,
    expires_at_ms: u64,
    state: GrantState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct GrantId(Uuid);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct GrantNonce([u8; 16]);

#[derive(Debug)]
struct GrantState {
    revoked: AtomicBool,
    consumed: AtomicBool,
}

impl PartialEq for GrantState {
    fn eq(&self, other: &Self) -> bool {
        self.revoked.load(Ordering::Acquire) == other.revoked.load(Ordering::Acquire)
            && self.consumed.load(Ordering::Acquire) == other.consumed.load(Ordering::Acquire)
    }
}

impl Eq for GrantState {}

impl fmt::Debug for ManagementGrant {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ManagementGrant")
            .field("task_id", &self.binding.task_id)
            .field("role", &self.role)
            .field("issued_at_ms", &self.issued_at_ms)
            .field("expires_at_ms", &self.expires_at_ms)
            .field("revoked", &self.is_revoked())
            .field("consumed", &self.is_consumed())
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GrantError {
    ExpiryNotAfterIssue,
    ZeroActionEpoch,
}

impl ManagementGrant {
    /// Grant construction is private until signed identity, tenant,
    /// membership, task link, policy revision, and persisted revocation are
    /// supplied by a later issuance phase.
    #[allow(dead_code)]
    fn try_new(
        binding: AuthorityBinding,
        role: ManagementRole,
        issued_at_ms: u64,
        expires_at_ms: u64,
    ) -> Result<Self, GrantError> {
        if binding.action_epoch == 0 {
            return Err(GrantError::ZeroActionEpoch);
        }
        if expires_at_ms <= issued_at_ms {
            return Err(GrantError::ExpiryNotAfterIssue);
        }
        let id = Uuid::now_v7();
        let nonce = Uuid::now_v7().into_bytes();
        Ok(Self {
            id: GrantId(id),
            nonce: GrantNonce(nonce),
            binding,
            role,
            issued_at_ms,
            expires_at_ms,
            state: GrantState {
                revoked: AtomicBool::new(false),
                consumed: AtomicBool::new(false),
            },
        })
    }

    /// Revocation is monotonic.  A holder cannot create or extend a grant;
    /// this method only removes authority from an already-issued one.
    pub fn revoke(&self) {
        self.state.revoked.store(true, Ordering::Release);
    }

    pub const fn task_id(&self) -> TaskId {
        self.binding.task_id
    }

    pub const fn role(&self) -> ManagementRole {
        self.role
    }

    pub fn is_revoked(&self) -> bool {
        self.state.revoked.load(Ordering::Acquire)
    }

    pub fn is_consumed(&self) -> bool {
        self.state.consumed.load(Ordering::Acquire)
    }

    pub const fn issued_at_ms(&self) -> u64 {
        self.issued_at_ms
    }

    pub const fn expires_at_ms(&self) -> u64 {
        self.expires_at_ms
    }

    fn binding(&self) -> AuthorityBinding {
        self.binding
    }

    fn claim_once(&self) -> bool {
        !self.state.consumed.swap(true, Ordering::AcqRel)
    }

    #[allow(dead_code)]
    fn id_for_tests(&self) -> GrantId {
        self.id
    }

    #[allow(dead_code)]
    fn nonce_for_tests(&self) -> GrantNonce {
        self.nonce
    }
}

/// Management roles are intentionally narrower than owner authority.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ManagementRole {
    ManagerWatcher,
    TaskCollaborator,
}

/// Opaque principal authority.  The former public `Owner`, `Grant`, and
/// `NoGrant` enum variants were removed: callers cannot construct a principal
/// or choose an identity outside the trusted host bridge.
#[derive(Debug, PartialEq, Eq)]
pub struct PolicyPrincipal<'grant> {
    authority: PrincipalAuthority<'grant>,
}

#[derive(Debug, PartialEq, Eq)]
#[allow(dead_code)]
enum PrincipalAuthority<'grant> {
    Owner(AuthorityBinding),
    Grant(&'grant ManagementGrant),
    NoGrant,
}

impl<'grant> PolicyPrincipal<'grant> {
    #[allow(dead_code)]
    fn owner(binding: AuthorityBinding) -> Self {
        Self {
            authority: PrincipalAuthority::Owner(binding),
        }
    }

    #[allow(dead_code)]
    fn grant(grant: &'grant ManagementGrant) -> Self {
        Self {
            authority: PrincipalAuthority::Grant(grant),
        }
    }

    #[allow(dead_code)]
    fn no_grant() -> Self {
        Self {
            authority: PrincipalAuthority::NoGrant,
        }
    }
}

/// Canonical policy operation.  It has no public variants and carries only
/// sealed provenance evidence produced by a validated command or host
/// reducer.  In particular, callers cannot pass `MutateTask`, `ManagedField`,
/// or `GitSummary` labels directly to policy.
///
/// ```compile_fail
/// use devmanager::connect::{ManagedField, PolicyOperation};
///
/// let _ = PolicyOperation::MutateTask;
/// let _ = PolicyOperation::ReadMetadata(ManagedField::GitSummary);
/// ```
#[derive(Debug, PartialEq, Eq)]
pub struct PolicyOperation {
    evidence: PolicyEvidence,
}

#[derive(Debug, PartialEq, Eq)]
#[allow(dead_code)]
enum PolicyEvidence {
    Metadata(CanonicalMetadataEvidence),
    Mutation(CanonicalActionEvidence),
    Dangerous(CanonicalActionEvidence),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
enum EvidenceProvenance {
    ValidatedCommand {
        command_id: CommandId,
        expected_task_revision: u64,
    },
    HostReducer {
        reducer_revision: u64,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CanonicalActionEvidence {
    binding: AuthorityBinding,
    task_generation: u64,
    resource_generation: u64,
    provenance: EvidenceProvenance,
}

/// A private wrapper produced only after the host command reducer has
/// validated a `CommandEnvelope` against the current task snapshot and
/// revision.  Keeping the wrapper private prevents policy callers from
/// relabelling an unvalidated command as a canonical mutation.
#[derive(Debug, Clone)]
#[allow(dead_code)]
struct HostValidatedCommand {
    envelope: CommandEnvelope,
}

impl HostValidatedCommand {
    #[allow(dead_code)]
    fn from_reducer(envelope: CommandEnvelope) -> Result<Self, HostPolicyError> {
        if envelope.task_id.is_none()
            || envelope.expected_task_revision.is_none()
            || canonical_command_action(&envelope.command).is_none()
        {
            return Err(HostPolicyError::InvalidEvidence);
        }
        Ok(Self { envelope })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CanonicalMetadataEvidence {
    binding: AuthorityBinding,
    task_generation: u64,
    resource_generation: u64,
    field: ManagedField,
    content_class: ManagementPrivacyClass,
    provenance: EvidenceProvenance,
}

impl CanonicalMetadataEvidence {
    fn is_well_formed(self) -> bool {
        let field_class_matches = if self.field.is_allowed() {
            matches!(
                self.content_class,
                ManagementPrivacyClass::ManagedMetadata
                    | ManagementPrivacyClass::PublishedOrganization
            )
        } else if self.field.is_denied_content() {
            self.content_class == ManagementPrivacyClass::RawContent
        } else {
            matches!(
                self.field,
                ManagedField::ProviderQuota
                    | ManagedField::ProviderCost
                    | ManagedField::ProviderEstimate
            ) && matches!(
                self.content_class,
                ManagementPrivacyClass::ManagedMetadata
                    | ManagementPrivacyClass::PublishedOrganization
            )
        };
        field_class_matches
            && !matches!(
                self.provenance,
                EvidenceProvenance::HostReducer {
                    reducer_revision: 0
                }
            )
    }
}

impl CanonicalActionEvidence {
    fn is_well_formed(self) -> bool {
        self.binding.action.known().is_some()
            && !matches!(
                self.provenance,
                EvidenceProvenance::HostReducer {
                    reducer_revision: 0
                }
            )
    }
}

/// Canonical field facts are emitted by a host reducer, not accepted from a
/// management caller.  The reducer chooses both field and content class in
/// one operation, which prevents raw content from being relabelled as safe
/// metadata.
#[derive(Debug, Clone, Copy)]
#[allow(dead_code)]
enum HostMetadataFact {
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

impl HostMetadataFact {
    #[allow(dead_code)]
    fn canonical(self) -> (ManagedField, ManagementPrivacyClass) {
        match self {
            Self::TaskState => (
                ManagedField::TaskState,
                ManagementPrivacyClass::ManagedMetadata,
            ),
            Self::TaskAttention => (
                ManagedField::TaskAttention,
                ManagementPrivacyClass::ManagedMetadata,
            ),
            Self::TaskAssignmentReference => (
                ManagedField::TaskAssignmentReference,
                ManagementPrivacyClass::ManagedMetadata,
            ),
            Self::ProviderKind => (
                ManagedField::ProviderKind,
                ManagementPrivacyClass::ManagedMetadata,
            ),
            Self::ProviderState => (
                ManagedField::ProviderState,
                ManagementPrivacyClass::ManagedMetadata,
            ),
            Self::SourceTimestamp => (
                ManagedField::SourceTimestamp,
                ManagementPrivacyClass::ManagedMetadata,
            ),
            Self::ObservedTimestamp => (
                ManagedField::ObservedTimestamp,
                ManagementPrivacyClass::ManagedMetadata,
            ),
            Self::ProviderReportedUsage => (
                ManagedField::ProviderReportedUsage,
                ManagementPrivacyClass::ManagedMetadata,
            ),
            Self::HumanMessageCount => (
                ManagedField::HumanMessageCount,
                ManagementPrivacyClass::ManagedMetadata,
            ),
            Self::HumanTurnCount => (
                ManagedField::HumanTurnCount,
                ManagementPrivacyClass::ManagedMetadata,
            ),
            Self::ActiveSessionInterval => (
                ManagedField::ActiveSessionInterval,
                ManagementPrivacyClass::ManagedMetadata,
            ),
            Self::GitSummary => (
                ManagedField::GitSummary,
                ManagementPrivacyClass::ManagedMetadata,
            ),
            Self::HostHealth => (
                ManagedField::HostHealth,
                ManagementPrivacyClass::ManagedMetadata,
            ),
            Self::ApprovedArtifactReference => (
                ManagedField::ApprovedArtifactReference,
                ManagementPrivacyClass::ManagedMetadata,
            ),
            Self::ProviderQuota => (
                ManagedField::ProviderQuota,
                ManagementPrivacyClass::ManagedMetadata,
            ),
            Self::ProviderCost => (
                ManagedField::ProviderCost,
                ManagementPrivacyClass::ManagedMetadata,
            ),
            Self::ProviderEstimate => (
                ManagedField::ProviderEstimate,
                ManagementPrivacyClass::ManagedMetadata,
            ),
            Self::Prompt => (ManagedField::Prompt, ManagementPrivacyClass::RawContent),
            Self::Response => (ManagedField::Response, ManagementPrivacyClass::RawContent),
            Self::Terminal => (ManagedField::Terminal, ManagementPrivacyClass::RawContent),
            Self::Browser => (ManagedField::Browser, ManagementPrivacyClass::RawContent),
            Self::Recording => (ManagedField::Recording, ManagementPrivacyClass::RawContent),
            Self::FileBody => (ManagedField::FileBody, ManagementPrivacyClass::RawContent),
            Self::FullDiff => (ManagedField::FullDiff, ManagementPrivacyClass::RawContent),
            Self::Credentials => (
                ManagedField::Credentials,
                ManagementPrivacyClass::RawContent,
            ),
            Self::EnvironmentValue => (
                ManagedField::EnvironmentValue,
                ManagementPrivacyClass::RawContent,
            ),
            Self::Unknown => (
                ManagedField::Unknown,
                ManagementPrivacyClass::ManagedMetadata,
            ),
        }
    }
}

/// A typed error used only inside the host bridge.  Public policy callers see
/// fixed `PolicyReasonCode` values and never receive task or identity data.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
enum HostPolicyError {
    UntrustedHost,
    PermissionDenied,
    TaskMismatch,
    ClientMismatch,
    ActionMismatch,
    TaskGenerationMismatch,
    InvalidEvidence,
    InvalidGrant,
}

/// A host admission record is private by construction.  The `signed_membership`
/// bit stands in for the deferred signed identity/tenant/membership verifier;
/// no caller-facing API can set it.
#[derive(Debug, Clone, Copy)]
#[allow(dead_code)]
struct HostAdmission {
    binding: AuthorityBinding,
    role: CanonicalRole,
    task_generation: u64,
    resource_generation: u64,
    signed_membership: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
struct CurrentHostAuthority {
    binding: AuthorityBinding,
    role: CanonicalRole,
    task_generation: u64,
    resource_generation: u64,
}

/// Crate-only bridge from validated host state to opaque policy authorities.
///
/// Production construction of `HostAdmission` is intentionally deferred until
/// signed external identity, tenant, and membership issuance exists.  Until
/// then `admit` fails closed for every unsigned record.  The existing
/// canonical `PermissionEvaluator` is always consulted before an authority is
/// issued; this module never accepts a caller-provided `PolicyPrincipal`,
/// `TaskContext`, field, consent, or operation label.
#[derive(Debug, Clone, Copy)]
#[allow(dead_code)]
pub(crate) struct HostPolicyBridge {
    evaluator: PermissionEvaluator,
}

#[allow(dead_code)]
impl HostPolicyBridge {
    pub(crate) const fn new(evaluator: PermissionEvaluator) -> Self {
        Self { evaluator }
    }

    fn admit(&self, admission: HostAdmission) -> Result<CurrentHostAuthority, HostPolicyError> {
        if !admission.signed_membership {
            return Err(HostPolicyError::UntrustedHost);
        }
        let request = PermissionRequest {
            role: admission.role.connect_role(admission.binding.task_id),
            task_id: Some(admission.binding.task_id),
            action: admission.binding.action,
        };
        if !self.evaluator.authorize(request) {
            return Err(HostPolicyError::PermissionDenied);
        }
        Ok(CurrentHostAuthority {
            binding: admission.binding,
            role: admission.role,
            task_generation: admission.task_generation,
            resource_generation: admission.resource_generation,
        })
    }

    fn issue_task_context(
        &self,
        authority: CurrentHostAuthority,
        facts: CanonicalTaskFacts,
    ) -> Result<TaskContext, HostPolicyError> {
        if facts.task_id != authority.binding.task_id
            || facts.task_generation != authority.task_generation
            || facts.resource_generation != authority.resource_generation
        {
            return Err(HostPolicyError::TaskGenerationMismatch);
        }
        if !facts.is_valid() {
            return Err(HostPolicyError::InvalidEvidence);
        }
        Ok(TaskContext::from_host_facts(facts))
    }

    fn issue_owner_principal(
        &self,
        authority: CurrentHostAuthority,
    ) -> Result<PolicyPrincipal<'static>, HostPolicyError> {
        if authority.role != CanonicalRole::Owner {
            return Err(HostPolicyError::PermissionDenied);
        }
        Ok(PolicyPrincipal::owner(authority.binding))
    }

    fn issue_grant(
        &self,
        authority: CurrentHostAuthority,
        issued_at_ms: u64,
        expires_at_ms: u64,
    ) -> Result<ManagementGrant, HostPolicyError> {
        let role = authority
            .role
            .grant_role()
            .ok_or(HostPolicyError::PermissionDenied)?;
        ManagementGrant::try_new(authority.binding, role, issued_at_ms, expires_at_ms)
            .map_err(|_| HostPolicyError::InvalidGrant)
    }

    fn issue_grant_principal<'grant>(
        &self,
        authority: CurrentHostAuthority,
        grant: &'grant ManagementGrant,
    ) -> Result<PolicyPrincipal<'grant>, HostPolicyError> {
        if authority.binding != grant.binding() || authority.role.grant_role() != Some(grant.role) {
            return Err(HostPolicyError::PermissionDenied);
        }
        Ok(PolicyPrincipal::grant(grant))
    }

    fn issue_metadata_operation(
        &self,
        authority: CurrentHostAuthority,
        fact: HostMetadataFact,
        reducer_revision: u64,
    ) -> Result<PolicyOperation, HostPolicyError> {
        if authority.binding.action != ActionId::READ_TASK {
            return Err(HostPolicyError::ActionMismatch);
        }
        let (field, content_class) = fact.canonical();
        let evidence = CanonicalMetadataEvidence {
            binding: authority.binding,
            task_generation: authority.task_generation,
            resource_generation: authority.resource_generation,
            field,
            content_class,
            provenance: EvidenceProvenance::HostReducer { reducer_revision },
        };
        if !evidence.is_well_formed() {
            return Err(HostPolicyError::InvalidEvidence);
        }
        Ok(PolicyOperation {
            evidence: PolicyEvidence::Metadata(evidence),
        })
    }

    fn issue_command_operation(
        &self,
        authority: CurrentHostAuthority,
        validated: &HostValidatedCommand,
    ) -> Result<PolicyOperation, HostPolicyError> {
        let envelope = &validated.envelope;
        if envelope.client_id != authority.binding.client_id {
            return Err(HostPolicyError::ClientMismatch);
        }
        if envelope.task_id != Some(authority.binding.task_id) {
            return Err(HostPolicyError::TaskMismatch);
        }
        let action =
            canonical_command_action(&envelope.command).ok_or(HostPolicyError::ActionMismatch)?;
        if action != authority.binding.action {
            return Err(HostPolicyError::ActionMismatch);
        }
        let expected_task_revision = envelope
            .expected_task_revision
            .ok_or(HostPolicyError::InvalidEvidence)?;
        let evidence = CanonicalActionEvidence {
            binding: authority.binding,
            task_generation: authority.task_generation,
            resource_generation: authority.resource_generation,
            provenance: EvidenceProvenance::ValidatedCommand {
                command_id: envelope.command_id,
                expected_task_revision,
            },
        };
        if !evidence.is_well_formed() {
            return Err(HostPolicyError::InvalidEvidence);
        }
        Ok(PolicyOperation {
            evidence: PolicyEvidence::Mutation(evidence),
        })
    }

    fn issue_dangerous_operation(
        &self,
        authority: CurrentHostAuthority,
        reducer_revision: u64,
    ) -> Result<PolicyOperation, HostPolicyError> {
        if authority.binding.action != ActionId::APPROVE_DANGEROUS {
            return Err(HostPolicyError::ActionMismatch);
        }
        let evidence = CanonicalActionEvidence {
            binding: authority.binding,
            task_generation: authority.task_generation,
            resource_generation: authority.resource_generation,
            provenance: EvidenceProvenance::HostReducer { reducer_revision },
        };
        if !evidence.is_well_formed() {
            return Err(HostPolicyError::InvalidEvidence);
        }
        Ok(PolicyOperation {
            evidence: PolicyEvidence::Dangerous(evidence),
        })
    }
}

fn canonical_command_action(command: &Command) -> Option<ActionId> {
    match command {
        Command::RenameTask(_)
        | Command::SetTaskAttention(_)
        | Command::BeginCloseTask
        | Command::ReopenTask
        | Command::RegisterAgentSession { .. }
        | Command::SetPrimaryAgent { .. }
        | Command::RegisterArtifact { .. }
        | Command::RegisterResource { .. }
        | Command::ReleaseResource { .. } => Some(ActionId::MUTATE_TASK),
        Command::CreateTask(_) | Command::ConfirmHostQuit(_) => None,
    }
}

/// Stable, non-secret reason codes.  No decision carries caller, task,
/// field, provider, or other arbitrary detail.
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
    GrantReplayed,
    GrantConnectionMismatch,
    GrantSessionMismatch,
    GrantClientMismatch,
    GrantTaskMismatch,
    GrantActionMismatch,
    GrantActionEpochMismatch,
    TaskGenerationMismatch,
    ResourceGenerationMismatch,
    WatcherReadOnly,
    OwnerOnlyDangerousApproval,
    MutationDenied,
    RawContentDisabled,
    DeniedMetadataField,
    DeniedContentClass,
    UnknownMetadataField,
    InvalidEvidence,
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
        Self::GrantReplayed,
        Self::GrantConnectionMismatch,
        Self::GrantSessionMismatch,
        Self::GrantClientMismatch,
        Self::GrantTaskMismatch,
        Self::GrantActionMismatch,
        Self::GrantActionEpochMismatch,
        Self::TaskGenerationMismatch,
        Self::ResourceGenerationMismatch,
        Self::WatcherReadOnly,
        Self::OwnerOnlyDangerousApproval,
        Self::MutationDenied,
        Self::RawContentDisabled,
        Self::DeniedMetadataField,
        Self::DeniedContentClass,
        Self::UnknownMetadataField,
        Self::InvalidEvidence,
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
            Self::GrantReplayed => "grant_replayed",
            Self::GrantConnectionMismatch => "grant_connection_mismatch",
            Self::GrantSessionMismatch => "grant_session_mismatch",
            Self::GrantClientMismatch => "grant_client_mismatch",
            Self::GrantTaskMismatch => "grant_task_mismatch",
            Self::GrantActionMismatch => "grant_action_mismatch",
            Self::GrantActionEpochMismatch => "grant_action_epoch_mismatch",
            Self::TaskGenerationMismatch => "task_generation_mismatch",
            Self::ResourceGenerationMismatch => "resource_generation_mismatch",
            Self::WatcherReadOnly => "watcher_read_only",
            Self::OwnerOnlyDangerousApproval => "owner_only_dangerous_approval",
            Self::MutationDenied => "mutation_denied",
            Self::RawContentDisabled => "raw_content_disabled",
            Self::DeniedMetadataField => "denied_metadata_field",
            Self::DeniedContentClass => "denied_content_class",
            Self::UnknownMetadataField => "unknown_metadata_field",
            Self::InvalidEvidence => "invalid_evidence",
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

/// Stateless policy rules backed by opaque, provenance-carrying authorities.
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

        let (
            operation_binding,
            task_generation,
            resource_generation,
            operation_action,
            operation_decision,
        ) = match operation.evidence {
            PolicyEvidence::Metadata(evidence) => {
                if !evidence.is_well_formed() {
                    return PolicyDecision::deny(PolicyReasonCode::InvalidEvidence);
                }
                (
                    evidence.binding,
                    evidence.task_generation,
                    evidence.resource_generation,
                    evidence.binding.action,
                    self.decide_metadata(task, &principal, evidence, now_ms),
                )
            }
            PolicyEvidence::Mutation(evidence) => {
                if !evidence.is_well_formed() {
                    return PolicyDecision::deny(PolicyReasonCode::InvalidEvidence);
                }
                (
                    evidence.binding,
                    evidence.task_generation,
                    evidence.resource_generation,
                    evidence.binding.action,
                    self.decide_mutation(task, &principal, evidence, now_ms),
                )
            }
            PolicyEvidence::Dangerous(evidence) => {
                if !evidence.is_well_formed() {
                    return PolicyDecision::deny(PolicyReasonCode::InvalidEvidence);
                }
                (
                    evidence.binding,
                    evidence.task_generation,
                    evidence.resource_generation,
                    evidence.binding.action,
                    self.decide_dangerous(task, &principal, evidence),
                )
            }
        };

        if operation_binding.task_id != task.task_id {
            return PolicyDecision::deny(PolicyReasonCode::GrantTaskMismatch);
        }
        if task_generation != task.task_generation {
            return PolicyDecision::deny(PolicyReasonCode::TaskGenerationMismatch);
        }
        if resource_generation != task.resource_generation {
            return PolicyDecision::deny(PolicyReasonCode::ResourceGenerationMismatch);
        }
        if operation_binding.action != operation_action {
            return PolicyDecision::deny(PolicyReasonCode::GrantActionMismatch);
        }
        if !operation_decision.is_allowed() {
            return operation_decision;
        }

        // Every allowed non-owner operation consumes the opaque grant.  A
        // second use of the same id/nonce is a replay, even if all labels are
        // otherwise identical.
        if let PrincipalAuthority::Grant(grant) = principal.authority {
            if !grant.claim_once() {
                return PolicyDecision::deny(PolicyReasonCode::GrantReplayed);
            }
        }
        PolicyDecision::allow()
    }

    fn decide_metadata(
        &self,
        task: &TaskContext,
        principal: &PolicyPrincipal<'_>,
        evidence: CanonicalMetadataEvidence,
        now_ms: u64,
    ) -> PolicyDecision {
        if evidence.binding.action != ActionId::READ_TASK {
            return PolicyDecision::deny(PolicyReasonCode::GrantActionMismatch);
        }
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

        if evidence.content_class == ManagementPrivacyClass::RawContent {
            return PolicyDecision::deny(PolicyReasonCode::DeniedContentClass);
        }
        if evidence.field.is_unknown() {
            return PolicyDecision::deny(PolicyReasonCode::UnknownMetadataField);
        }
        if !evidence.field.is_allowed() {
            return PolicyDecision::deny(if evidence.field.is_denied_content() {
                PolicyReasonCode::DeniedContentClass
            } else {
                PolicyReasonCode::DeniedMetadataField
            });
        }

        self.authorize(principal, evidence.binding, now_ms, false)
    }

    fn decide_mutation(
        &self,
        task: &TaskContext,
        principal: &PolicyPrincipal<'_>,
        evidence: CanonicalActionEvidence,
        now_ms: u64,
    ) -> PolicyDecision {
        if evidence.binding.action != ActionId::MUTATE_TASK {
            return PolicyDecision::deny(PolicyReasonCode::GrantActionMismatch);
        }
        match principal.authority {
            PrincipalAuthority::Owner(binding) => self.authorize_owner(binding, evidence.binding),
            PrincipalAuthority::NoGrant => PolicyDecision::deny(PolicyReasonCode::GrantMissing),
            PrincipalAuthority::Grant(grant) => {
                if let Some(denial) = self.validate_grant(task, grant, evidence.binding, now_ms) {
                    return denial;
                }
                match grant.role {
                    ManagementRole::ManagerWatcher => {
                        PolicyDecision::deny(PolicyReasonCode::WatcherReadOnly)
                    }
                    ManagementRole::TaskCollaborator => PolicyDecision::allow(),
                }
            }
        }
    }

    fn decide_dangerous(
        &self,
        _task: &TaskContext,
        principal: &PolicyPrincipal<'_>,
        evidence: CanonicalActionEvidence,
    ) -> PolicyDecision {
        if evidence.binding.action != ActionId::APPROVE_DANGEROUS {
            return PolicyDecision::deny(PolicyReasonCode::GrantActionMismatch);
        }
        match principal.authority {
            PrincipalAuthority::Owner(binding) => self.authorize_owner(binding, evidence.binding),
            PrincipalAuthority::Grant(_) | PrincipalAuthority::NoGrant => {
                PolicyDecision::deny(PolicyReasonCode::OwnerOnlyDangerousApproval)
            }
        }
    }

    fn authorize(
        &self,
        principal: &PolicyPrincipal<'_>,
        binding: AuthorityBinding,
        now_ms: u64,
        mutation: bool,
    ) -> PolicyDecision {
        match principal.authority {
            PrincipalAuthority::Owner(principal_binding) => {
                self.authorize_owner(principal_binding, binding)
            }
            PrincipalAuthority::NoGrant => PolicyDecision::deny(PolicyReasonCode::GrantMissing),
            PrincipalAuthority::Grant(grant) => {
                if let Some(denial) = self.validate_grant_from_binding(grant, binding, now_ms) {
                    return denial;
                }
                if mutation && grant.role == ManagementRole::ManagerWatcher {
                    return PolicyDecision::deny(PolicyReasonCode::WatcherReadOnly);
                }
                PolicyDecision::allow()
            }
        }
    }

    fn authorize_owner(
        &self,
        principal_binding: AuthorityBinding,
        operation_binding: AuthorityBinding,
    ) -> PolicyDecision {
        self.binding_decision(principal_binding, operation_binding)
    }

    fn validate_grant(
        &self,
        task: &TaskContext,
        grant: &ManagementGrant,
        operation_binding: AuthorityBinding,
        now_ms: u64,
    ) -> Option<PolicyDecision> {
        if let Some(denial) = self.validate_grant_from_binding(grant, operation_binding, now_ms) {
            return Some(denial);
        }
        if grant.task_id() != task.task_id {
            return Some(PolicyDecision::deny(PolicyReasonCode::GrantTaskMismatch));
        }
        None
    }

    fn validate_grant_from_binding(
        &self,
        grant: &ManagementGrant,
        operation_binding: AuthorityBinding,
        now_ms: u64,
    ) -> Option<PolicyDecision> {
        let grant_binding = grant.binding();
        if !grant_binding.same_connection(operation_binding) {
            return Some(PolicyDecision::deny(
                PolicyReasonCode::GrantConnectionMismatch,
            ));
        }
        if !grant_binding.same_session(operation_binding) {
            return Some(PolicyDecision::deny(PolicyReasonCode::GrantSessionMismatch));
        }
        if !grant_binding.same_client(operation_binding) {
            return Some(PolicyDecision::deny(PolicyReasonCode::GrantClientMismatch));
        }
        if !grant_binding.same_task(operation_binding) {
            return Some(PolicyDecision::deny(PolicyReasonCode::GrantTaskMismatch));
        }
        if !grant_binding.same_action(operation_binding) {
            return Some(PolicyDecision::deny(PolicyReasonCode::GrantActionMismatch));
        }
        if !grant_binding.same_epoch(operation_binding) {
            return Some(PolicyDecision::deny(
                PolicyReasonCode::GrantActionEpochMismatch,
            ));
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
        if grant.is_consumed() {
            return Some(PolicyDecision::deny(PolicyReasonCode::GrantReplayed));
        }
        None
    }

    fn binding_decision(
        &self,
        principal: AuthorityBinding,
        operation: AuthorityBinding,
    ) -> PolicyDecision {
        if !principal.same_connection(operation) {
            return PolicyDecision::deny(PolicyReasonCode::GrantConnectionMismatch);
        }
        if !principal.same_session(operation) {
            return PolicyDecision::deny(PolicyReasonCode::GrantSessionMismatch);
        }
        if !principal.same_client(operation) {
            return PolicyDecision::deny(PolicyReasonCode::GrantClientMismatch);
        }
        if !principal.same_task(operation) {
            return PolicyDecision::deny(PolicyReasonCode::GrantTaskMismatch);
        }
        if !principal.same_action(operation) {
            return PolicyDecision::deny(PolicyReasonCode::GrantActionMismatch);
        }
        if !principal.same_epoch(operation) {
            return PolicyDecision::deny(PolicyReasonCode::GrantActionEpochMismatch);
        }
        PolicyDecision::allow()
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

    fn connect_id(tail: u8) -> ConnectionId {
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
        ConnectionId::from_bytes(bytes).expect("connection id")
    }

    fn session_id(tail: u8) -> SessionId {
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
        SessionId::from_bytes(bytes).expect("session id")
    }

    fn client_id(tail: u8) -> ClientId {
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
        ClientId::from_bytes(bytes).expect("client id")
    }

    fn binding(tail: u8, action: ActionId, epoch: u64) -> AuthorityBinding {
        AuthorityBinding {
            connection_id: connect_id(tail),
            session_id: session_id(tail),
            client_id: client_id(tail),
            task_id: task_id(tail),
            action,
            action_epoch: epoch,
        }
    }

    fn admission(tail: u8, action: ActionId, role: CanonicalRole) -> HostAdmission {
        HostAdmission {
            binding: binding(tail, action, 1),
            role,
            task_generation: 3,
            resource_generation: 5,
            signed_membership: true,
        }
    }

    fn task_for(authority: CurrentHostAuthority, privacy: ManagementPrivacyClass) -> TaskContext {
        HostPolicyBridge::new(PermissionEvaluator::default())
            .issue_task_context(
                authority,
                CanonicalTaskFacts {
                    task_id: task_id(authority.binding.task_id.as_bytes()[15]),
                    task_generation: authority.task_generation,
                    resource_generation: authority.resource_generation,
                    privacy_class: privacy,
                    enrollment: if privacy == ManagementPrivacyClass::PersonalLocalOnly {
                        TaskEnrollment::EnrolledWithConsent
                    } else {
                        TaskEnrollment::EnrolledWithoutConsent
                    },
                },
            )
            .expect("task context")
    }

    fn admitted(
        tail: u8,
        action: ActionId,
        role: CanonicalRole,
    ) -> (HostPolicyBridge, CurrentHostAuthority) {
        let bridge = HostPolicyBridge::new(PermissionEvaluator::default());
        let authority = bridge
            .admit(admission(tail, action, role))
            .expect("admitted");
        (bridge, authority)
    }

    #[test]
    fn unsigned_host_bridge_fails_closed() {
        let bridge = HostPolicyBridge::new(PermissionEvaluator::default());
        let mut admission = admission(1, ActionId::READ_TASK, CanonicalRole::ManagerWatcher);
        admission.signed_membership = false;
        assert_eq!(bridge.admit(admission), Err(HostPolicyError::UntrustedHost));
    }

    #[test]
    fn canonical_command_evidence_binds_exact_action_and_generations() {
        let (bridge, authority) = admitted(2, ActionId::MUTATE_TASK, CanonicalRole::Owner);
        let envelope = CommandEnvelope {
            command_id: CommandId::new(),
            client_id: authority.binding.client_id,
            task_id: Some(authority.binding.task_id),
            issued_at_ms: 50,
            expected_task_revision: Some(4),
            command: Command::BeginCloseTask,
        };
        let validated = HostValidatedCommand { envelope };
        let operation = bridge
            .issue_command_operation(authority, &validated)
            .expect("canonical operation");
        let task = task_for(authority, ManagementPrivacyClass::ManagedMetadata);
        let principal = bridge.issue_owner_principal(authority).expect("owner");
        assert!(ManagementPolicy::default()
            .decide(&task, principal, operation, 50)
            .is_allowed());
    }

    #[test]
    fn dangerous_approval_is_owner_only_and_missing_principal_denies() {
        let (bridge, authority) = admitted(7, ActionId::APPROVE_DANGEROUS, CanonicalRole::Owner);
        let task = task_for(authority, ManagementPrivacyClass::ManagedMetadata);
        let operation = bridge
            .issue_dangerous_operation(authority, 1)
            .expect("dangerous operation");
        let owner = bridge.issue_owner_principal(authority).expect("owner");
        assert!(ManagementPolicy::default()
            .decide(&task, owner, operation, 50)
            .is_allowed());

        let (bridge, authority) = admitted(8, ActionId::READ_TASK, CanonicalRole::ManagerWatcher);
        let task = task_for(authority, ManagementPrivacyClass::ManagedMetadata);
        let operation = bridge
            .issue_metadata_operation(authority, HostMetadataFact::TaskState, 1)
            .expect("operation");
        let missing =
            ManagementPolicy::default().decide(&task, PolicyPrincipal::no_grant(), operation, 50);
        assert_eq!(missing.reason_code(), PolicyReasonCode::GrantMissing);
    }

    #[test]
    fn raw_content_cannot_be_relabelled_as_git_metadata() {
        let (bridge, authority) = admitted(3, ActionId::READ_TASK, CanonicalRole::Owner);
        let task = task_for(authority, ManagementPrivacyClass::ManagedMetadata);
        let principal = bridge.issue_owner_principal(authority).expect("owner");
        let mut evidence = match bridge
            .issue_metadata_operation(authority, HostMetadataFact::Response, 1)
            .expect("raw operation")
            .evidence
        {
            PolicyEvidence::Metadata(evidence) => evidence,
            _ => unreachable!(),
        };
        evidence.field = ManagedField::GitSummary;
        let decision = ManagementPolicy::default().decide(
            &task,
            principal,
            PolicyOperation {
                evidence: PolicyEvidence::Metadata(evidence),
            },
            50,
        );
        assert_eq!(decision.reason_code(), PolicyReasonCode::InvalidEvidence);
        assert!(!decision.is_allowed());
    }

    #[test]
    fn foreign_session_and_stale_generation_are_denied() {
        let (bridge, authority) = admitted(4, ActionId::READ_TASK, CanonicalRole::Owner);
        let task = task_for(authority, ManagementPrivacyClass::ManagedMetadata);
        let principal = bridge.issue_owner_principal(authority).expect("owner");
        let operation = bridge
            .issue_metadata_operation(authority, HostMetadataFact::TaskState, 1)
            .expect("operation");
        let mut foreign = match operation.evidence {
            PolicyEvidence::Metadata(evidence) => evidence,
            _ => unreachable!(),
        };
        foreign.binding.session_id = session_id(44);
        let foreign_decision = ManagementPolicy::default().decide(
            &task,
            principal,
            PolicyOperation {
                evidence: PolicyEvidence::Metadata(foreign),
            },
            50,
        );
        assert_eq!(
            foreign_decision.reason_code(),
            PolicyReasonCode::GrantSessionMismatch
        );

        let operation = bridge
            .issue_metadata_operation(authority, HostMetadataFact::TaskState, 1)
            .expect("operation");
        let mut stale = match operation.evidence {
            PolicyEvidence::Metadata(evidence) => evidence,
            _ => unreachable!(),
        };
        stale.task_generation += 1;
        let stale_decision = ManagementPolicy::default().decide(
            &task,
            bridge.issue_owner_principal(authority).expect("owner"),
            PolicyOperation {
                evidence: PolicyEvidence::Metadata(stale),
            },
            50,
        );
        assert_eq!(
            stale_decision.reason_code(),
            PolicyReasonCode::TaskGenerationMismatch
        );

        let operation = bridge
            .issue_metadata_operation(authority, HostMetadataFact::TaskState, 1)
            .expect("operation");
        let mut stale_resource = match operation.evidence {
            PolicyEvidence::Metadata(evidence) => evidence,
            _ => unreachable!(),
        };
        stale_resource.resource_generation += 1;
        let stale_resource_decision = ManagementPolicy::default().decide(
            &task,
            bridge.issue_owner_principal(authority).expect("owner"),
            PolicyOperation {
                evidence: PolicyEvidence::Metadata(stale_resource),
            },
            50,
        );
        assert_eq!(
            stale_resource_decision.reason_code(),
            PolicyReasonCode::ResourceGenerationMismatch
        );
    }

    #[test]
    fn grants_bind_to_session_and_are_one_shot() {
        let (bridge, authority) = admitted(5, ActionId::READ_TASK, CanonicalRole::TaskCollaborator);
        let task = task_for(authority, ManagementPrivacyClass::ManagedMetadata);
        let grant = bridge.issue_grant(authority, 10, 100).expect("grant");
        let principal = bridge
            .issue_grant_principal(authority, &grant)
            .expect("principal");
        let operation = bridge
            .issue_metadata_operation(authority, HostMetadataFact::TaskState, 1)
            .expect("operation");
        let policy = ManagementPolicy::default();
        assert!(policy.decide(&task, principal, operation, 50).is_allowed());

        let replay_operation = bridge
            .issue_metadata_operation(authority, HostMetadataFact::TaskState, 1)
            .expect("operation");
        let replay = policy.decide(
            &task,
            bridge
                .issue_grant_principal(authority, &grant)
                .expect("principal"),
            replay_operation,
            50,
        );
        assert_eq!(replay.reason_code(), PolicyReasonCode::GrantReplayed);

        let mut wrong_session_binding = authority.binding;
        wrong_session_binding.session_id = session_id(55);
        let wrong_session_operation = PolicyOperation {
            evidence: PolicyEvidence::Metadata(CanonicalMetadataEvidence {
                binding: wrong_session_binding,
                task_generation: authority.task_generation,
                resource_generation: authority.resource_generation,
                field: ManagedField::TaskState,
                content_class: ManagementPrivacyClass::ManagedMetadata,
                provenance: EvidenceProvenance::HostReducer {
                    reducer_revision: 1,
                },
            }),
        };
        let wrong_session = policy.decide(
            &task,
            bridge
                .issue_grant_principal(authority, &grant)
                .expect("principal"),
            wrong_session_operation,
            50,
        );
        assert_eq!(
            wrong_session.reason_code(),
            PolicyReasonCode::GrantSessionMismatch
        );
    }

    #[test]
    fn grant_expiry_and_revocation_remain_denials() {
        let (bridge, authority) = admitted(6, ActionId::READ_TASK, CanonicalRole::ManagerWatcher);
        let task = task_for(authority, ManagementPrivacyClass::ManagedMetadata);
        let operation = bridge
            .issue_metadata_operation(authority, HostMetadataFact::TaskState, 1)
            .expect("operation");
        let grant = bridge.issue_grant(authority, 10, 100).expect("grant");
        grant.revoke();
        assert!(grant.is_revoked());
        let decision = ManagementPolicy::default().decide(
            &task,
            bridge
                .issue_grant_principal(authority, &grant)
                .expect("principal"),
            operation,
            50,
        );
        assert_eq!(decision.reason_code(), PolicyReasonCode::GrantRevoked);

        let expired = bridge.issue_grant(authority, 10, 100).expect("grant");
        let expired_operation = bridge
            .issue_metadata_operation(authority, HostMetadataFact::TaskState, 1)
            .expect("operation");
        let decision = ManagementPolicy::default().decide(
            &task,
            bridge
                .issue_grant_principal(authority, &expired)
                .expect("principal"),
            expired_operation,
            100,
        );
        assert_eq!(decision.reason_code(), PolicyReasonCode::GrantStale);
    }

    #[test]
    fn managed_field_sets_and_intervals_remain_closed() {
        assert!(ManagedField::GitSummary.is_allowed());
        assert!(ManagedField::Response.is_denied_content());
        assert!(
            ActiveSessionInterval::try_new(1_000, 1_000 + ACTIVE_SESSION_IDLE_LIMIT_MS).is_ok()
        );
        assert!(
            ActiveSessionInterval::try_new(1_000, 1_001 + ACTIVE_SESSION_IDLE_LIMIT_MS).is_err()
        );
    }
}
