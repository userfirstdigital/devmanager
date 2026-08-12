use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrgError {
    StandaloneMode,
    ConnectSignInDoesNotEnroll,
    EmptyIdentity,
    CrossTenant,
    RoleDenied,
    DisabledMember,
    HostUnenrolled,
    MembershipRevoked,
    EnrollmentNotConfirmed,
    DuplicateLink,
    LinkConflict,
    Unlinked,
    PersonalTask,
    StalePolicy,
    StaleGrant,
    Expired,
    Replay,
    BoundExceeded,
    ProhibitedField,
    ProhibitedLabel,
    ImmutableVersion,
    MissingApproval,
    FingerprintMismatch,
    ProductionRiskNotRetrySafe,
    UncertainOutcome,
    TamperedEvidence,
    UntrustedSigner,
    ReviewRequired,
    LastWriteWinsForbidden,
    AutoLaunchForbidden,
    WatcherReadOnly,
    OwnerOnly,
    Unavailable(OrgDependency),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrgDependency {
    PortalMembershipIssuer,
    PortalBoardCard,
    DurableOutbox,
    DevAgentExport,
    SignedIdentityIssuer,
}

impl fmt::Display for OrgError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::StandaloneMode => {
                write!(f, "organization features are unavailable in anonymous local mode")
            }
            Self::ConnectSignInDoesNotEnroll => {
                write!(f, "Connect sign-in does not enroll hosts or Tasks")
            }
            Self::EmptyIdentity => write!(f, "external identity is empty"),
            Self::CrossTenant => write!(f, "cross-tenant organization access is denied"),
            Self::RoleDenied => write!(f, "membership role is insufficient"),
            Self::DisabledMember => write!(f, "disabled membership cannot act"),
            Self::HostUnenrolled => write!(f, "host is not enrolled"),
            Self::MembershipRevoked => write!(f, "membership or device is revoked"),
            Self::EnrollmentNotConfirmed => {
                write!(f, "local host has not confirmed enrollment")
            }
            Self::DuplicateLink => write!(f, "managed Task link already exists"),
            Self::LinkConflict => write!(f, "managed Task revision conflict remains visible"),
            Self::Unlinked => write!(f, "Task is not linked to a BoardCard"),
            Self::PersonalTask => write!(f, "personal Task is invisible to organization viewers"),
            Self::StalePolicy => write!(f, "organization policy revision is stale"),
            Self::StaleGrant => write!(f, "organization grant is stale or expired"),
            Self::Expired => write!(f, "organization entitlement or cache has expired"),
            Self::Replay => write!(f, "request or observation was already applied"),
            Self::BoundExceeded => write!(f, "organization projection bound exceeded"),
            Self::ProhibitedField => write!(f, "field is excluded from managed metadata"),
            Self::ProhibitedLabel => {
                write!(f, "surveillance, ranking, or payroll labels are forbidden")
            }
            Self::ImmutableVersion => write!(f, "published prompt versions are immutable"),
            Self::MissingApproval => write!(f, "required local approval is missing"),
            Self::FingerprintMismatch => write!(f, "local target fingerprint does not match"),
            Self::ProductionRiskNotRetrySafe => {
                write!(f, "production-risk actions are never assumed retry-safe")
            }
            Self::UncertainOutcome => {
                write!(f, "ambiguous local apply remains uncertain without automatic repeat")
            }
            Self::TamperedEvidence => write!(f, "EvidenceBundle hash or signature does not match"),
            Self::UntrustedSigner => write!(f, "EvidenceBundle signer is untrusted"),
            Self::ReviewRequired => write!(f, "EvidenceBundle must be reviewed before Task creation"),
            Self::LastWriteWinsForbidden => {
                write!(f, "last-write-wins is forbidden for dual-writer fields")
            }
            Self::AutoLaunchForbidden => {
                write!(f, "assignment must not auto-launch a provider")
            }
            Self::WatcherReadOnly => write!(f, "manager Watcher grants are read-only"),
            Self::OwnerOnly => write!(f, "the action is Owner-only"),
            Self::Unavailable(dependency) => write!(f, "organization dependency unavailable: {dependency:?}"),
        }
    }
}

impl std::error::Error for OrgError {}
