use crate::git::model::update_os_string_digest;
pub use crate::git::model::GitCapability;
pub(crate) use crate::git::model::MutationPlan;
use crate::git::model::{
    BranchName, CommitPlan, DiffDocument, DiffPlan, PushPlan, RemoteEndpointLease, RemotePolicy,
    RemoteTransport, RepoPath, RepositoryStatus, ReviewPlan, StagePlan, StatusKind, StatusPlan,
    UnstagePlan, WorkspaceIdentity,
};
use crate::git::review::{parse_unified_diff_limited, PullRequestProvider};
use sha2::{Digest, Sha256};
use std::env;
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[cfg(windows)]
use crate::services::platform_service::{
    claim_suspended_process, ManagedProcessJob, MANAGED_PROCESS_CREATION_FLAGS,
};

#[cfg(windows)]
use std::os::windows::process::CommandExt;

#[cfg(unix)]
use std::os::unix::fs::MetadataExt;

#[cfg(windows)]
use std::os::windows::io::{AsRawHandle, FromRawHandle};

#[cfg(windows)]
use std::os::windows::ffi::OsStringExt;

#[cfg(windows)]
use std::os::windows::ffi::OsStrExt;

#[cfg(windows)]
use std::os::windows::fs::MetadataExt;

#[cfg(unix)]
use std::os::unix::io::AsRawFd;

#[cfg(unix)]
use std::os::unix::process::CommandExt;

#[cfg(windows)]
#[link(name = "kernel32")]
extern "system" {
    fn TerminateJobObject(job: *mut std::ffi::c_void, exit_code: u32) -> i32;
    fn OpenProcess(
        desired_access: u32,
        inherit_handle: i32,
        process_id: u32,
    ) -> *mut std::ffi::c_void;
    fn QueryFullProcessImageNameW(
        process: *mut std::ffi::c_void,
        flags: u32,
        image_name: *mut u16,
        size: *mut u32,
    ) -> i32;
    fn CloseHandle(handle: *mut std::ffi::c_void) -> i32;
    fn GetCurrentThreadId() -> u32;
    fn OpenThread(
        desired_access: u32,
        inherit_handle: i32,
        thread_id: u32,
    ) -> *mut std::ffi::c_void;
    fn CancelSynchronousIo(thread: *mut std::ffi::c_void) -> i32;
}

#[cfg(windows)]
const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;
#[cfg(windows)]
const PROCESS_SYNCHRONIZE: u32 = 0x0010_0000;
#[cfg(windows)]
const THREAD_TERMINATE: u32 = 0x0001;

#[cfg(unix)]
extern "C" {
    fn kill(process_group: i32, signal: i32) -> i32;
    fn fchdir(file_descriptor: i32) -> i32;
}

#[cfg(unix)]
const SIGTERM: i32 = 15;
#[cfg(unix)]
const SIGKILL: i32 = 9;
#[cfg(unix)]
const ESRCH: i32 = 3;
#[cfg(unix)]
const F_GETFL: i32 = 3;
#[cfg(unix)]
const F_SETFL: i32 = 4;
#[cfg(unix)]
const O_NONBLOCK: i32 = 0x800;

#[cfg(unix)]
extern "C" {
    fn fcntl(file_descriptor: i32, command: i32, ...) -> i32;
}

const HARD_MAX_STDOUT_BYTES: usize = 8 * 1024 * 1024;
const HARD_MAX_STDERR_BYTES: usize = 256 * 1024;
const HARD_MAX_ARGUMENTS: usize = 256;
const HARD_MAX_ARGUMENT_BYTES: usize = 256 * 1024;
const HARD_MAX_STAGE_FILES: usize = 256;
const HARD_MAX_STAGE_ARGUMENT_BYTES: usize = 256 * 1024;
const HARD_MAX_DIFF_BYTES: usize = 8 * 1024 * 1024;
const HARD_MAX_GRAPH_FILE_BYTES: u64 = 16 * 1024 * 1024;
const HARD_MAX_GRAPH_NODES: usize = 16 * 1024;
const HARD_MAX_GRAPH_DEPTH: usize = 32;
const HARD_MAX_APPROVED_GRAPH_ROOTS: usize = 128;
const HARD_MAX_ALTERNATES: usize = 32;
const HARD_MAX_PACK_FILES: usize = 4096;
const HARD_MAX_REF_ENTRIES: usize = 8192;
const HARD_MAX_LOG_ENTRIES: usize = 8192;
const HARD_MAX_WORKTREE_ENTRIES: usize = 1024;
const HARD_MAX_GRAPH_BYTES: u64 = 256 * 1024 * 1024;
const HARD_MAX_TIMEOUT: Duration = Duration::from_secs(10);
const CLEANUP_RESERVE: Duration = Duration::from_secs(1);
const READER_DROP_TIMEOUT: Duration = Duration::from_millis(250);
const HARD_MAX_READER_REAPERS: usize = 16;

#[derive(Clone, Debug)]
pub(crate) struct GitLimits {
    max_stdout_bytes: usize,
    max_stderr_bytes: usize,
    timeout: Duration,
}

impl Default for GitLimits {
    fn default() -> Self {
        Self {
            max_stdout_bytes: HARD_MAX_STDOUT_BYTES,
            max_stderr_bytes: HARD_MAX_STDERR_BYTES,
            timeout: HARD_MAX_TIMEOUT,
        }
    }
}

impl GitLimits {
    pub(crate) fn narrow_output(self, max_stdout_bytes: usize, max_stderr_bytes: usize) -> Self {
        Self {
            max_stdout_bytes: self.max_stdout_bytes.min(max_stdout_bytes),
            max_stderr_bytes: self.max_stderr_bytes.min(max_stderr_bytes),
            timeout: self.timeout,
        }
    }

    pub(crate) fn narrow_timeout(self, timeout: Duration) -> Self {
        Self {
            max_stdout_bytes: self.max_stdout_bytes,
            max_stderr_bytes: self.max_stderr_bytes,
            timeout: self.timeout.min(timeout),
        }
    }

    fn bounded(self) -> Self {
        Self {
            max_stdout_bytes: self.max_stdout_bytes.min(HARD_MAX_STDOUT_BYTES),
            max_stderr_bytes: self.max_stderr_bytes.min(HARD_MAX_STDERR_BYTES),
            timeout: self.timeout.min(HARD_MAX_TIMEOUT),
        }
    }
}

/// Private identity carried by every host authority marker.  A live bit on its
/// own is not an authority: the marker must still belong to this exact task,
/// workspace/repository graph, controller, and connection.
#[derive(Clone, Debug, Eq, PartialEq)]
struct AuthorityIdentity {
    task_id: String,
    workspace: WorkspaceIdentity,
    repository_static_identity: String,
    controller_id: String,
    connection_id: String,
}

/// Private marker for the live WorkspaceService authorization.  A Git binding
/// carries the marker itself, rather than copying a task/client/path tuple.
/// The real WorkspaceService issuer will replace the test-only constructor
/// when the Config/Workspace union lands.
#[derive(Clone)]
struct WorkspaceAuthorization(Arc<AuthorityState>);

#[derive(Clone)]
struct ResourceLease(Arc<AuthorityState>);

#[derive(Clone)]
struct ControllerHandle(Arc<AuthorityState>);

#[derive(Clone)]
struct ConnectionHandle(Arc<AuthorityState>);

#[derive(Clone)]
struct ActionGeneration {
    current: Arc<AtomicU64>,
    issued: u64,
}

struct AuthorityState {
    generation: AtomicU64,
    issued_generation: u64,
    identity: AuthorityIdentity,
}

impl AuthorityState {
    fn new(identity: AuthorityIdentity) -> Self {
        Self {
            generation: AtomicU64::new(1),
            issued_generation: 1,
            identity,
        }
    }

    fn is_live_for(&self, expected: &AuthorityIdentity) -> bool {
        self.generation.load(Ordering::Acquire) == self.issued_generation
            && &self.identity == expected
    }
}

macro_rules! authority_marker_live {
    ($marker:ident) => {
        impl $marker {
            fn is_live_for(&self, expected: &AuthorityIdentity) -> bool {
                self.0.is_live_for(expected)
            }
        }
    };
}

authority_marker_live!(WorkspaceAuthorization);
authority_marker_live!(ResourceLease);
authority_marker_live!(ControllerHandle);
authority_marker_live!(ConnectionHandle);

impl ActionGeneration {
    fn is_current(&self) -> bool {
        self.issued != 0 && self.current.load(Ordering::Acquire) == self.issued
    }
}

/// This is the sole in-crate authority contract consumed by the Git runner.
/// It is intentionally sealed: no caller can construct a binding from a
/// scalar path, lease number, or display metadata.  In particular, the live
/// handles remain owned by the capability while any child is running.
#[derive(Clone)]
struct GitAuthorityCapability {
    workspace_authorization: WorkspaceAuthorization,
    resource_lease: ResourceLease,
    controller: ControllerHandle,
    connection: ConnectionHandle,
    action_generation: ActionGeneration,
    identity: AuthorityIdentity,
    root: PathBuf,
    root_handle: Arc<fs::File>,
    graph_handles: Arc<Vec<fs::File>>,
    repository_identity: Arc<Mutex<String>>,
    repository_static_identity: String,
    approved_external_roots: Vec<PathBuf>,
    authority_deadline: Instant,
    limits: GitLimits,
}

impl GitAuthorityCapability {
    // Filesystem graph proof is deliberately not performed here: every
    // operation calls the deadline-aware RepositoryRoot validator immediately
    // before and after its effect. Keeping this check marker/lease-only avoids
    // an unbounded path read from becoming an authority decision.
    fn is_live(&self) -> bool {
        self.workspace_authorization.is_live_for(&self.identity)
            && self.resource_lease.is_live_for(&self.identity)
            && self.controller.is_live_for(&self.identity)
            && self.connection.is_live_for(&self.identity)
            && self.action_generation.is_current()
            && Instant::now() < self.authority_deadline
            && Arc::strong_count(&self.root_handle) >= 1
            && !self.graph_handles.is_empty()
    }
}

/// Opaque host-issued Git authority.  The production app currently carries
/// `Option<GitHostBinding>` and deliberately supplies `None` until the
/// Config/Workspace union can issue the complete capability.  The only
/// issuer in this revision is `#[cfg(test)]`.
#[derive(Clone)]
pub(crate) struct GitHostBinding {
    capability: Arc<GitAuthorityCapability>,
}

impl fmt::Debug for GitHostBinding {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("GitHostBinding")
            .field("live", &self.capability.is_live())
            .field("authority", &"<sealed-workspace-capability>")
            .field("root", &"<workspace-root-handle>")
            .field("graph", &"<retained-graph-handles>")
            .finish()
    }
}

#[cfg(test)]
impl GitHostBinding {
    fn has_live_authority_for_test(&self) -> bool {
        self.capability.is_live()
    }

    fn retained_handle_count_for_test(&self) -> usize {
        1 + self.capability.graph_handles.len()
    }
}

#[cfg(test)]
pub(crate) fn test_issue_git_host_binding(
    root: impl AsRef<Path>,
    approved_external_roots: Vec<PathBuf>,
) -> Result<GitHostBinding, GitError> {
    let root =
        RepositoryRoot::open_with_approved_external_roots(root.as_ref(), &approved_external_roots)
            .map_err(|reason| GitError::InvalidRepositoryRoot {
                path: "<test-bound-root>".to_string(),
                reason,
            })?;
    let approved_external_roots =
        canonicalize_approved_graph_roots(&root.path, &approved_external_roots).map_err(
            |reason| GitError::InvalidRepositoryRoot {
                path: "<test-bound-root>".to_string(),
                reason,
            },
        )?;
    let repository_identity = repository_graph_identity(&root);
    let repository_static_identity = repository_static_graph_identity(&root);
    let workspace = WorkspaceIdentity::from_canonical_root(root.path.clone());
    let identity = AuthorityIdentity {
        task_id: "test-task-6-6a".to_string(),
        workspace,
        repository_static_identity: repository_static_identity.clone(),
        controller_id: "test-controller".to_string(),
        connection_id: "test-connection".to_string(),
    };
    let state = Arc::new(AuthorityState::new(identity.clone()));
    Ok(GitHostBinding {
        capability: Arc::new(GitAuthorityCapability {
            workspace_authorization: WorkspaceAuthorization(Arc::clone(&state)),
            resource_lease: ResourceLease(Arc::clone(&state)),
            controller: ControllerHandle(Arc::clone(&state)),
            connection: ConnectionHandle(Arc::clone(&state)),
            action_generation: ActionGeneration {
                current: Arc::new(AtomicU64::new(1)),
                issued: 1,
            },
            identity,
            root: root.path,
            root_handle: Arc::clone(&root.handle),
            graph_handles: Arc::clone(&root.pinned_handles),
            repository_identity: Arc::new(Mutex::new(repository_identity)),
            repository_static_identity,
            approved_external_roots,
            authority_deadline: Instant::now() + Duration::from_secs(30),
            limits: GitLimits::default(),
        }),
    })
}

/// A single operation permit is deliberately non-Clone.  It is created by the
/// eventual Workspace/Config issuer (the only current issuers are the
/// test-only fixtures) and carries the exact operation, plan digest, endpoint,
/// graph authority, action generation, absolute deadline, and limits.
#[derive(Clone)]
enum GitPermitAuthority {
    Host(Arc<GitAuthorityCapability>),
    #[cfg(test)]
    Test,
}

#[derive(Clone)]
enum GitPermitOperation {
    ReadOnly,
    Mutation {
        capability: GitCapability,
        plan_digest: String,
        remote_policy: Option<RemotePolicy>,
        remote_name: Option<String>,
    },
    ServiceMutation {
        plan_digest: String,
        remote_policy: Option<RemotePolicy>,
        remote_name: Option<String>,
    },
}

pub(crate) struct GitOperationPermit {
    authority: GitPermitAuthority,
    operation: GitPermitOperation,
    deadline: OperationDeadline,
}

impl GitOperationPermit {
    fn host_read(capability: Arc<GitAuthorityCapability>) -> Self {
        let deadline = OperationDeadline::from_host_authority(
            capability.authority_deadline,
            capability.limits.timeout,
        );
        Self {
            authority: GitPermitAuthority::Host(capability),
            operation: GitPermitOperation::ReadOnly,
            deadline,
        }
    }

    #[cfg(test)]
    fn test_read() -> Self {
        Self::test_read_with_timeout(HARD_MAX_TIMEOUT)
    }

    #[cfg(test)]
    fn test_read_with_timeout(timeout: Duration) -> Self {
        Self {
            authority: GitPermitAuthority::Test,
            operation: GitPermitOperation::ReadOnly,
            deadline: OperationDeadline::from_now(timeout.min(HARD_MAX_TIMEOUT)),
        }
    }

    #[cfg(test)]
    fn test_mutation<P: MutationPlan>(plan: &P) -> Self {
        Self::test_mutation_with_timeout(plan, HARD_MAX_TIMEOUT)
    }

    #[cfg(test)]
    fn test_mutation_with_timeout<P: MutationPlan>(plan: &P, timeout: Duration) -> Self {
        Self {
            authority: GitPermitAuthority::Test,
            operation: GitPermitOperation::Mutation {
                capability: plan.capability(),
                plan_digest: plan.plan_digest(),
                remote_policy: plan.remote_policy().cloned(),
                remote_name: plan.remote_name().map(str::to_string),
            },
            deadline: OperationDeadline::from_now(timeout.min(HARD_MAX_TIMEOUT)),
        }
    }

    #[cfg(test)]
    fn host_mutation<P: MutationPlan>(capability: Arc<GitAuthorityCapability>, plan: &P) -> Self {
        let deadline = OperationDeadline::from_host_authority(
            capability.authority_deadline,
            capability.limits.timeout,
        );
        Self {
            authority: GitPermitAuthority::Host(capability),
            operation: GitPermitOperation::Mutation {
                capability: plan.capability(),
                plan_digest: plan.plan_digest(),
                remote_policy: plan.remote_policy().cloned(),
                remote_name: plan.remote_name().map(str::to_string),
            },
            deadline,
        }
    }

    fn host_service(
        capability: Arc<GitAuthorityCapability>,
        arguments: &[OsString],
        remote_policy: Option<RemotePolicy>,
        remote_name: Option<String>,
    ) -> Self {
        let deadline = OperationDeadline::from_host_authority(
            capability.authority_deadline,
            capability.limits.timeout,
        );
        let plan_digest =
            service_mutation_digest(arguments, remote_policy.as_ref(), remote_name.as_deref());
        Self {
            authority: GitPermitAuthority::Host(capability),
            operation: GitPermitOperation::ServiceMutation {
                plan_digest,
                remote_policy,
                remote_name,
            },
            deadline,
        }
    }

    #[cfg(test)]
    pub(crate) fn test_service_mutation(
        arguments: &[OsString],
        remote_policy: Option<RemotePolicy>,
        remote_name: Option<String>,
    ) -> Self {
        Self::test_service_mutation_with_timeout(
            arguments,
            remote_policy,
            remote_name,
            HARD_MAX_TIMEOUT,
        )
    }

    #[cfg(test)]
    fn test_service_mutation_with_timeout(
        arguments: &[OsString],
        remote_policy: Option<RemotePolicy>,
        remote_name: Option<String>,
        timeout: Duration,
    ) -> Self {
        let plan_digest =
            service_mutation_digest(arguments, remote_policy.as_ref(), remote_name.as_deref());
        Self {
            authority: GitPermitAuthority::Test,
            operation: GitPermitOperation::ServiceMutation {
                plan_digest,
                remote_policy,
                remote_name,
            },
            deadline: OperationDeadline::from_now(timeout.min(HARD_MAX_TIMEOUT)),
        }
    }

    fn is_live(&self) -> bool {
        (match &self.authority {
            GitPermitAuthority::Host(capability) => capability.is_live(),
            #[cfg(test)]
            GitPermitAuthority::Test => true,
        }) && !self.deadline.is_expired()
    }

    fn renewed_for_execution(&self) -> Result<Self, GitError> {
        if !self.is_live() {
            return Err(GitError::AuthorityUnavailable);
        }
        let deadline = match &self.authority {
            GitPermitAuthority::Host(capability) => {
                if !capability.is_live() {
                    return Err(GitError::AuthorityUnavailable);
                }
                self.deadline
            }
            #[cfg(test)]
            GitPermitAuthority::Test => self.deadline,
        };
        Ok(Self {
            authority: self.authority.clone(),
            operation: self.operation.clone(),
            deadline,
        })
    }

    fn operation_matches_policy(
        &self,
        policy: &GitExecutionPolicy,
        arguments: &[OsString],
    ) -> bool {
        match (&self.operation, policy) {
            (GitPermitOperation::ReadOnly, GitExecutionPolicy::ReadOnly) => true,
            (
                GitPermitOperation::Mutation {
                    capability,
                    remote_policy,
                    remote_name,
                    ..
                },
                GitExecutionPolicy::AuthorizedMutation {
                    capability: requested,
                    remote,
                    remote_name: requested_name,
                },
            ) => {
                capability == requested
                    && remote_policy_matches(remote_policy.as_ref(), remote.as_ref())
                    && remote_name.as_ref() == requested_name.as_ref()
            }
            (
                GitPermitOperation::ServiceMutation {
                    plan_digest,
                    remote_policy,
                    remote_name,
                },
                GitExecutionPolicy::ServiceMutation {
                    remote,
                    remote_name: requested_name,
                },
            ) => {
                plan_digest
                    == &service_mutation_digest(
                        arguments,
                        remote.as_ref(),
                        requested_name.as_deref(),
                    )
                    && remote_policy_matches(remote_policy.as_ref(), remote.as_ref())
                    && remote_name.as_ref() == requested_name.as_ref()
            }
            _ => false,
        }
    }

    fn plan_matches<P: MutationPlan>(&self, plan: &P) -> bool {
        matches!(
            &self.operation,
            GitPermitOperation::Mutation {
                capability,
                plan_digest,
                remote_policy,
                remote_name,
            } if *capability == plan.capability()
                && plan_digest == &plan.plan_digest()
                && remote_policy_matches(remote_policy.as_ref(), plan.remote_policy())
                && remote_name.as_deref() == plan.remote_name()
        )
    }
}

pub struct GitConfirmation {
    permit: GitOperationPermit,
    capability: GitCapability,
    workspace: WorkspaceIdentity,
    plan_digest: String,
    remote_policy: Option<RemotePolicy>,
}

impl fmt::Debug for GitConfirmation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("GitConfirmation")
            .field("capability", &self.capability)
            .field("workspace", &"<workspace>")
            .field("plan_digest", &self.plan_digest)
            .field("remote_policy", &self.remote_policy)
            .finish()
    }
}

impl GitConfirmation {
    pub fn capability(&self) -> GitCapability {
        self.capability
    }
}

#[derive(Clone, Debug, Default)]
pub(crate) struct GitCapabilityGate {
    capabilities: u8,
    authorized_remotes: Vec<RemotePolicy>,
}

impl GitCapabilityGate {
    pub(crate) fn new(capabilities: impl IntoIterator<Item = GitCapability>) -> Self {
        let mut gate = Self::default();
        for capability in capabilities {
            gate.capabilities |= capability_bit(capability);
        }
        gate
    }

    pub(crate) fn read_only() -> Self {
        Self::default()
    }

    pub(crate) fn allows(&self, capability: GitCapability) -> bool {
        self.capabilities & capability_bit(capability) != 0
    }

    pub(crate) fn confirm<P: MutationPlan>(
        &self,
        plan: &P,
        permit: GitOperationPermit,
    ) -> Result<GitConfirmation, GitError> {
        if !self.allows(plan.capability()) {
            return Err(GitError::CapabilityDenied {
                capability: plan.capability(),
            });
        }
        if !permit.is_live() || !permit.plan_matches(plan) {
            return Err(GitError::AuthorityUnavailable);
        }
        if let Some(remote) = plan.remote_policy() {
            if !self
                .authorized_remotes
                .iter()
                .any(|authorized| authorized == remote)
            {
                return Err(GitError::RemoteNotAuthorized);
            }
        }
        Ok(GitConfirmation {
            permit,
            capability: plan.capability(),
            workspace: plan.workspace().clone(),
            plan_digest: plan.plan_digest(),
            remote_policy: plan.remote_policy().cloned(),
        })
    }

    pub(crate) fn authorize_remote(&mut self, remote: RemotePolicy) {
        if !self
            .authorized_remotes
            .iter()
            .any(|authorized| authorized == &remote)
        {
            self.authorized_remotes.push(remote);
        }
    }
}

fn capability_bit(capability: GitCapability) -> u8 {
    match capability {
        GitCapability::Stage => 1 << 0,
        GitCapability::Unstage => 1 << 1,
        GitCapability::Commit => 1 << 2,
        GitCapability::Push => 1 << 3,
        GitCapability::CreatePullRequest => 1 << 4,
    }
}

#[derive(Clone, Debug, Default)]
pub(crate) struct GitCancellation(Arc<AtomicBool>);

impl GitCancellation {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn cancel(&self) {
        self.0.store(true, Ordering::Release);
    }

    pub(crate) fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

pub enum GitError {
    InvalidRepositoryRoot {
        path: String,
        reason: String,
    },
    InvalidPath {
        path: String,
        reason: String,
    },
    InvalidRequest {
        message: String,
    },
    Parse {
        message: String,
    },
    CommandStart {
        operation: String,
        message: String,
    },
    CommandFailed {
        operation: String,
        code: Option<i32>,
        stderr: String,
    },
    CleanupFailed {
        operation: String,
        reason: String,
    },
    TimedOut {
        operation: String,
        timeout: Duration,
    },
    Cancelled {
        operation: String,
    },
    OutputLimitExceeded {
        stream: &'static str,
        limit: usize,
    },
    FingerprintMismatch {
        expected: crate::git::model::RepoFingerprint,
        actual: crate::git::model::RepoFingerprint,
    },
    NoUpstream {
        branch: Option<BranchName>,
    },
    CapabilityDenied {
        capability: GitCapability,
    },
    ConfirmationMismatch {
        capability: GitCapability,
    },
    RemoteNotAuthorized,
    WorkspaceMismatch {
        expected: String,
        actual: String,
    },
    AuthorityUnavailable,
}

impl fmt::Display for GitError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidRepositoryRoot { .. } => {
                formatter.write_str("invalid Git repository root")
            }
            Self::InvalidPath { .. } => formatter.write_str("invalid repository path"),
            Self::InvalidRequest { .. } => formatter.write_str("invalid Git request"),
            Self::Parse { .. } => formatter.write_str("Git output could not be parsed"),
            Self::CommandStart { .. } => formatter.write_str("could not start Git operation"),
            Self::CommandFailed { code, stderr, .. } => {
                write!(
                    formatter,
                    "Git operation failed{}",
                    code.map_or_else(String::new, |code| format!(" (exit {code})")),
                )?;
                let safe_details = sanitize_command_output(stderr);
                if !safe_details.is_empty() {
                    write!(formatter, ": {safe_details}")?;
                }
                Ok(())
            }
            Self::CleanupFailed { .. } => formatter.write_str("Git operation cleanup failed"),
            Self::TimedOut { timeout, .. } => write!(
                formatter,
                "Git operation exceeded the {}ms deadline",
                timeout.as_millis()
            ),
            Self::Cancelled { .. } => formatter.write_str("Git operation was cancelled"),
            Self::OutputLimitExceeded { stream, limit } => {
                write!(formatter, "Git {stream} exceeded the {limit}-byte limit")
            }
            Self::FingerprintMismatch { .. } => {
                formatter.write_str("Git workspace changed since the action preview")
            }
            Self::NoUpstream { .. } => {
                formatter.write_str("the current branch has no authorized upstream")
            }
            Self::CapabilityDenied { capability } => {
                write!(formatter, "Git capability {capability:?} was not granted")
            }
            Self::ConfirmationMismatch { capability } => write!(
                formatter,
                "Git {capability:?} confirmation does not match this workspace plan"
            ),
            Self::RemoteNotAuthorized => formatter.write_str("Git remote is not authorized"),
            Self::WorkspaceMismatch { .. } => {
                formatter.write_str("Git workspace identity does not match")
            }
            Self::AuthorityUnavailable => {
                formatter.write_str("Git requires a live WorkspaceService authority")
            }
        }
    }
}

impl std::error::Error for GitError {}

impl fmt::Debug for GitError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("GitError")
            .field("category", &self.category())
            .finish()
    }
}

impl GitError {
    fn category(&self) -> &'static str {
        match self {
            Self::InvalidRepositoryRoot { .. } => "invalid_repository_root",
            Self::InvalidPath { .. } => "invalid_path",
            Self::InvalidRequest { .. } => "invalid_request",
            Self::Parse { .. } => "parse",
            Self::CommandStart { .. } => "command_start",
            Self::CommandFailed { .. } => "command_failed",
            Self::CleanupFailed { .. } => "cleanup_failed",
            Self::TimedOut { .. } => "timed_out",
            Self::Cancelled { .. } => "cancelled",
            Self::OutputLimitExceeded { .. } => "output_limit_exceeded",
            Self::FingerprintMismatch { .. } => "fingerprint_mismatch",
            Self::NoUpstream { .. } => "no_upstream",
            Self::CapabilityDenied { .. } => "capability_denied",
            Self::ConfirmationMismatch { .. } => "confirmation_mismatch",
            Self::RemoteNotAuthorized => "remote_not_authorized",
            Self::WorkspaceMismatch { .. } => "workspace_mismatch",
            Self::AuthorityUnavailable => "authority_unavailable",
        }
    }
}

pub(crate) struct GitOutput {
    pub(crate) stdout: Vec<u8>,
    pub(crate) stderr: Vec<u8>,
    pub(crate) status: ExitStatus,
}

impl fmt::Debug for GitOutput {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("GitOutput")
            .field("stdout_bytes", &self.stdout.len())
            .field("stderr_bytes", &self.stderr.len())
            .field("status", &self.status)
            .finish()
    }
}

#[derive(Clone, Debug)]
enum GitExecutionPolicy {
    ReadOnly,
    AuthorizedMutation {
        capability: GitCapability,
        remote: Option<RemotePolicy>,
        remote_name: Option<String>,
    },
    ServiceMutation {
        remote: Option<RemotePolicy>,
        remote_name: Option<String>,
    },
}

fn execution_endpoint_lease(policy: &GitExecutionPolicy) -> Option<Arc<RemoteEndpointLease>> {
    match policy {
        GitExecutionPolicy::AuthorizedMutation {
            remote: Some(remote),
            ..
        }
        | GitExecutionPolicy::ServiceMutation {
            remote: Some(remote),
            ..
        } => remote.endpoint_lease().cloned(),
        _ => None,
    }
}

#[derive(Clone, Copy, Debug)]
struct OperationDeadline {
    deadline: Instant,
    timeout: Duration,
}

impl OperationDeadline {
    fn from_now(timeout: Duration) -> Self {
        let now = Instant::now();
        Self {
            deadline: now.checked_add(timeout).unwrap_or(now),
            timeout,
        }
    }

    fn from_absolute(deadline: Instant, timeout: Duration) -> Self {
        Self { deadline, timeout }
    }

    fn from_host_authority(authority_deadline: Instant, timeout: Duration) -> Self {
        let timeout = timeout.min(HARD_MAX_TIMEOUT);
        let timeout_deadline = Instant::now()
            .checked_add(timeout)
            .unwrap_or(authority_deadline);
        Self::from_absolute(authority_deadline.min(timeout_deadline), timeout)
    }

    fn is_expired(self) -> bool {
        Instant::now() >= self.deadline
    }

    fn remaining(self) -> Duration {
        self.deadline.saturating_duration_since(Instant::now())
    }

    fn sleep(self) {
        let remaining = self.remaining();
        if !remaining.is_zero() {
            thread::sleep(remaining.min(Duration::from_millis(2)));
        }
    }

    fn with_cleanup_reserve(self) -> Self {
        let now = Instant::now();
        let base = if self.deadline > now {
            self.deadline
        } else {
            now
        };
        Self {
            deadline: base.checked_add(CLEANUP_RESERVE).unwrap_or(base),
            timeout: self.timeout.saturating_add(CLEANUP_RESERVE),
        }
    }
}

#[derive(Clone, Eq, PartialEq)]
struct FileIdentity {
    #[cfg(windows)]
    volume_serial_number: u32,
    #[cfg(windows)]
    file_index: u64,
    #[cfg(windows)]
    number_of_links: u32,
    #[cfg(windows)]
    file_size: u64,
    #[cfg(windows)]
    last_write_time: u64,
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
    #[cfg(unix)]
    number_of_links: u64,
    #[cfg(unix)]
    file_size: u64,
    #[cfg(unix)]
    modified_seconds: i64,
    #[cfg(unix)]
    modified_nanos: i64,
    content_digest: [u8; 32],
}

#[cfg(windows)]
#[repr(C)]
struct WindowsFileTime {
    low: u32,
    high: u32,
}

#[cfg(windows)]
#[repr(C)]
struct WindowsByHandleFileInformation {
    attributes: u32,
    creation_time: WindowsFileTime,
    last_access_time: WindowsFileTime,
    last_write_time: WindowsFileTime,
    volume_serial_number: u32,
    file_size_high: u32,
    file_size_low: u32,
    number_of_links: u32,
    file_index_high: u32,
    file_index_low: u32,
}

#[cfg(windows)]
const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;

#[cfg(windows)]
const INVALID_HANDLE_VALUE: *mut std::ffi::c_void = -1_isize as *mut std::ffi::c_void;

#[cfg(windows)]
const FILE_READ_ATTRIBUTES: u32 = 0x0080;
#[cfg(windows)]
const GENERIC_READ: u32 = 0x8000_0000;
#[cfg(windows)]
const SYNCHRONIZE: u32 = 0x0010_0000;
#[cfg(windows)]
const FILE_SHARE_READ: u32 = 0x0000_0001;
#[cfg(windows)]
const FILE_SHARE_WRITE: u32 = 0x0000_0002;
#[cfg(windows)]
const FILE_SHARE_DELETE: u32 = 0x0000_0004;
#[cfg(windows)]
const OPEN_EXISTING: u32 = 3;
#[cfg(windows)]
const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;

#[cfg(windows)]
extern "system" {
    fn CreateFileW(
        file_name: *const u16,
        desired_access: u32,
        share_mode: u32,
        security_attributes: *mut std::ffi::c_void,
        creation_disposition: u32,
        flags_and_attributes: u32,
        template_file: *mut std::ffi::c_void,
    ) -> *mut std::ffi::c_void;
    fn GetFileInformationByHandle(
        file: *mut std::ffi::c_void,
        information: *mut WindowsByHandleFileInformation,
    ) -> i32;
}

fn file_identity(path: &Path) -> Result<FileIdentity, String> {
    stable_file_identity(path, true, None, true)
}

fn data_file_identity(path: &Path) -> Result<FileIdentity, String> {
    stable_file_identity(path, false, None, true)
}

fn data_file_identity_with_deadline(
    path: &Path,
    deadline: OperationDeadline,
) -> Result<FileIdentity, String> {
    stable_file_identity(path, false, Some(deadline), true)
}

fn file_identity_with_deadline(
    path: &Path,
    deadline: OperationDeadline,
) -> Result<FileIdentity, String> {
    stable_file_identity(path, true, Some(deadline), true)
}

#[cfg(test)]
fn test_file_identity(path: &Path) -> Result<FileIdentity, String> {
    // Test-only process fixtures may resolve to Windows system binaries whose
    // image is legitimately hard-linked.  Their identity is still pinned by
    // file id, size, timestamp, and digest; repository graph data files use
    // `data_file_identity` above and continue to reject hard links.
    stable_file_identity(path, false, None, false)
}

#[cfg(test)]
fn test_file_identity_with_deadline(
    path: &Path,
    deadline: OperationDeadline,
) -> Result<FileIdentity, String> {
    stable_file_identity(path, false, Some(deadline), false)
}

fn stable_file_identity(
    path: &Path,
    require_native_image: bool,
    deadline: Option<OperationDeadline>,
    reject_hard_links: bool,
) -> Result<FileIdentity, String> {
    if deadline.is_some_and(OperationDeadline::is_expired) {
        return Err("Git executable identity exceeded the operation deadline".to_string());
    }
    reject_reparse_components(path)?;
    let metadata = fs::symlink_metadata(path)
        .map_err(|_| "Git executable metadata is unavailable".to_string())?;
    if !metadata.is_file() {
        return Err("Git executable is not a regular file".to_string());
    }
    if !require_native_image && metadata.len() > HARD_MAX_GRAPH_FILE_BYTES {
        return Err("Git graph file exceeds the immutable size limit".to_string());
    }
    if require_native_image {
        validate_native_image(path, deadline)?;
    }
    let mut file = fs::File::open(path)
        .map_err(|_| "Git executable cannot be opened for identity".to_string())?;
    let content_digest = digest_file(
        &mut file,
        deadline,
        (!require_native_image).then_some(HARD_MAX_GRAPH_FILE_BYTES),
    )?;

    #[cfg(windows)]
    {
        let mut information = std::mem::MaybeUninit::<WindowsByHandleFileInformation>::uninit();
        let result =
            unsafe { GetFileInformationByHandle(file.as_raw_handle(), information.as_mut_ptr()) };
        if result == 0 {
            return Err("Git executable file identity is unavailable".to_string());
        }
        let information = unsafe { information.assume_init() };
        if reject_hard_links
            && information.number_of_links != 1
            && (!require_native_image || !is_explicitly_trusted_git_path(path))
        {
            return Err("Git graph file hard-link ambiguity is not allowed".to_string());
        }
        return Ok(FileIdentity {
            volume_serial_number: information.volume_serial_number,
            file_index: (u64::from(information.file_index_high) << 32)
                | u64::from(information.file_index_low),
            number_of_links: information.number_of_links,
            file_size: (u64::from(information.file_size_high) << 32)
                | u64::from(information.file_size_low),
            last_write_time: (u64::from(information.last_write_time.high) << 32)
                | u64::from(information.last_write_time.low),
            content_digest,
        });
    }

    #[cfg(unix)]
    {
        if reject_hard_links
            && metadata.nlink() != 1
            && (!require_native_image || !is_explicitly_trusted_git_path(path))
        {
            return Err("Git graph file hard-link ambiguity is not allowed".to_string());
        }
        return Ok(FileIdentity {
            device: metadata.dev(),
            inode: metadata.ino(),
            number_of_links: metadata.nlink(),
            file_size: metadata.size(),
            modified_seconds: metadata.mtime(),
            modified_nanos: metadata.mtime_nsec(),
            content_digest,
        });
    }

    #[cfg(not(any(unix, windows)))]
    {
        Ok(FileIdentity { content_digest })
    }
}

fn directory_identity(path: &Path) -> Result<FileIdentity, String> {
    reject_reparse_components(path)?;
    let metadata = fs::symlink_metadata(path)
        .map_err(|_| "repository root metadata is unavailable".to_string())?;
    if !metadata.is_dir() {
        return Err("repository root is not a directory".to_string());
    }

    #[cfg(windows)]
    {
        let handle = open_directory_handle(path)?;
        let mut identity = windows_handle_identity(&handle, [0; 32])?;
        // Directory timestamps and reported size change during ordinary Git
        // lock/ref/index maintenance.  The native file id and link count are
        // the stable replacement-detection identity for a directory.
        identity.file_size = 0;
        identity.last_write_time = 0;
        return Ok(identity);
    }

    #[cfg(unix)]
    {
        return Ok(FileIdentity {
            device: metadata.dev(),
            inode: metadata.ino(),
            number_of_links: metadata.nlink(),
            file_size: 0,
            modified_seconds: 0,
            modified_nanos: 0,
            content_digest: [0; 32],
        });
    }

    #[cfg(not(any(unix, windows)))]
    {
        Ok(FileIdentity {
            content_digest: [0; 32],
        })
    }
}

fn directory_identity_with_deadline(
    path: &Path,
    deadline: Option<OperationDeadline>,
) -> Result<FileIdentity, String> {
    if deadline.is_some_and(OperationDeadline::is_expired) {
        return Err("repository graph validation exceeded the operation deadline".to_string());
    }
    let identity = directory_identity(path)?;
    if deadline.is_some_and(OperationDeadline::is_expired) {
        return Err("repository graph validation exceeded the operation deadline".to_string());
    }
    Ok(identity)
}

#[cfg(windows)]
fn open_directory_handle(path: &Path) -> Result<fs::File, String> {
    open_windows_directory_handle(path, FILE_SHARE_READ | FILE_SHARE_WRITE)
}

#[cfg(windows)]
fn open_windows_directory_handle(path: &Path, share_mode: u32) -> Result<fs::File, String> {
    let mut wide = path.as_os_str().encode_wide().collect::<Vec<_>>();
    wide.push(0);
    let handle = unsafe {
        CreateFileW(
            wide.as_ptr(),
            GENERIC_READ | FILE_READ_ATTRIBUTES | SYNCHRONIZE,
            share_mode,
            std::ptr::null_mut(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS,
            std::ptr::null_mut(),
        )
    };
    if handle.is_null() || handle == INVALID_HANDLE_VALUE {
        return Err("repository root cannot be held by handle".to_string());
    }
    Ok(unsafe { fs::File::from_raw_handle(handle) })
}

#[cfg(windows)]
fn open_executable_spawn_handle(path: &Path) -> Result<fs::File, String> {
    let mut wide = path.as_os_str().encode_wide().collect::<Vec<_>>();
    wide.push(0);
    // Do not share delete/write while the binding is alive.  This makes the
    // final path check and CreateProcess use one replacement-resistant file
    // identity instead of a path-only TOCTOU window.
    let handle = unsafe {
        CreateFileW(
            wide.as_ptr(),
            GENERIC_READ | FILE_READ_ATTRIBUTES | SYNCHRONIZE,
            FILE_SHARE_READ,
            std::ptr::null_mut(),
            OPEN_EXISTING,
            0,
            std::ptr::null_mut(),
        )
    };
    if handle.is_null() || handle == INVALID_HANDLE_VALUE {
        return Err("trusted Git executable cannot be held for spawn".to_string());
    }
    Ok(unsafe { fs::File::from_raw_handle(handle) })
}

#[cfg(windows)]
fn pin_executable_ancestors(
    executable: &Path,
    deadline: Option<OperationDeadline>,
) -> Result<Vec<fs::File>, String> {
    let mut handles = Vec::new();
    let mut ancestor = executable.parent();
    let mut depth = 0;
    while let Some(path) = ancestor {
        if depth >= HARD_MAX_GRAPH_DEPTH {
            return Err("trusted Git executable ancestor graph is too deep".to_string());
        }
        depth += 1;
        if deadline.is_some_and(OperationDeadline::is_expired) {
            return Err(
                "trusted Git executable ancestor binding exceeded the operation deadline"
                    .to_string(),
            );
        }
        reject_reparse_components(path)?;
        let metadata = fs::symlink_metadata(path)
            .map_err(|_| "trusted Git executable ancestor is unavailable".to_string())?;
        if !metadata.is_dir() {
            return Err("trusted Git executable ancestor is not a directory".to_string());
        }
        handles.push(open_directory_handle_for_pin(path)?);
        ancestor = path.parent();
    }
    Ok(handles)
}

#[cfg(windows)]
fn windows_handle_identity(
    file: &fs::File,
    content_digest: [u8; 32],
) -> Result<FileIdentity, String> {
    let mut information = std::mem::MaybeUninit::<WindowsByHandleFileInformation>::uninit();
    let result =
        unsafe { GetFileInformationByHandle(file.as_raw_handle(), information.as_mut_ptr()) };
    if result == 0 {
        return Err("file identity is unavailable".to_string());
    }
    let information = unsafe { information.assume_init() };
    Ok(FileIdentity {
        volume_serial_number: information.volume_serial_number,
        file_index: (u64::from(information.file_index_high) << 32)
            | u64::from(information.file_index_low),
        number_of_links: information.number_of_links,
        file_size: (u64::from(information.file_size_high) << 32)
            | u64::from(information.file_size_low),
        last_write_time: (u64::from(information.last_write_time.high) << 32)
            | u64::from(information.last_write_time.low),
        content_digest,
    })
}

fn digest_file(
    file: &mut fs::File,
    deadline: Option<OperationDeadline>,
    max_bytes: Option<u64>,
) -> Result<[u8; 32], String> {
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    let mut bytes_read = 0u64;
    loop {
        if deadline.is_some_and(OperationDeadline::is_expired) {
            return Err("Git executable identity exceeded the operation deadline".to_string());
        }
        let count = file
            .read(&mut buffer)
            .map_err(|_| "Git executable content could not be read".to_string())?;
        if count == 0 {
            break;
        }
        bytes_read = bytes_read
            .checked_add(count as u64)
            .ok_or_else(|| "Git graph file size accounting overflowed".to_string())?;
        if max_bytes.is_some_and(|limit| bytes_read > limit) {
            return Err("Git graph file exceeds the immutable size limit".to_string());
        }
        hasher.update(&buffer[..count]);
    }
    if deadline.is_some_and(OperationDeadline::is_expired) {
        return Err("Git executable identity exceeded the operation deadline".to_string());
    }
    let digest = hasher.finalize();
    let mut result = [0u8; 32];
    result.copy_from_slice(&digest);
    Ok(result)
}

fn validate_native_image(path: &Path, deadline: Option<OperationDeadline>) -> Result<(), String> {
    if deadline.is_some_and(OperationDeadline::is_expired) {
        return Err("Git executable identity exceeded the operation deadline".to_string());
    }
    let mut file = fs::File::open(path)
        .map_err(|_| "Git executable cannot be opened as a native image".to_string())?;
    if deadline.is_some_and(OperationDeadline::is_expired) {
        return Err("Git executable identity exceeded the operation deadline".to_string());
    }
    let mut header = [0u8; 4096];
    let count = file
        .read(&mut header)
        .map_err(|_| "Git executable image header cannot be read".to_string())?;
    if deadline.is_some_and(OperationDeadline::is_expired) {
        return Err("Git executable identity exceeded the operation deadline".to_string());
    }
    let header = &header[..count];

    #[cfg(windows)]
    {
        if header.len() < 64 || &header[..2] != b"MZ" {
            return Err("Git executable is not a native Windows image".to_string());
        }
        let pe_offset = u32::from_le_bytes(header[60..64].try_into().unwrap()) as usize;
        if pe_offset.checked_add(4).is_none()
            || pe_offset + 4 > header.len()
            || &header[pe_offset..pe_offset + 4] != b"PE\0\0"
        {
            return Err("Git executable is not a native Windows image".to_string());
        }
        return Ok(());
    }

    #[cfg(unix)]
    {
        let native = header.starts_with(b"\x7fELF")
            || header.starts_with(&[0xfe, 0xed, 0xfa, 0xce])
            || header.starts_with(&[0xce, 0xfa, 0xed, 0xfe])
            || header.starts_with(&[0xfe, 0xed, 0xfa, 0xcf])
            || header.starts_with(&[0xcf, 0xfa, 0xed, 0xfe]);
        if !native {
            return Err("Git executable is not a native image".to_string());
        }
        return Ok(());
    }

    #[cfg(not(any(unix, windows)))]
    {
        let _ = header;
        Ok(())
    }
}

fn reject_reparse_components(path: &Path) -> Result<(), String> {
    for (depth, ancestor) in path.ancestors().enumerate() {
        if depth >= HARD_MAX_GRAPH_DEPTH {
            return Err("Git path graph is too deep".to_string());
        }
        let Ok(metadata) = fs::symlink_metadata(ancestor) else {
            continue;
        };
        if metadata.file_type().is_symlink() {
            return Err("Git path contains a symlink or junction".to_string());
        }
        #[cfg(windows)]
        if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err("Git path contains a reparse point".to_string());
        }
    }
    Ok(())
}

#[derive(Clone)]
struct TrustedExecutable {
    path: PathBuf,
    identity: FileIdentity,
    #[cfg(test)]
    test_fixture: bool,
}

#[cfg(test)]
struct TestTrustedExecutable {
    executable: TrustedExecutable,
}

impl fmt::Debug for TrustedExecutable {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TrustedExecutable")
            .field("path", &"<trusted-git>")
            .field("identity", &"<stable-file-identity>")
            .finish()
    }
}

impl TrustedExecutable {
    #[cfg(test)]
    fn resolve_git() -> Result<Self, String> {
        let path_value =
            env::var_os("PATH").ok_or_else(|| "Git PATH is unavailable".to_string())?;
        Self::resolve_from_path_with_deadline(&path_value, None)
    }

    fn resolve_git_with_deadline(deadline: OperationDeadline) -> Result<Self, String> {
        let path_value =
            env::var_os("PATH").ok_or_else(|| "Git PATH is unavailable".to_string())?;
        Self::resolve_from_path_with_deadline(&path_value, Some(deadline))
    }

    #[cfg(test)]
    fn resolve_from_path(path_value: &OsStr) -> Result<Self, String> {
        Self::resolve_from_path_with_deadline(path_value, None)
    }

    fn resolve_from_path_with_deadline(
        path_value: &OsStr,
        deadline: Option<OperationDeadline>,
    ) -> Result<Self, String> {
        let executable_name = if cfg!(windows) { "git.exe" } else { "git" };
        let mut candidates: Vec<(PathBuf, FileIdentity)> = Vec::new();
        let directories = env::split_paths(path_value).collect::<Vec<_>>();
        if directories.is_empty() {
            return Err("Git PATH has no entries".to_string());
        }
        let current_directory =
            env::current_dir().map_err(|_| "Git current directory is unavailable".to_string())?;
        for directory in directories {
            if deadline.is_some_and(OperationDeadline::is_expired) {
                return Err("Git resolution exceeded the operation deadline".to_string());
            }
            if directory.as_os_str().is_empty() || !directory.is_absolute() {
                return Err("Git PATH entry must be a non-empty absolute directory".to_string());
            }
            if directory == current_directory
                || fs::canonicalize(&directory)
                    .ok()
                    .is_some_and(|canonical| same_path(&canonical, &current_directory))
            {
                return Err("Git PATH entry must not be the current directory".to_string());
            }
            let candidate = directory.join(executable_name);
            let Ok(metadata) = fs::symlink_metadata(&candidate) else {
                continue;
            };
            if !metadata.is_file() {
                return Err("Git PATH candidate is not a regular file".to_string());
            }
            if is_known_git_wrapper(&candidate) {
                continue;
            }
            let canonical = fs::canonicalize(&candidate)
                .map_err(|_| "Git executable cannot be canonicalized".to_string())?;
            if !same_path(&canonical, &candidate) {
                return Err("Git PATH candidate is a symlink or junction".to_string());
            }
            if !is_explicitly_trusted_git_path(&canonical) {
                return Err(
                    "Git PATH candidate is outside the trusted Git installation roots".to_string(),
                );
            }
            let canonical = fs::canonicalize(&candidate)
                .map_err(|_| "Git executable cannot be canonicalized".to_string())?;
            let identity = match deadline {
                Some(deadline) => file_identity_with_deadline(&canonical, deadline),
                None => file_identity(&canonical),
            }?;
            if candidates
                .iter()
                .any(|(existing, _)| same_path(existing.as_path(), &canonical))
            {
                continue;
            }
            candidates.push((canonical, identity));
        }

        if candidates.len() > 1 {
            return Err("Git PATH resolves to multiple installations".to_string());
        }

        let (path, identity) = candidates
            .into_iter()
            .next()
            .ok_or_else(|| "Git executable was not found on PATH".to_string())?;
        if deadline.is_some_and(OperationDeadline::is_expired) {
            return Err("Git resolution exceeded the operation deadline".to_string());
        }
        Ok(Self {
            path,
            identity,
            #[cfg(test)]
            test_fixture: false,
        })
    }

    #[cfg(test)]
    fn issue_test_fixture(path: &Path) -> Result<Self, String> {
        let path = resolve_test_executable_path(path)?;
        let canonical = fs::canonicalize(&path)
            .map_err(|_| "test fixture executable cannot be canonicalized".to_string())?;
        let identity = test_file_identity(&canonical)?;
        Ok(Self {
            path: canonical,
            identity,
            test_fixture: true,
        })
    }

    #[cfg(test)]
    fn test_fixture(path: &Path) -> Result<TestTrustedExecutable, String> {
        Ok(TestTrustedExecutable {
            executable: Self::issue_test_fixture(path)?,
        })
    }

    #[cfg(test)]
    fn verify(&self) -> Result<(), String> {
        self.verify_with_deadline(None)
    }

    fn verify_with_deadline(&self, deadline: Option<OperationDeadline>) -> Result<(), String> {
        if deadline.is_some_and(OperationDeadline::is_expired) {
            return Err(
                "trusted Git executable verification exceeded the operation deadline".to_string(),
            );
        }
        let canonical = fs::canonicalize(&self.path)
            .map_err(|_| "trusted Git executable path changed".to_string())?;
        if !same_path(&canonical, &self.path) {
            return Err("trusted Git executable canonical path changed".to_string());
        }
        #[cfg(test)]
        let identity = if self.test_fixture {
            match deadline {
                Some(deadline) => test_file_identity_with_deadline(&canonical, deadline)?,
                None => test_file_identity(&canonical)?,
            }
        } else {
            match deadline {
                Some(deadline) => file_identity_with_deadline(&canonical, deadline)?,
                None => file_identity(&canonical)?,
            }
        };
        #[cfg(not(test))]
        let identity = match deadline {
            Some(deadline) => file_identity_with_deadline(&canonical, deadline)?,
            None => file_identity(&canonical)?,
        };
        if identity != self.identity {
            return Err("trusted Git executable file identity changed".to_string());
        }
        Ok(())
    }
}

struct ExecutableBinding {
    command_path: OsString,
    #[cfg(unix)]
    file: fs::File,
    #[cfg(windows)]
    file: fs::File,
    #[cfg(windows)]
    /// Keep every directory between the executable and the volume root open
    /// without delete sharing for the complete spawn/child lifetime.  The
    /// executable handle protects the final file identity; these handles
    /// close the remaining parent-directory rename window.
    ancestor_handles: Vec<fs::File>,
}

impl TrustedExecutable {
    fn bind_with_deadline(
        &self,
        deadline: Option<OperationDeadline>,
    ) -> Result<ExecutableBinding, String> {
        self.verify_with_deadline(deadline)?;
        #[cfg(unix)]
        {
            let file = fs::File::open(&self.path)
                .map_err(|_| "trusted Git executable cannot be held open".to_string())?;
            if deadline.is_some_and(OperationDeadline::is_expired) {
                return Err(
                    "trusted Git executable binding exceeded the operation deadline".to_string(),
                );
            }
            let file_descriptor = file.as_raw_fd();
            let proc_fd = if Path::new("/proc/self/fd").is_dir() {
                "/proc/self/fd"
            } else {
                "/dev/fd"
            };
            return Ok(ExecutableBinding {
                command_path: OsString::from(format!("{proc_fd}/{file_descriptor}")),
                file,
            });
        }
        #[cfg(windows)]
        {
            let file = open_executable_spawn_handle(&self.path)?;
            let identity = windows_handle_identity(&file, self.identity.content_digest)?;
            if identity != self.identity {
                return Err("trusted Git executable identity changed before spawn".to_string());
            }
            let ancestor_handles = pin_executable_ancestors(&self.path, deadline)?;
            if deadline.is_some_and(OperationDeadline::is_expired) {
                return Err(
                    "trusted Git executable binding exceeded the operation deadline".to_string(),
                );
            }
            Ok(ExecutableBinding {
                command_path: self.path.clone().into_os_string(),
                file,
                ancestor_handles,
            })
        }
        #[cfg(not(any(unix, windows)))]
        {
            if deadline.is_some_and(OperationDeadline::is_expired) {
                return Err(
                    "trusted Git executable binding exceeded the operation deadline".to_string(),
                );
            }
            Ok(ExecutableBinding {
                command_path: self.path.clone().into_os_string(),
            })
        }
    }
}

fn is_known_git_wrapper(path: &Path) -> bool {
    #[cfg(windows)]
    {
        let path = path
            .to_string_lossy()
            .replace('/', "\\")
            .to_ascii_lowercase();
        return path.ends_with("\\git\\cmd\\git.exe");
    }
    #[cfg(not(windows))]
    {
        let _ = path;
        false
    }
}

fn is_explicitly_trusted_git_path(path: &Path) -> bool {
    trusted_installation_roots().into_iter().any(|root| {
        let Ok(root) = fs::canonicalize(root) else {
            return false;
        };
        if !is_within(&root, path) {
            return false;
        }
        let Ok(relative) = path.strip_prefix(&root) else {
            return false;
        };
        let relative = relative.to_string_lossy().replace('\\', "/");
        #[cfg(windows)]
        {
            matches!(
                relative.to_ascii_lowercase().as_str(),
                "mingw64/bin/git.exe" | "mingw32/bin/git.exe" | "usr/bin/git.exe" | "bin/git.exe"
            )
        }
        #[cfg(not(windows))]
        {
            matches!(
                relative.as_str(),
                "bin/git" | "lib/git-core/git" | "libexec/git-core/git"
            )
        }
    })
}

fn trusted_installation_roots() -> Vec<PathBuf> {
    #[cfg(windows)]
    {
        vec![
            PathBuf::from(r"C:\Program Files\Git"),
            PathBuf::from(r"C:\Program Files (x86)\Git"),
            PathBuf::from(r"C:\Users\Public\Git"),
        ]
    }
    #[cfg(not(windows))]
    {
        vec![
            PathBuf::from("/usr"),
            PathBuf::from("/usr/local"),
            PathBuf::from("/opt/homebrew"),
            PathBuf::from("/opt/local"),
        ]
    }
}

#[cfg(test)]
fn resolve_test_executable_path(path: &Path) -> Result<PathBuf, String> {
    if path.is_absolute() {
        return Ok(path.to_path_buf());
    }
    let path_value = env::var_os("PATH").ok_or_else(|| "test PATH is unavailable".to_string())?;
    for directory in env::split_paths(&path_value) {
        if directory.as_os_str().is_empty() {
            continue;
        }
        let candidate = directory.join(path);
        if candidate.is_file() {
            return Ok(candidate);
        }
    }
    Err("test fixture executable was not found".to_string())
}

fn same_path(left: &Path, right: &Path) -> bool {
    #[cfg(windows)]
    {
        windows_path_units(left) == windows_path_units(right)
    }

    #[cfg(not(windows))]
    {
        left == right
    }
}

#[cfg(windows)]
fn windows_path_units(path: &Path) -> Vec<u16> {
    let mut units = path
        .as_os_str()
        .encode_wide()
        .map(|unit| {
            if unit == u16::from(b'/') {
                u16::from(b'\\')
            } else {
                unit
            }
        })
        .collect::<Vec<_>>();
    const UNC_PREFIX: [u16; 8] = [
        b'\\' as u16,
        b'\\' as u16,
        b'?' as u16,
        b'\\' as u16,
        b'U' as u16,
        b'N' as u16,
        b'C' as u16,
        b'\\' as u16,
    ];
    const DOS_PREFIX: [u16; 4] = [b'\\' as u16, b'\\' as u16, b'?' as u16, b'\\' as u16];
    if units.starts_with(&UNC_PREFIX) {
        units.splice(..UNC_PREFIX.len(), [u16::from(b'\\'), u16::from(b'\\')]);
    } else if units.starts_with(&DOS_PREFIX) {
        units.drain(..DOS_PREFIX.len());
    }
    for unit in &mut units {
        if (u16::from(b'A')..=u16::from(b'Z')).contains(unit) {
            *unit += u16::from(b'a') - u16::from(b'A');
        }
    }
    while units.last() == Some(&u16::from(b'\\')) {
        units.pop();
    }
    units
}

#[derive(Clone)]
struct RepositoryGraph {
    nodes: Vec<GraphNode>,
    git_dir: PathBuf,
    common_dir: Option<PathBuf>,
    object_stores: Vec<PathBuf>,
    metadata_roots: Vec<PathBuf>,
    approved_external_roots: Vec<PathBuf>,
    mutable_baseline: Arc<Mutex<std::collections::HashMap<PathBuf, FileIdentity>>>,
    mutable_entry_baseline:
        Arc<Mutex<std::collections::HashMap<PathBuf, MutableDirectorySnapshot>>>,
    optional_static_inputs: Vec<MutableGraphInput>,
    optional_static_baseline: Arc<Mutex<std::collections::HashMap<PathBuf, Option<FileIdentity>>>>,
    optional_mutable_inputs: Vec<MutableGraphInput>,
    optional_mutable_baseline: Arc<Mutex<std::collections::HashMap<PathBuf, Option<FileIdentity>>>>,
    optional_mutable_entry_baseline:
        Arc<Mutex<std::collections::HashMap<PathBuf, MutableDirectorySnapshot>>>,
}

/// The only mutable graph transitions that a Git operation may adopt after
/// the host pinned the repository.  A mutation is never allowed to replace an
/// arbitrary graph node merely because it happened to run under a mutation
/// capability.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum GraphTransition {
    ReadOnly,
    StatusRefresh,
    Stage,
    Commit,
    Reset,
    Branch,
    Fetch,
    Pull,
    Push,
}

impl GraphTransition {
    fn allows_replacement(
        self,
        git_dir: &Path,
        common_dir: Option<&Path>,
        object_stores: &[PathBuf],
        metadata_roots: &[PathBuf],
        path: &Path,
    ) -> bool {
        if self == Self::ReadOnly {
            return false;
        }
        if same_path(path, git_dir) || common_dir.is_some_and(|root| same_path(path, root)) {
            return true;
        }
        if let Some(object_store) = object_stores
            .iter()
            .find(|object_store| is_within(object_store, path))
        {
            return self.allows_object_path(object_store, path);
        }

        let state_root = common_dir.unwrap_or(git_dir);
        for refs_root in [state_root.join("refs"), git_dir.join("refs")] {
            if is_within(&refs_root, path) {
                return self.allows_ref_path(&refs_root, path, false);
            }
        }
        for logs_root in [state_root.join("logs"), git_dir.join("logs")] {
            if is_within(&logs_root, path) {
                return self.allows_ref_path(&logs_root, path, true);
            }
        }
        if let Some(metadata_root) = metadata_roots
            .iter()
            .find(|metadata_root| is_within(metadata_root, path))
        {
            return self.allows_worktree_metadata(metadata_root, path);
        }
        self.allows_state_file(git_dir, common_dir, path)
    }

    fn allows_object_path(self, object_store: &Path, path: &Path) -> bool {
        if !matches!(self, Self::Stage | Self::Commit | Self::Fetch | Self::Pull) {
            return false;
        }
        let Ok(relative) = path.strip_prefix(object_store) else {
            return false;
        };
        let components = relative
            .components()
            .filter_map(|component| match component {
                std::path::Component::Normal(name) => Some(name),
                _ => None,
            })
            .collect::<Vec<_>>();
        if components.is_empty() {
            return true;
        }
        if components[0] == OsStr::new("pack") {
            if components.len() == 1 {
                return true;
            }
            if components.len() != 2 {
                return false;
            }
            let name = components[1].to_string_lossy();
            return name.starts_with("tmp_")
                || name.starts_with("pack-")
                    && ["pack", "idx", "rev", "keep", "bitmap", "promisor"]
                        .iter()
                        .any(|extension| name.ends_with(&format!(".{extension}")))
                || name.ends_with(".lock");
        }
        if components[0] == OsStr::new("info") {
            if components.len() == 1 {
                return true;
            }
            if components.len() != 2 {
                return false;
            }
            return matches!(
                components[1].to_str(),
                Some("packs")
                    | Some("commit-graph")
                    | Some("commit-graph-chain")
                    | Some("multi-pack-index")
                    | Some("multi-pack-index-chain")
                    | Some("commit-graph.lock")
                    | Some("commit-graph-chain.lock")
                    | Some("multi-pack-index.lock")
                    | Some("multi-pack-index-chain.lock")
            );
        }
        if components.len() != 2 {
            return components.len() == 1
                && components[0].to_string_lossy().len() == 2
                && components[0]
                    .to_string_lossy()
                    .bytes()
                    .all(|byte| byte.is_ascii_hexdigit());
        }
        let fanout = components[0].to_string_lossy();
        let name = components[1].to_string_lossy();
        if fanout.len() != 2 || !fanout.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return false;
        }
        name.starts_with("tmp_obj_")
            || (name.len() == 38 && name.bytes().all(|byte| byte.is_ascii_hexdigit()))
            || name.ends_with(".lock")
    }

    fn allows_ref_path(self, root: &Path, path: &Path, logs_root: bool) -> bool {
        if !matches!(
            self,
            Self::Commit | Self::Reset | Self::Branch | Self::Fetch | Self::Pull
        ) {
            return false;
        }
        let Ok(relative) = path.strip_prefix(root) else {
            return false;
        };
        let components = relative
            .components()
            .filter_map(|component| match component {
                std::path::Component::Normal(name) => Some(name),
                _ => None,
            })
            .collect::<Vec<_>>();
        if components.is_empty() {
            return self != Self::Reset || logs_root;
        }
        if logs_root && components.len() == 1 {
            let name = components[0].to_string_lossy();
            if name.strip_suffix(".lock").unwrap_or(&name) == "HEAD" {
                return true;
            }
        }
        let components = if logs_root && components[0] == OsStr::new("refs") {
            &components[1..]
        } else if logs_root {
            return false;
        } else {
            &components[..]
        };
        if components.is_empty() {
            return false;
        }
        let last = components.last().expect("nonempty ref path");
        let last = last.to_string_lossy();
        let last = last.strip_suffix(".lock").unwrap_or(&last);
        !last.is_empty()
            && components[..components.len() - 1]
                .iter()
                .all(|component| valid_git_ref_component(component))
            && valid_git_ref_component(OsStr::new(last))
    }

    fn allows_worktree_metadata(self, root: &Path, path: &Path) -> bool {
        if !matches!(
            self,
            Self::StatusRefresh
                | Self::Stage
                | Self::Commit
                | Self::Reset
                | Self::Branch
                | Self::Fetch
                | Self::Pull
        ) {
            return false;
        }
        let Ok(relative) = path.strip_prefix(root) else {
            return false;
        };
        let components = relative
            .components()
            .filter_map(|component| match component {
                std::path::Component::Normal(name) => Some(name),
                _ => None,
            })
            .collect::<Vec<_>>();
        if components.is_empty() {
            return true;
        }
        if components.len() == 1 {
            let Some(name) = components[0].to_str() else {
                return false;
            };
            let name = name.strip_suffix(".lock").unwrap_or(name);
            return match name {
                "HEAD" => matches!(self, Self::Commit | Self::Reset | Self::Branch | Self::Pull),
                "index" => matches!(
                    self,
                    Self::StatusRefresh | Self::Stage | Self::Commit | Self::Reset | Self::Pull
                ),
                "ORIG_HEAD" => matches!(self, Self::Commit | Self::Reset | Self::Pull),
                "logs" | "refs" => {
                    matches!(
                        self,
                        Self::Commit | Self::Reset | Self::Branch | Self::Fetch | Self::Pull
                    )
                }
                _ => false,
            };
        }
        if !valid_git_ref_component(components[0]) {
            return false;
        }
        if components.len() == 2 {
            let Some(name) = components[1].to_str() else {
                return false;
            };
            let name = name.strip_suffix(".lock").unwrap_or(name);
            return match name {
                "HEAD" => matches!(
                    self,
                    Self::StatusRefresh | Self::Commit | Self::Reset | Self::Branch | Self::Pull
                ),
                "index" => matches!(
                    self,
                    Self::StatusRefresh | Self::Stage | Self::Commit | Self::Reset | Self::Pull
                ),
                "ORIG_HEAD" => matches!(self, Self::Commit | Self::Reset | Self::Pull),
                "logs" | "refs" => {
                    matches!(
                        self,
                        Self::Commit | Self::Reset | Self::Branch | Self::Fetch | Self::Pull
                    )
                }
                _ => false,
            };
        }
        match components[1].to_str() {
            Some("logs") | Some("refs") => {
                if !matches!(
                    self,
                    Self::Commit | Self::Reset | Self::Branch | Self::Fetch | Self::Pull
                ) {
                    return false;
                }
                components[2..]
                    .iter()
                    .all(|component| valid_git_ref_component(component))
            }
            _ => false,
        }
    }

    fn allows_state_file(self, git_dir: &Path, common_dir: Option<&Path>, path: &Path) -> bool {
        let roots = [Some(git_dir), common_dir];
        roots.into_iter().flatten().any(|root| {
            let Ok(relative) = path.strip_prefix(root) else {
                return false;
            };
            let components = relative.components().collect::<Vec<_>>();
            if components.len() != 1 {
                return false;
            }
            let Some(name) = components[0].as_os_str().to_str() else {
                return false;
            };
            let name = name.strip_suffix(".lock").unwrap_or(name);
            match name {
                "index" => matches!(
                    self,
                    Self::StatusRefresh | Self::Stage | Self::Commit | Self::Reset | Self::Pull
                ),
                "HEAD" => matches!(self, Self::Commit | Self::Reset | Self::Branch | Self::Pull),
                "FETCH_HEAD" => matches!(self, Self::Fetch | Self::Pull),
                "MERGE_HEAD" | "ORIG_HEAD" | "CHERRY_PICK_HEAD" | "REVERT_HEAD" => {
                    matches!(self, Self::Commit | Self::Reset | Self::Pull)
                }
                "packed-refs" | "shallow" => {
                    matches!(self, Self::Commit | Self::Branch | Self::Fetch | Self::Pull)
                }
                "COMMIT_EDITMSG" | "MERGE_MSG" | "SQUASH_MSG" => {
                    matches!(self, Self::Commit | Self::Pull)
                }
                _ if name.starts_with("BISECT_") => matches!(self, Self::Commit | Self::Pull),
                _ => false,
            }
        })
    }
}

fn valid_git_ref_component(component: &OsStr) -> bool {
    let Some(component) = component.to_str() else {
        return false;
    };
    !component.is_empty()
        && component != "."
        && component != ".."
        && !component.starts_with('.')
        && !component.ends_with('.')
        && !component.contains("..")
        && !component.contains("@{")
        && !component
            .chars()
            .any(|character| matches!(character, ' ' | '~' | '^' | ':' | '?' | '*' | '[' | '\\'))
}

#[derive(Clone)]
struct GraphNode {
    path: PathBuf,
    identity: FileIdentity,
    is_file: bool,
    mutable: bool,
    mutable_recursive: bool,
    /// Static alternate roots bind their bounded recursive content as well as
    /// their file identity, so an unenumerated pack/object insertion cannot
    /// appear between admission and the child checks.
    content_bound: bool,
}

fn is_mutable_directory_snapshot_root(nodes: &[GraphNode], candidate: &GraphNode) -> bool {
    if !candidate.mutable_recursive {
        return true;
    }
    !nodes.iter().any(|other| {
        other.mutable
            && !other.is_file
            && other.mutable_recursive
            && !same_path(&other.path, &candidate.path)
            && is_within(&other.path, &candidate.path)
    })
}

/// Mutable Git inputs such as `index` and `packed-refs` are legitimately
/// absent in a freshly initialized repository.  Keep an explicit absence
/// baseline so an attacker cannot create one between host admission and a
/// read, while an authorized mutation can adopt the newly created identity.
#[derive(Clone)]
struct MutableGraphInput {
    path: PathBuf,
    is_file: bool,
    initial_identity: Option<FileIdentity>,
}

type MutableDirectorySnapshot = Vec<(OsString, bool, FileIdentity)>;

impl RepositoryGraph {
    fn open(root: &Path, approved_external_roots: &[PathBuf]) -> Result<Self, String> {
        Self::open_with_deadline(
            root,
            approved_external_roots,
            OperationDeadline::from_now(HARD_MAX_TIMEOUT),
        )
    }

    fn open_with_deadline(
        root: &Path,
        approved_external_roots: &[PathBuf],
        deadline: OperationDeadline,
    ) -> Result<Self, String> {
        check_graph_deadline(deadline)?;
        let approved_external_roots = canonicalize_approved_graph_roots_with_deadline(
            root,
            approved_external_roots,
            deadline,
        )?;
        check_graph_deadline(deadline)?;
        let git_entry = root.join(".git");
        reject_reparse_components(&git_entry)?;
        check_graph_deadline(deadline)?;
        let metadata = fs::symlink_metadata(&git_entry)
            .map_err(|_| "repository Git directory is unavailable".to_string())?;
        let gitdir = if metadata.is_dir() {
            check_graph_deadline(deadline)?;
            fs::canonicalize(&git_entry)
                .map_err(|_| "repository Git directory cannot be canonicalized".to_string())?
        } else if metadata.is_file() {
            check_graph_deadline(deadline)?;
            let descriptor = String::from_utf8(
                read_file_bounded_with_deadline(&git_entry, HARD_MAX_STDERR_BYTES, Some(deadline))
                    .map_err(|_| {
                        "repository Git directory descriptor cannot be read".to_string()
                    })?,
            )
            .map_err(|_| "repository Git directory descriptor is not UTF-8".to_string())?;
            let value = descriptor
                .trim()
                .strip_prefix("gitdir:")
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .ok_or_else(|| "repository Git directory descriptor is invalid".to_string())?;
            let requested = PathBuf::from(value);
            let requested = if requested.is_absolute() {
                requested
            } else {
                root.join(requested)
            };
            check_graph_deadline(deadline)?;
            fs::canonicalize(requested)
                .map_err(|_| "repository Git directory cannot be canonicalized".to_string())?
        } else {
            return Err("repository Git directory is not a directory or descriptor".to_string());
        };
        if !graph_path_allowed(root, &gitdir, &approved_external_roots) {
            return Err("repository Git directory is external to the workspace".to_string());
        }

        let mut nodes = Vec::new();
        let mut object_stores = Vec::new();
        // Keep the primary object store separate from alternates.  The
        // primary store is mutable under the exact Git transition allow-list;
        // alternates remain static, but all stores still participate in the
        // bounded admission graph (pack, node, and byte caps).
        let mut graph_object_stores = Vec::new();
        let mut metadata_roots = Vec::new();
        push_graph_node(
            root,
            &git_entry,
            metadata.is_file(),
            &approved_external_roots,
            &mut nodes,
            deadline,
        )?;
        push_shallow_mutable_graph_node(
            root,
            &gitdir,
            false,
            &approved_external_roots,
            &mut nodes,
            deadline,
        )?;

        let commondir_file = gitdir.join("commondir");
        check_graph_deadline(deadline)?;
        let commondir = if commondir_file.is_file() {
            check_graph_deadline(deadline)?;
            push_graph_node(
                root,
                &commondir_file,
                true,
                &approved_external_roots,
                &mut nodes,
                deadline,
            )?;
            let descriptor = String::from_utf8(
                read_file_bounded_with_deadline(
                    &commondir_file,
                    HARD_MAX_STDERR_BYTES,
                    Some(deadline),
                )
                .map_err(|_| "repository common directory descriptor cannot be read".to_string())?,
            )
            .map_err(|_| "repository common directory descriptor is not UTF-8".to_string())?;
            let value = descriptor.trim();
            if value.is_empty() || value.contains('\n') || value.contains('\r') {
                return Err("repository common directory descriptor is invalid".to_string());
            }
            let requested = PathBuf::from(value);
            let requested = if requested.is_absolute() {
                requested
            } else {
                gitdir.join(requested)
            };
            check_graph_deadline(deadline)?;
            let canonical = fs::canonicalize(requested)
                .map_err(|_| "repository common directory cannot be canonicalized".to_string())?;
            if !graph_path_allowed(root, &canonical, &approved_external_roots) {
                return Err("repository common directory is external to the workspace".to_string());
            }
            push_shallow_mutable_graph_node(
                root,
                &canonical,
                false,
                &approved_external_roots,
                &mut nodes,
                deadline,
            )?;
            Some(canonical)
        } else {
            // The absence of the descriptor is part of the pinned graph. A
            // linked-worktree descriptor appearing later must be admitted by
            // an explicit host transition, never silently by a read.
            None
        };

        let mut optional_static_inputs = Vec::new();
        if commondir.is_none() {
            push_optional_static_input(
                root,
                &commondir_file,
                true,
                &approved_external_roots,
                &mut optional_static_inputs,
                deadline,
            )?;
        }

        let object_store = if let Some(common) = &commondir {
            common.join("objects")
        } else {
            gitdir.join("objects")
        };
        if !graph_path_allowed(root, &object_store, &approved_external_roots) {
            return Err("repository object store is external to the workspace".to_string());
        }
        push_mutable_graph_node(
            root,
            &object_store,
            false,
            &approved_external_roots,
            &mut nodes,
            deadline,
        )?;
        object_stores.push(object_store.clone());
        graph_object_stores.push(object_store.clone());

        let alternates = object_store.join("info").join("alternates");
        if alternates.is_file() {
            check_graph_deadline(deadline)?;
            push_graph_node(
                root,
                &alternates,
                true,
                &approved_external_roots,
                &mut nodes,
                deadline,
            )?;
            let contents = String::from_utf8(
                read_file_bounded_with_deadline(&alternates, HARD_MAX_STDERR_BYTES, Some(deadline))
                    .map_err(|_| "Git alternates file cannot be read safely".to_string())?,
            )
            .map_err(|_| "Git alternates file is not UTF-8".to_string())?;
            for (alternate_index, line) in contents
                .lines()
                .map(str::trim)
                .filter(|line| !line.is_empty())
                .enumerate()
            {
                check_graph_deadline(deadline)?;
                if alternate_index >= HARD_MAX_ALTERNATES {
                    return Err(format!(
                        "Git alternates exceed the {HARD_MAX_ALTERNATES}-entry limit"
                    ));
                }
                let requested = PathBuf::from(line);
                let requested = if requested.is_absolute() {
                    requested
                } else {
                    object_store.join(requested)
                };
                check_graph_deadline(deadline)?;
                let canonical = fs::canonicalize(requested).map_err(|_| {
                    "Git alternate object store cannot be resolved safely".to_string()
                })?;
                if !graph_path_allowed(root, &canonical, &approved_external_roots) {
                    return Err(
                        "Git alternate object store is external to the workspace".to_string()
                    );
                }
                push_graph_node(
                    root,
                    &canonical,
                    false,
                    &approved_external_roots,
                    &mut nodes,
                    deadline,
                )?;
                mark_static_graph_root(
                    root,
                    &canonical,
                    &approved_external_roots,
                    &mut nodes,
                    deadline,
                )?;
                if !graph_object_stores
                    .iter()
                    .any(|existing| same_path(existing, &canonical))
                {
                    graph_object_stores.push(canonical.clone());
                }
                push_static_graph_descendants(
                    root,
                    &canonical,
                    &approved_external_roots,
                    &mut nodes,
                    deadline,
                    0,
                )?;
            }
        } else {
            push_optional_static_input(
                root,
                &alternates,
                true,
                &approved_external_roots,
                &mut optional_static_inputs,
                deadline,
            )?;
        }

        for config in [gitdir.join("config"), gitdir.join("config.worktree")] {
            check_graph_deadline(deadline)?;
            if config.is_file() {
                push_graph_node(
                    root,
                    &config,
                    true,
                    &approved_external_roots,
                    &mut nodes,
                    deadline,
                )?;
                reject_unsafe_local_config(&config, Some(deadline))?;
            } else {
                push_optional_static_input(
                    root,
                    &config,
                    true,
                    &approved_external_roots,
                    &mut optional_static_inputs,
                    deadline,
                )?;
            }
        }
        if let Some(common) = &commondir {
            check_graph_deadline(deadline)?;
            let config = common.join("config");
            if config.is_file() {
                push_graph_node(
                    root,
                    &config,
                    true,
                    &approved_external_roots,
                    &mut nodes,
                    deadline,
                )?;
                reject_unsafe_local_config(&config, Some(deadline))?;
            } else {
                push_optional_static_input(
                    root,
                    &config,
                    true,
                    &approved_external_roots,
                    &mut optional_static_inputs,
                    deadline,
                )?;
            }
        }

        let mut optional_mutable_inputs = Vec::new();
        let worktrees = commondir.as_ref().unwrap_or(&gitdir).join("worktrees");
        check_graph_deadline(deadline)?;
        if fs::symlink_metadata(&worktrees)
            .ok()
            .is_some_and(|metadata| metadata.is_dir())
        {
            push_worktree_graph(
                root,
                &worktrees,
                commondir.as_deref().unwrap_or(&gitdir),
                &approved_external_roots,
                &mut nodes,
                &mut optional_mutable_inputs,
                deadline,
            )?;
        } else {
            push_mutable_graph_input(
                root,
                &worktrees,
                false,
                &approved_external_roots,
                &mut nodes,
                &mut optional_mutable_inputs,
                deadline,
            )?;
        }
        // The root itself is absence-baselined even when no linked worktree
        // exists at admission.  Authorized worktree transitions may adopt a
        // newly created root, while reads reject it as a graph substitution.
        metadata_roots.push(worktrees);

        // These are the mutable Git inputs that affect preview and mutation
        // semantics.  They are snapshotted as graph nodes so a read-only
        // operation rejects substitution, while an explicitly mutating
        // operation may change their contents but must preserve containment,
        // regular-file identity rules, and the mutable directory listing.
        for input in [
            gitdir.join("HEAD"),
            gitdir.join("index"),
            commondir.as_ref().unwrap_or(&gitdir).join("packed-refs"),
        ] {
            check_graph_deadline(deadline)?;
            push_mutable_graph_input(
                root,
                &input,
                true,
                &approved_external_roots,
                &mut nodes,
                &mut optional_mutable_inputs,
                deadline,
            )?;
        }
        let refs_dir = commondir.as_ref().unwrap_or(&gitdir).join("refs");
        check_graph_deadline(deadline)?;
        push_mutable_graph_input(
            root,
            &refs_dir,
            false,
            &approved_external_roots,
            &mut nodes,
            &mut optional_mutable_inputs,
            deadline,
        )?;
        let logs_dir = commondir.as_ref().unwrap_or(&gitdir).join("logs");
        check_graph_deadline(deadline)?;
        push_mutable_graph_input(
            root,
            &logs_dir,
            false,
            &approved_external_roots,
            &mut nodes,
            &mut optional_mutable_inputs,
            deadline,
        )?;
        for (input, is_file) in [
            (gitdir.join("MERGE_HEAD"), true),
            (gitdir.join("rebase-merge"), false),
            (gitdir.join("rebase-apply"), false),
        ] {
            check_graph_deadline(deadline)?;
            push_mutable_graph_input(
                root,
                &input,
                is_file,
                &approved_external_roots,
                &mut nodes,
                &mut optional_mutable_inputs,
                deadline,
            )?;
        }

        // Directory enumeration order is not an authority identity. Sort the
        // admitted graph before computing baselines/identities so a fresh
        // reopen binds the same repository even when the filesystem returns
        // children in a different order.
        nodes.sort_by(|left, right| left.path.as_os_str().cmp(right.path.as_os_str()));
        optional_static_inputs
            .sort_by(|left, right| left.path.as_os_str().cmp(right.path.as_os_str()));
        optional_mutable_inputs
            .sort_by(|left, right| left.path.as_os_str().cmp(right.path.as_os_str()));

        let mutable_baseline = nodes
            .iter()
            .filter(|node| node.mutable)
            .map(|node| (node.path.clone(), node.identity.clone()))
            .collect();
        let optional_mutable_baseline = optional_mutable_inputs
            .iter()
            .map(|input| (input.path.clone(), input.initial_identity.clone()))
            .collect();
        let optional_static_baseline = optional_static_inputs
            .iter()
            .map(|input| (input.path.clone(), input.initial_identity.clone()))
            .collect();
        let mut mutable_entry_baseline = std::collections::HashMap::new();
        for node in nodes
            .iter()
            .filter(|node| node.mutable && !node.is_file)
            .filter(|node| is_mutable_directory_snapshot_root(&nodes, node))
        {
            check_graph_deadline(deadline)?;
            mutable_entry_baseline.insert(
                node.path.clone(),
                mutable_directory_snapshot_for_node(node, Some(deadline))?,
            );
        }
        let optional_mutable_entry_baseline = optional_mutable_inputs
            .iter()
            .filter(|input| !input.is_file)
            .map(|input| (input.path.clone(), Vec::new()))
            .collect();
        enforce_graph_limits(
            root,
            &graph_object_stores,
            &nodes,
            &gitdir,
            commondir.as_deref(),
            &metadata_roots,
            &approved_external_roots,
            deadline,
        )?;
        check_graph_deadline(deadline)?;
        for approved in &approved_external_roots {
            check_graph_deadline(deadline)?;
            if !nodes.iter().any(|node| is_within(approved, &node.path)) {
                return Err(
                    "approved external Git graph root is not referenced by repository descriptors"
                        .to_string(),
                );
            }
        }
        check_graph_deadline(deadline)?;
        Ok(Self {
            nodes,
            git_dir: gitdir,
            common_dir: commondir,
            object_stores,
            metadata_roots,
            approved_external_roots,
            mutable_baseline: Arc::new(Mutex::new(mutable_baseline)),
            mutable_entry_baseline: Arc::new(Mutex::new(mutable_entry_baseline)),
            optional_static_inputs,
            optional_static_baseline: Arc::new(Mutex::new(optional_static_baseline)),
            optional_mutable_inputs,
            optional_mutable_baseline: Arc::new(Mutex::new(optional_mutable_baseline)),
            optional_mutable_entry_baseline: Arc::new(Mutex::new(optional_mutable_entry_baseline)),
        })
    }

    fn revalidate(&self) -> Result<(), String> {
        self.revalidate_with_deadline(None)
    }

    fn revalidate_with_deadline(&self, deadline: Option<OperationDeadline>) -> Result<(), String> {
        self.revalidate_nodes(GraphTransition::ReadOnly, deadline, false)
    }

    fn revalidate_after_transition(&self, transition: GraphTransition) -> Result<(), String> {
        self.revalidate_after_transition_with_deadline(transition, None)
    }

    fn revalidate_after_transition_with_deadline(
        &self,
        transition: GraphTransition,
        deadline: Option<OperationDeadline>,
    ) -> Result<(), String> {
        self.revalidate_nodes(transition, deadline, true)
    }

    fn revalidate_during_transition_with_deadline(
        &self,
        transition: GraphTransition,
        deadline: Option<OperationDeadline>,
    ) -> Result<(), String> {
        self.revalidate_nodes(transition, deadline, false)
    }

    fn revalidate_nodes(
        &self,
        transition: GraphTransition,
        deadline: Option<OperationDeadline>,
        update_baseline: bool,
    ) -> Result<(), String> {
        // A full content snapshot is mandatory for read admission and for the
        // post-effect proof that adopts a legitimate mutation.  While the
        // child is running, retain the same no-follow/type/container checks
        // but defer recursive object/worktree content enumeration to the
        // post-effect proof.  This keeps the one bounded operation budget
        // usable for short Git commands without weakening the before/after
        // identity contract.
        let full_content = update_baseline
            || matches!(
                transition,
                GraphTransition::ReadOnly | GraphTransition::StatusRefresh
            );
        if deadline.is_some_and(OperationDeadline::is_expired) {
            return Err("repository validation exceeded the operation deadline".to_string());
        }
        let baseline = self
            .mutable_baseline
            .lock()
            .map_err(|_| "repository mutable graph baseline is poisoned".to_string())?
            .clone();
        let entry_baseline = self
            .mutable_entry_baseline
            .lock()
            .map_err(|_| "repository mutable graph entry baseline is poisoned".to_string())?
            .clone();
        let mut current_mutable = Vec::new();
        let mut current_mutable_entries = Vec::new();
        for node in &self.nodes {
            if deadline.is_some_and(OperationDeadline::is_expired) {
                return Err("repository validation exceeded the operation deadline".to_string());
            }
            reject_reparse_components(&node.path)?;
            let metadata = fs::symlink_metadata(&node.path)
                .map_err(|_| "repository graph path changed".to_string())?;
            if node.is_file != metadata.is_file() {
                return Err("repository graph entry type changed".to_string());
            }
            let current_entries = if node.mutable
                && !node.is_file
                && is_mutable_directory_snapshot_root(&self.nodes, node)
                && (full_content || !node.mutable_recursive)
            {
                Some(mutable_directory_snapshot_for_node(node, deadline)?)
            } else {
                None
            };
            let identity = if node.is_file {
                deadline.map_or_else(
                    || data_file_identity(&node.path),
                    |deadline| data_file_identity_with_deadline(&node.path, deadline),
                )?
            } else if let Some(entries) = &current_entries {
                let mut identity = directory_identity_with_deadline(&node.path, deadline)?;
                if !node.mutable_recursive {
                    identity.content_digest.copy_from_slice(&[0; 32]);
                    identity
                } else {
                    let mut hasher = Sha256::new();
                    for (relative, is_file, entry_identity) in entries {
                        update_os_string_digest(&mut hasher, relative);
                        hasher.update([*is_file as u8]);
                        hasher.update(identity_token(entry_identity).as_bytes());
                    }
                    identity.content_digest.copy_from_slice(&hasher.finalize());
                    identity
                }
            } else if node.content_bound || (node.mutable_recursive && full_content) {
                mutable_directory_identity_with_deadline(&node.path, deadline)?
            } else {
                directory_identity_with_deadline(&node.path, deadline)?
            };
            if node.mutable {
                if !transition.allows_replacement(
                    &self.git_dir,
                    self.common_dir.as_deref(),
                    &self.object_stores,
                    &self.metadata_roots,
                    &node.path,
                ) {
                    let expected = baseline.get(&node.path).unwrap_or(&node.identity);
                    if !graph_identity_matches(expected, &identity) {
                        return Err(format!(
                            "repository graph mutable entry was substituted: {}",
                            node.path.display()
                        ));
                    }
                } else if !node.is_file {
                    let expected = baseline.get(&node.path).unwrap_or(&node.identity);
                    if !graph_container_identity_matches(expected, &identity) {
                        return Err(format!(
                            "repository graph mutable container was substituted: {}",
                            node.path.display()
                        ));
                    }
                }
                if let Some(current_entries) = current_entries {
                    let expected_entries =
                        entry_baseline.get(&node.path).cloned().unwrap_or_default();
                    validate_mutable_directory_snapshot(
                        transition,
                        &self.git_dir,
                        self.common_dir.as_deref(),
                        &self.object_stores,
                        &self.metadata_roots,
                        &node.path,
                        &expected_entries,
                        &current_entries,
                    )?;
                    current_mutable_entries.push((node.path.clone(), current_entries));
                }
                current_mutable.push((node.path.clone(), identity));
            } else if identity != node.identity {
                return Err(format!(
                    "repository graph file identity changed: {}",
                    node.path.display()
                ));
            }
        }
        self.revalidate_optional_mutable_inputs(
            transition,
            deadline,
            update_baseline,
            full_content,
        )?;
        self.revalidate_optional_static_inputs(deadline)?;
        if deadline.is_some_and(OperationDeadline::is_expired) {
            return Err("repository validation exceeded the operation deadline".to_string());
        }
        if update_baseline {
            let mut baseline = self
                .mutable_baseline
                .lock()
                .map_err(|_| "repository mutable graph baseline is poisoned".to_string())?;
            for (path, identity) in current_mutable {
                baseline.insert(path, identity);
            }
            let mut entry_baseline = self
                .mutable_entry_baseline
                .lock()
                .map_err(|_| "repository mutable graph entry baseline is poisoned".to_string())?;
            for (path, entries) in current_mutable_entries {
                entry_baseline.insert(path, entries);
            }
        }
        if deadline.is_some_and(OperationDeadline::is_expired) {
            return Err("repository validation exceeded the operation deadline".to_string());
        }
        Ok(())
    }

    fn revalidate_optional_static_inputs(
        &self,
        deadline: Option<OperationDeadline>,
    ) -> Result<(), String> {
        let baseline = self
            .optional_static_baseline
            .lock()
            .map_err(|_| "repository optional static graph baseline is poisoned".to_string())?
            .clone();
        for input in &self.optional_static_inputs {
            if deadline.is_some_and(OperationDeadline::is_expired) {
                return Err("repository validation exceeded the operation deadline".to_string());
            }
            let actual =
                optional_mutable_identity_with_deadline(&input.path, input.is_file, deadline)?;
            let expected = baseline
                .get(&input.path)
                .cloned()
                .unwrap_or_else(|| input.initial_identity.clone());
            if actual != expected {
                return Err(format!(
                    "repository graph static entry was substituted: {}",
                    input.path.display()
                ));
            }
        }
        Ok(())
    }

    fn revalidate_optional_mutable_inputs(
        &self,
        transition: GraphTransition,
        deadline: Option<OperationDeadline>,
        update_baseline: bool,
        full_content: bool,
    ) -> Result<(), String> {
        let baseline = self
            .optional_mutable_baseline
            .lock()
            .map_err(|_| "repository optional mutable graph baseline is poisoned".to_string())?
            .clone();
        let entry_baseline = self
            .optional_mutable_entry_baseline
            .lock()
            .map_err(|_| {
                "repository optional mutable graph entry baseline is poisoned".to_string()
            })?
            .clone();
        let mut current = Vec::new();
        let mut current_entries = Vec::new();
        for input in &self.optional_mutable_inputs {
            if deadline.is_some_and(OperationDeadline::is_expired) {
                return Err("repository validation exceeded the operation deadline".to_string());
            }
            let actual = if !input.is_file && !full_content {
                optional_mutable_container_identity_with_deadline(&input.path, deadline)?
            } else {
                optional_mutable_identity_with_deadline(&input.path, input.is_file, deadline)?
            };
            let expected = baseline
                .get(&input.path)
                .cloned()
                .unwrap_or_else(|| input.initial_identity.clone());
            if !transition.allows_replacement(
                &self.git_dir,
                self.common_dir.as_deref(),
                &self.object_stores,
                &self.metadata_roots,
                &input.path,
            ) {
                match (&expected, &actual) {
                    (None, None) => {}
                    (Some(expected), Some(actual)) if graph_identity_matches(expected, actual) => {}
                    _ => {
                        return Err(format!(
                            "repository graph mutable entry was substituted: {}",
                            input.path.display()
                        ));
                    }
                }
            } else if !input.is_file {
                if let (Some(expected), Some(actual)) = (&expected, &actual) {
                    if !graph_container_identity_matches(expected, actual) {
                        return Err(format!(
                            "repository graph mutable container was substituted: {}",
                            input.path.display()
                        ));
                    }
                }
                if actual.is_some() && (full_content || input.is_file) {
                    let actual_entries =
                        mutable_directory_snapshot_with_deadline(&input.path, deadline)?;
                    let expected_entries =
                        entry_baseline.get(&input.path).cloned().unwrap_or_default();
                    validate_mutable_directory_snapshot(
                        transition,
                        &self.git_dir,
                        self.common_dir.as_deref(),
                        &self.object_stores,
                        &self.metadata_roots,
                        &input.path,
                        &expected_entries,
                        &actual_entries,
                    )?;
                    current_entries.push((input.path.clone(), actual_entries));
                }
            }
            current.push((input.path.clone(), actual));
        }
        if deadline.is_some_and(OperationDeadline::is_expired) {
            return Err("repository validation exceeded the operation deadline".to_string());
        }
        if update_baseline {
            let mut baseline = self.optional_mutable_baseline.lock().map_err(|_| {
                "repository optional mutable graph baseline is poisoned".to_string()
            })?;
            for (path, identity) in current {
                baseline.insert(path, identity);
            }
            let mut entry_baseline = self.optional_mutable_entry_baseline.lock().map_err(|_| {
                "repository optional mutable graph entry baseline is poisoned".to_string()
            })?;
            for (path, entries) in current_entries {
                entry_baseline.insert(path, entries);
            }
        }
        Ok(())
    }

    fn state_exists(&self, root: &Path, relative: &str) -> Result<bool, String> {
        self.state_exists_with_deadline(root, relative, None)
    }

    fn state_exists_with_deadline(
        &self,
        root: &Path,
        relative: &str,
        deadline: Option<OperationDeadline>,
    ) -> Result<bool, String> {
        if deadline.is_some_and(OperationDeadline::is_expired) {
            return Err("repository state validation exceeded the operation deadline".to_string());
        }
        let candidate = self.git_dir.join(relative);
        reject_reparse_components(&candidate)?;
        if deadline.is_some_and(OperationDeadline::is_expired) {
            return Err("repository state validation exceeded the operation deadline".to_string());
        }
        let metadata = match fs::symlink_metadata(&candidate) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(_) => return Err("repository state path metadata is unavailable".to_string()),
        };
        let canonical = fs::canonicalize(&candidate)
            .map_err(|_| "repository state path cannot be canonicalized".to_string())?;
        if deadline.is_some_and(OperationDeadline::is_expired) {
            return Err("repository state validation exceeded the operation deadline".to_string());
        }
        if !graph_path_allowed(root, &canonical, &self.approved_external_roots) {
            return Err("repository state path is external to the workspace".to_string());
        }
        if metadata.is_file() {
            if let Some(deadline) = deadline {
                data_file_identity_with_deadline(&canonical, deadline)?;
            } else {
                data_file_identity(&canonical)?;
            }
        } else if metadata.is_dir() {
            if let Some(deadline) = deadline {
                mutable_directory_identity_with_deadline(&canonical, Some(deadline))?;
            } else {
                directory_identity_with_deadline(&canonical, deadline)?;
            }
        } else {
            return Err("repository state path is not a regular file or directory".to_string());
        }
        if deadline.is_some_and(OperationDeadline::is_expired) {
            return Err("repository state validation exceeded the operation deadline".to_string());
        }
        Ok(true)
    }
}

fn validate_mutable_directory_snapshot(
    transition: GraphTransition,
    root: &Path,
    common_dir: Option<&Path>,
    object_stores: &[PathBuf],
    metadata_roots: &[PathBuf],
    directory: &Path,
    expected: &MutableDirectorySnapshot,
    actual: &MutableDirectorySnapshot,
) -> Result<(), String> {
    let mut expected_index = 0;
    let mut actual_index = 0;
    while expected_index < expected.len() || actual_index < actual.len() {
        let expected_entry = expected.get(expected_index);
        let actual_entry = actual.get(actual_index);
        let ordering = match (expected_entry, actual_entry) {
            (Some(expected_entry), Some(actual_entry)) => {
                expected_entry.0.as_os_str().cmp(actual_entry.0.as_os_str())
            }
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => break,
        };
        let (relative, expected_identity, actual_identity) = match ordering {
            std::cmp::Ordering::Less => {
                let expected_entry = expected_entry.expect("expected entry is present");
                expected_index += 1;
                (expected_entry.0.clone(), Some(expected_entry), None)
            }
            std::cmp::Ordering::Equal => {
                let expected_entry = expected_entry.expect("expected entry is present");
                let actual_entry = actual_entry.expect("actual entry is present");
                expected_index += 1;
                actual_index += 1;
                (
                    expected_entry.0.clone(),
                    Some(expected_entry),
                    Some(actual_entry),
                )
            }
            std::cmp::Ordering::Greater => {
                let actual_entry = actual_entry.expect("actual entry is present");
                actual_index += 1;
                (actual_entry.0.clone(), None, Some(actual_entry))
            }
        };
        let changed = match (expected_identity, actual_identity) {
            (Some(expected), Some(actual)) => expected.1 != actual.1 || expected.2 != actual.2,
            (None, Some(_)) | (Some(_), None) => true,
            (None, None) => false,
        };
        if !changed {
            continue;
        }
        let changed_path = directory.join(&relative);
        let object_path = object_stores
            .iter()
            .find(|object_store| is_within(object_store, &changed_path));
        if object_path.is_some() && expected_identity.is_some() {
            return Err(format!(
                "repository object content was removed or replaced during Git operation: {}",
                changed_path.display()
            ));
        }
        if !transition.allows_replacement(
            root,
            common_dir,
            object_stores,
            metadata_roots,
            &changed_path,
        ) {
            return Err(format!(
                "repository graph mutation is outside the operation transition: {}",
                changed_path.display()
            ));
        }
    }
    Ok(())
}

fn push_graph_node(
    root: &Path,
    path: &Path,
    is_file: bool,
    approved_external_roots: &[PathBuf],
    nodes: &mut Vec<GraphNode>,
    deadline: OperationDeadline,
) -> Result<(), String> {
    push_graph_node_with_mode(
        root,
        path,
        is_file,
        false,
        approved_external_roots,
        nodes,
        deadline,
    )
}

fn push_mutable_graph_node(
    root: &Path,
    path: &Path,
    is_file: bool,
    approved_external_roots: &[PathBuf],
    nodes: &mut Vec<GraphNode>,
    deadline: OperationDeadline,
) -> Result<(), String> {
    push_graph_node_with_mode(
        root,
        path,
        is_file,
        true,
        approved_external_roots,
        nodes,
        deadline,
    )
}

fn push_mutable_graph_input(
    root: &Path,
    path: &Path,
    is_file: bool,
    approved_external_roots: &[PathBuf],
    nodes: &mut Vec<GraphNode>,
    optional_inputs: &mut Vec<MutableGraphInput>,
    deadline: OperationDeadline,
) -> Result<(), String> {
    push_mutable_graph_input_with_mode(
        root,
        path,
        is_file,
        approved_external_roots,
        nodes,
        optional_inputs,
        deadline,
        true,
    )
}

fn push_shallow_mutable_graph_input(
    root: &Path,
    path: &Path,
    is_file: bool,
    approved_external_roots: &[PathBuf],
    nodes: &mut Vec<GraphNode>,
    optional_inputs: &mut Vec<MutableGraphInput>,
    deadline: OperationDeadline,
) -> Result<(), String> {
    push_mutable_graph_input_with_mode(
        root,
        path,
        is_file,
        approved_external_roots,
        nodes,
        optional_inputs,
        deadline,
        false,
    )
}

fn push_mutable_graph_input_with_mode(
    root: &Path,
    path: &Path,
    is_file: bool,
    approved_external_roots: &[PathBuf],
    nodes: &mut Vec<GraphNode>,
    optional_inputs: &mut Vec<MutableGraphInput>,
    deadline: OperationDeadline,
    recursive: bool,
) -> Result<(), String> {
    check_graph_deadline(deadline)?;
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if (is_file && !metadata.is_file()) || (!is_file && !metadata.is_dir()) {
                return Err("repository graph path type is invalid".to_string());
            }
            if recursive {
                push_mutable_graph_node(
                    root,
                    path,
                    is_file,
                    approved_external_roots,
                    nodes,
                    deadline,
                )?;
            } else {
                push_shallow_mutable_graph_node(
                    root,
                    path,
                    is_file,
                    approved_external_roots,
                    nodes,
                    deadline,
                )?;
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            let canonical =
                canonicalize_missing_graph_path(root, path, approved_external_roots, deadline)?;
            optional_inputs.push(MutableGraphInput {
                path: canonical,
                is_file,
                initial_identity: None,
            });
        }
        Err(_) => return Err("repository graph path metadata is unavailable".to_string()),
    }
    Ok(())
}

fn push_optional_static_input(
    root: &Path,
    path: &Path,
    is_file: bool,
    approved_external_roots: &[PathBuf],
    optional_inputs: &mut Vec<MutableGraphInput>,
    deadline: OperationDeadline,
) -> Result<(), String> {
    check_graph_deadline(deadline)?;
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if (is_file && !metadata.is_file()) || (!is_file && !metadata.is_dir()) {
                return Err("repository graph path type is invalid".to_string());
            }
            let canonical = fs::canonicalize(path)
                .map_err(|_| "repository graph path cannot be canonicalized".to_string())?;
            check_graph_deadline(deadline)?;
            if !graph_path_allowed(root, &canonical, approved_external_roots)
                || !same_path(&canonical, path)
            {
                return Err("repository graph path is external or reparseable".to_string());
            }
            return Ok(());
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            let canonical =
                canonicalize_missing_graph_path(root, path, approved_external_roots, deadline)?;
            optional_inputs.push(MutableGraphInput {
                path: canonical,
                is_file,
                initial_identity: None,
            });
        }
        Err(_) => return Err("repository graph path metadata is unavailable".to_string()),
    }
    Ok(())
}

fn canonicalize_missing_graph_path(
    root: &Path,
    path: &Path,
    approved_external_roots: &[PathBuf],
    deadline: OperationDeadline,
) -> Result<PathBuf, String> {
    reject_reparse_components(path)?;
    let mut existing = path.to_path_buf();
    let mut missing = Vec::new();
    loop {
        check_graph_deadline(deadline)?;
        match fs::symlink_metadata(&existing) {
            Ok(_) => break,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                let name = existing
                    .file_name()
                    .ok_or_else(|| "repository graph path has no file name".to_string())?;
                if name == OsStr::new(".") || name == OsStr::new("..") {
                    return Err("repository graph path contains a parent traversal".to_string());
                }
                missing.push(name.to_os_string());
                if missing.len() > HARD_MAX_GRAPH_DEPTH {
                    return Err("repository graph path is too deep".to_string());
                }
                if !existing.pop() {
                    return Err("repository graph path has no existing ancestor".to_string());
                }
            }
            Err(_) => return Err("repository graph path parent cannot be inspected".to_string()),
        }
    }
    let canonical_existing = fs::canonicalize(&existing)
        .map_err(|_| "repository graph path parent cannot be canonicalized".to_string())?;
    check_graph_deadline(deadline)?;
    if !graph_path_allowed(root, &canonical_existing, approved_external_roots) {
        return Err("repository graph path is external to the workspace".to_string());
    }
    let mut canonical = canonical_existing;
    for name in missing.iter().rev() {
        check_graph_deadline(deadline)?;
        canonical.push(name);
    }
    if !graph_path_allowed(root, &canonical, approved_external_roots) {
        return Err("repository graph path is external to the workspace".to_string());
    }
    Ok(canonical)
}

fn optional_mutable_identity(path: &Path, is_file: bool) -> Result<Option<FileIdentity>, String> {
    optional_mutable_identity_with_deadline(path, is_file, None)
}

fn optional_mutable_identity_with_deadline(
    path: &Path,
    is_file: bool,
    deadline: Option<OperationDeadline>,
) -> Result<Option<FileIdentity>, String> {
    reject_reparse_components(path)?;
    if deadline.is_some_and(OperationDeadline::is_expired) {
        return Err("repository validation exceeded the operation deadline".to_string());
    }
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err("repository graph path metadata is unavailable".to_string()),
    };
    if (is_file && !metadata.is_file()) || (!is_file && !metadata.is_dir()) {
        return Err("repository graph path type is invalid".to_string());
    }
    let canonical = fs::canonicalize(path)
        .map_err(|_| "repository graph path cannot be canonicalized".to_string())?;
    if !same_path(&canonical, path) {
        return Err("repository graph path canonical identity changed".to_string());
    }
    let identity = if is_file {
        deadline.map_or_else(
            || data_file_identity(path),
            |deadline| data_file_identity_with_deadline(path, deadline),
        )?
    } else {
        mutable_directory_identity_with_deadline(path, deadline)?
    };
    Ok(Some(identity))
}

fn optional_mutable_container_identity_with_deadline(
    path: &Path,
    deadline: Option<OperationDeadline>,
) -> Result<Option<FileIdentity>, String> {
    if deadline.is_some_and(OperationDeadline::is_expired) {
        return Err("repository validation exceeded the operation deadline".to_string());
    }
    reject_reparse_components(path)?;
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err("repository graph path metadata is unavailable".to_string()),
    };
    if !metadata.is_dir() {
        return Err("repository graph path type is invalid".to_string());
    }
    let canonical = fs::canonicalize(path)
        .map_err(|_| "repository graph path cannot be canonicalized".to_string())?;
    if !same_path(&canonical, path) {
        return Err("repository graph path canonical identity changed".to_string());
    }
    let identity = directory_identity_with_deadline(&canonical, deadline)?;
    if deadline.is_some_and(OperationDeadline::is_expired) {
        return Err("repository validation exceeded the operation deadline".to_string());
    }
    Ok(Some(identity))
}

fn push_graph_node_with_mode(
    root: &Path,
    path: &Path,
    is_file: bool,
    mutable: bool,
    approved_external_roots: &[PathBuf],
    nodes: &mut Vec<GraphNode>,
    deadline: OperationDeadline,
) -> Result<(), String> {
    // Static graph directories are enumerated/bound through their descendants;
    // only mutable stores need a recursive directory snapshot.  Keeping the
    // repository Git-dir container shallow lets its direct operation-specific
    // entries (HEAD/index/refs/objects/worktree metadata) be validated without
    // conflating a child store's content digest with the container identity.
    push_graph_node_with_content_mode(
        root,
        path,
        is_file,
        mutable,
        mutable,
        approved_external_roots,
        nodes,
        deadline,
    )
}

fn push_shallow_mutable_graph_node(
    root: &Path,
    path: &Path,
    is_file: bool,
    approved_external_roots: &[PathBuf],
    nodes: &mut Vec<GraphNode>,
    deadline: OperationDeadline,
) -> Result<(), String> {
    push_graph_node_with_content_mode(
        root,
        path,
        is_file,
        true,
        false,
        approved_external_roots,
        nodes,
        deadline,
    )
}

fn push_graph_node_with_content_mode(
    root: &Path,
    path: &Path,
    is_file: bool,
    mutable: bool,
    mutable_recursive: bool,
    approved_external_roots: &[PathBuf],
    nodes: &mut Vec<GraphNode>,
    deadline: OperationDeadline,
) -> Result<(), String> {
    check_graph_deadline(deadline)?;
    reject_reparse_components(path)?;
    let canonical = fs::canonicalize(path)
        .map_err(|_| "repository graph path cannot be canonicalized".to_string())?;
    check_graph_deadline(deadline)?;
    if !graph_path_allowed(root, &canonical, approved_external_roots) {
        return Err("repository graph path is external to the workspace".to_string());
    }
    let metadata = fs::symlink_metadata(&canonical)
        .map_err(|_| "repository graph path metadata is unavailable".to_string())?;
    if (is_file && !metadata.is_file()) || (!is_file && !metadata.is_dir()) {
        return Err("repository graph path type is invalid".to_string());
    }
    let identity = if is_file {
        data_file_identity_with_deadline(&canonical, deadline)?
    } else if mutable && mutable_recursive {
        mutable_directory_identity_with_deadline(&canonical, Some(deadline))?
    } else {
        directory_identity_with_deadline(&canonical, Some(deadline))?
    };
    check_graph_deadline(deadline)?;
    if let Some(node) = nodes
        .iter_mut()
        .find(|node| same_path(&node.path, &canonical))
    {
        if node.is_file != is_file {
            return Err("repository graph entry type changed during admission".to_string());
        }
        let was_recursive = node.mutable_recursive;
        node.mutable |= mutable;
        node.mutable_recursive |= mutable_recursive;
        if mutable_recursive && !was_recursive {
            node.identity = identity;
        }
    } else {
        nodes.push(GraphNode {
            path: canonical,
            identity,
            is_file,
            mutable,
            mutable_recursive,
            content_bound: false,
        });
    }
    Ok(())
}

fn mark_static_graph_root(
    root: &Path,
    path: &Path,
    approved_external_roots: &[PathBuf],
    nodes: &mut [GraphNode],
    deadline: OperationDeadline,
) -> Result<(), String> {
    check_graph_deadline(deadline)?;
    let canonical = fs::canonicalize(path)
        .map_err(|_| "repository static graph root cannot be canonicalized".to_string())?;
    if !graph_path_allowed(root, &canonical, approved_external_roots) {
        return Err("repository static graph root is external to the workspace".to_string());
    }
    let identity = mutable_directory_identity_with_deadline(&canonical, Some(deadline))?;
    let node = nodes
        .iter_mut()
        .find(|node| same_path(&node.path, &canonical))
        .ok_or_else(|| "repository static graph root was not admitted".to_string())?;
    if node.is_file {
        return Err("repository static graph root is not a directory".to_string());
    }
    node.identity = identity;
    node.content_bound = true;
    check_graph_deadline(deadline)
}

fn push_static_graph_descendants(
    root: &Path,
    directory: &Path,
    approved_external_roots: &[PathBuf],
    nodes: &mut Vec<GraphNode>,
    deadline: OperationDeadline,
    depth: usize,
) -> Result<(), String> {
    check_graph_deadline(deadline)?;
    if depth > HARD_MAX_GRAPH_DEPTH {
        return Err("repository static Git graph is too deep".to_string());
    }
    let entries = fs::read_dir(directory)
        .map_err(|_| "repository graph directory cannot be enumerated safely".to_string())?;
    for entry in entries {
        check_graph_deadline(deadline)?;
        if nodes.len() >= HARD_MAX_GRAPH_NODES {
            return Err(format!(
                "repository graph exceeds the {HARD_MAX_GRAPH_NODES}-node limit"
            ));
        }
        let entry =
            entry.map_err(|_| "repository graph directory entry is unavailable".to_string())?;
        let path = entry.path();
        reject_reparse_components(&path)?;
        let metadata = fs::symlink_metadata(&path)
            .map_err(|_| "repository graph entry metadata is unavailable".to_string())?;
        if metadata.is_file() {
            push_graph_node(root, &path, true, approved_external_roots, nodes, deadline)?;
        } else if metadata.is_dir() {
            push_graph_node(root, &path, false, approved_external_roots, nodes, deadline)?;
            push_static_graph_descendants(
                root,
                &path,
                approved_external_roots,
                nodes,
                deadline,
                depth + 1,
            )?;
        } else {
            return Err("repository graph contains a non-regular entry".to_string());
        }
    }
    Ok(())
}

fn push_worktree_graph(
    root: &Path,
    worktrees: &Path,
    common_dir: &Path,
    approved_external_roots: &[PathBuf],
    nodes: &mut Vec<GraphNode>,
    optional_mutable_inputs: &mut Vec<MutableGraphInput>,
    deadline: OperationDeadline,
) -> Result<(), String> {
    check_graph_deadline(deadline)?;
    push_mutable_graph_node(
        root,
        worktrees,
        false,
        approved_external_roots,
        nodes,
        deadline,
    )?;
    let mut worktree_count = 0;
    for entry in fs::read_dir(worktrees)
        .map_err(|_| "repository worktree metadata cannot be enumerated safely".to_string())?
    {
        check_graph_deadline(deadline)?;
        if worktree_count >= HARD_MAX_WORKTREE_ENTRIES {
            return Err(format!(
                "Git worktree metadata exceeds the {HARD_MAX_WORKTREE_ENTRIES}-entry limit"
            ));
        }
        worktree_count += 1;
        let entry =
            entry.map_err(|_| "repository worktree metadata entry is unavailable".to_string())?;
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path)
            .map_err(|_| "repository worktree metadata entry is unavailable".to_string())?;
        if !metadata.is_dir() {
            return Err("repository worktree metadata entry is not a directory".to_string());
        }
        push_graph_node(root, &path, false, approved_external_roots, nodes, deadline)?;
        validate_worktree_metadata_descriptors(
            root,
            &path,
            common_dir,
            approved_external_roots,
            nodes,
            deadline,
        )?;
        push_worktree_metadata_descendants(
            root,
            &path,
            path.to_path_buf(),
            approved_external_roots,
            nodes,
            deadline,
            0,
        )?;
        for (name, is_file) in [
            ("HEAD", true),
            ("index", true),
            ("ORIG_HEAD", true),
            ("logs", false),
            ("refs", false),
        ] {
            push_shallow_mutable_graph_input(
                root,
                &path.join(name),
                is_file,
                approved_external_roots,
                nodes,
                optional_mutable_inputs,
                deadline,
            )?;
        }
    }
    Ok(())
}

fn validate_worktree_metadata_descriptors(
    root: &Path,
    metadata: &Path,
    common_dir: &Path,
    approved_external_roots: &[PathBuf],
    nodes: &mut Vec<GraphNode>,
    deadline: OperationDeadline,
) -> Result<(), String> {
    let gitdir_descriptor = metadata.join("gitdir");
    let linked_git_file = resolve_worktree_descriptor_path(
        &gitdir_descriptor,
        metadata,
        true,
        root,
        approved_external_roots,
        deadline,
    )?;
    let linked_git_contents =
        read_file_bounded_with_deadline(&linked_git_file, HARD_MAX_STDERR_BYTES, Some(deadline))
            .map_err(|_| "linked worktree Git descriptor cannot be read safely".to_string())?;
    let linked_git_contents = String::from_utf8(linked_git_contents)
        .map_err(|_| "linked worktree Git descriptor is not UTF-8".to_string())?;
    let linked_git_value = linked_git_contents
        .trim()
        .strip_prefix("gitdir:")
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "linked worktree Git descriptor is invalid".to_string())?;
    let linked_git_target = resolve_descriptor_value(
        linked_git_file
            .parent()
            .ok_or_else(|| "linked worktree Git descriptor has no parent".to_string())?,
        linked_git_value,
        root,
        approved_external_roots,
        deadline,
    )?;
    if !same_path(&linked_git_target, metadata) {
        return Err(
            "linked worktree Git descriptors do not bind to one metadata directory".to_string(),
        );
    }
    push_graph_node(
        root,
        &linked_git_file,
        true,
        approved_external_roots,
        nodes,
        deadline,
    )?;

    let commondir_descriptor = metadata.join("commondir");
    let linked_common_dir = resolve_worktree_descriptor_path(
        &commondir_descriptor,
        metadata,
        false,
        root,
        approved_external_roots,
        deadline,
    )?;
    if !same_path(&linked_common_dir, common_dir) {
        return Err(
            "linked worktree common directory is not the admitted common store".to_string(),
        );
    }
    Ok(())
}

fn resolve_worktree_descriptor_path(
    descriptor: &Path,
    base: &Path,
    expect_file: bool,
    root: &Path,
    approved_external_roots: &[PathBuf],
    deadline: OperationDeadline,
) -> Result<PathBuf, String> {
    check_graph_deadline(deadline)?;
    reject_reparse_components(descriptor)?;
    let metadata = fs::symlink_metadata(descriptor)
        .map_err(|_| "linked worktree descriptor is unavailable".to_string())?;
    if !metadata.is_file() {
        return Err("linked worktree descriptor is not a regular file".to_string());
    }
    check_graph_deadline(deadline)?;
    let contents =
        read_file_bounded_with_deadline(descriptor, HARD_MAX_STDERR_BYTES, Some(deadline))
            .map_err(|_| "linked worktree descriptor cannot be read safely".to_string())?;
    let contents = String::from_utf8(contents)
        .map_err(|_| "linked worktree descriptor is not UTF-8".to_string())?;
    let value = contents.trim();
    if value.is_empty() || value.contains('\n') || value.contains('\r') {
        return Err("linked worktree descriptor is invalid".to_string());
    }
    let canonical = resolve_descriptor_value(base, value, root, approved_external_roots, deadline)?;
    let metadata = fs::symlink_metadata(&canonical)
        .map_err(|_| "linked worktree descriptor target is unavailable".to_string())?;
    if (expect_file && !metadata.is_file()) || (!expect_file && !metadata.is_dir()) {
        return Err("linked worktree descriptor target has the wrong type".to_string());
    }
    Ok(canonical)
}

fn resolve_descriptor_value(
    base: &Path,
    value: &str,
    root: &Path,
    approved_external_roots: &[PathBuf],
    deadline: OperationDeadline,
) -> Result<PathBuf, String> {
    check_graph_deadline(deadline)?;
    let requested = PathBuf::from(value);
    let requested = if requested.is_absolute() {
        requested
    } else {
        base.join(requested)
    };
    reject_reparse_components(&requested)?;
    check_graph_deadline(deadline)?;
    let canonical = fs::canonicalize(&requested)
        .map_err(|_| "linked worktree descriptor target cannot be canonicalized".to_string())?;
    check_graph_deadline(deadline)?;
    if !graph_path_allowed(root, &canonical, approved_external_roots) {
        return Err(
            "linked worktree descriptor target is outside the approved Git graph".to_string(),
        );
    }
    Ok(canonical)
}

fn push_worktree_metadata_descendants(
    root: &Path,
    worktree: &Path,
    directory: PathBuf,
    approved_external_roots: &[PathBuf],
    nodes: &mut Vec<GraphNode>,
    deadline: OperationDeadline,
    depth: usize,
) -> Result<(), String> {
    // The enclosing worktrees root owns the recursive content snapshot. Keep
    // nested mutable directories as shallow containers so revalidation does
    // not rescan the same subtree once per descendant; the outer snapshot
    // still binds every descendant identity/content before and after Git.
    check_graph_deadline(deadline)?;
    if depth > HARD_MAX_GRAPH_DEPTH {
        return Err("repository worktree metadata graph is too deep".to_string());
    }
    for entry in fs::read_dir(&directory)
        .map_err(|_| "repository worktree metadata cannot be enumerated safely".to_string())?
    {
        check_graph_deadline(deadline)?;
        if nodes.len() >= HARD_MAX_GRAPH_NODES {
            return Err(format!(
                "repository graph exceeds the {HARD_MAX_GRAPH_NODES}-node limit"
            ));
        }
        let entry =
            entry.map_err(|_| "repository worktree metadata entry is unavailable".to_string())?;
        let path = entry.path();
        reject_reparse_components(&path)?;
        let metadata = fs::symlink_metadata(&path)
            .map_err(|_| "repository worktree metadata entry is unavailable".to_string())?;
        let relative = path
            .strip_prefix(worktree)
            .map_err(|_| "repository worktree metadata path escaped its root".to_string())?;
        let mutable = relative.components().any(|component| {
            matches!(
                component,
                std::path::Component::Normal(name)
                    if name == OsStr::new("HEAD")
                        || name == OsStr::new("index")
                        || name == OsStr::new("ORIG_HEAD")
                        || name == OsStr::new("logs")
                        || name == OsStr::new("refs")
            )
        });
        if metadata.is_file() {
            if mutable {
                push_shallow_mutable_graph_node(
                    root,
                    &path,
                    true,
                    approved_external_roots,
                    nodes,
                    deadline,
                )?;
            } else {
                push_graph_node(root, &path, true, approved_external_roots, nodes, deadline)?;
            }
        } else if metadata.is_dir() {
            if mutable {
                push_shallow_mutable_graph_node(
                    root,
                    &path,
                    false,
                    approved_external_roots,
                    nodes,
                    deadline,
                )?;
            } else {
                push_graph_node(root, &path, false, approved_external_roots, nodes, deadline)?;
            }
            push_worktree_metadata_descendants(
                root,
                worktree,
                path,
                approved_external_roots,
                nodes,
                deadline,
                depth + 1,
            )?;
        } else {
            return Err("repository worktree metadata contains a non-regular entry".to_string());
        }
    }
    Ok(())
}

fn enforce_graph_limits(
    root: &Path,
    object_stores: &[PathBuf],
    nodes: &[GraphNode],
    git_dir: &Path,
    common_dir: Option<&Path>,
    metadata_roots: &[PathBuf],
    approved_external_roots: &[PathBuf],
    deadline: OperationDeadline,
) -> Result<(), String> {
    check_graph_deadline(deadline)?;
    let mut stats = GraphTreeStats::default();
    // Static descriptors, alternates, pack metadata, and worktree metadata
    // are all represented as graph nodes. Count their bounded file content in
    // the same admission budget as the mutable trees so a large alternate
    // cannot bypass the aggregate graph-byte cap.
    for node in nodes {
        check_graph_deadline(deadline)?;
        if node.is_file {
            stats.bytes = stats.bytes.saturating_add(node.identity.file_size);
        }
    }
    for object_store in object_stores {
        collect_graph_tree_stats(object_store, GraphTreeRole::Object, &mut stats, deadline, 0)?;
    }
    let state_root = common_dir.unwrap_or(git_dir);
    collect_graph_tree_stats(
        &state_root.join("refs"),
        GraphTreeRole::Refs,
        &mut stats,
        deadline,
        0,
    )?;
    collect_graph_tree_stats(
        &state_root.join("logs"),
        GraphTreeRole::Logs,
        &mut stats,
        deadline,
        0,
    )?;
    if common_dir.is_some() {
        collect_graph_tree_stats(
            &git_dir.join("refs"),
            GraphTreeRole::Refs,
            &mut stats,
            deadline,
            0,
        )?;
        collect_graph_tree_stats(
            &git_dir.join("logs"),
            GraphTreeRole::Logs,
            &mut stats,
            deadline,
            0,
        )?;
    }
    for metadata_root in metadata_roots {
        collect_graph_tree_stats(
            metadata_root,
            GraphTreeRole::Worktrees,
            &mut stats,
            deadline,
            0,
        )?;
    }
    if stats.nodes > HARD_MAX_GRAPH_NODES {
        return Err(format!(
            "repository graph exceeds the {HARD_MAX_GRAPH_NODES}-node limit"
        ));
    }
    if stats.bytes > HARD_MAX_GRAPH_BYTES {
        return Err(format!(
            "repository graph content exceeds the {HARD_MAX_GRAPH_BYTES}-byte limit"
        ));
    }
    if stats.packs > HARD_MAX_PACK_FILES {
        return Err(format!(
            "Git pack files exceed the {HARD_MAX_PACK_FILES}-entry limit"
        ));
    }
    if stats.refs > HARD_MAX_REF_ENTRIES {
        return Err(format!(
            "Git refs exceed the {HARD_MAX_REF_ENTRIES}-entry limit"
        ));
    }
    if stats.logs > HARD_MAX_LOG_ENTRIES {
        return Err(format!(
            "Git logs exceed the {HARD_MAX_LOG_ENTRIES}-entry limit"
        ));
    }
    if stats.worktrees > HARD_MAX_WORKTREE_ENTRIES {
        return Err(format!(
            "Git worktree metadata exceeds the {HARD_MAX_WORKTREE_ENTRIES}-entry limit"
        ));
    }
    if !object_stores
        .iter()
        .all(|object_store| graph_path_allowed(root, object_store, approved_external_roots))
    {
        return Err("repository graph object store is external".to_string());
    }
    Ok(())
}

#[derive(Clone, Copy)]
enum GraphTreeRole {
    Object,
    Refs,
    Logs,
    Worktrees,
}

#[derive(Default)]
struct GraphTreeStats {
    nodes: usize,
    bytes: u64,
    packs: usize,
    refs: usize,
    logs: usize,
    worktrees: usize,
}

fn collect_graph_tree_stats(
    directory: &Path,
    role: GraphTreeRole,
    stats: &mut GraphTreeStats,
    deadline: OperationDeadline,
    depth: usize,
) -> Result<(), String> {
    check_graph_deadline(deadline)?;
    if depth > HARD_MAX_GRAPH_DEPTH {
        return Err("repository graph tree is too deep".to_string());
    }
    let metadata = match fs::symlink_metadata(directory) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(_) => return Err("repository graph directory metadata is unavailable".to_string()),
    };
    if !metadata.is_dir() {
        return Err("repository graph tree root is not a directory".to_string());
    }
    let entries = fs::read_dir(directory)
        .map_err(|_| "repository graph directory cannot be enumerated safely".to_string())?;
    for entry in entries {
        check_graph_deadline(deadline)?;
        stats.nodes = stats.nodes.saturating_add(1);
        let entry =
            entry.map_err(|_| "repository graph directory entry is unavailable".to_string())?;
        let path = entry.path();
        reject_reparse_components(&path)?;
        let metadata = fs::symlink_metadata(&path)
            .map_err(|_| "repository graph entry metadata is unavailable".to_string())?;
        if metadata.is_file() {
            stats.bytes = stats.bytes.saturating_add(metadata.len());
            if matches!(role, GraphTreeRole::Object)
                && path
                    .parent()
                    .and_then(Path::file_name)
                    .is_some_and(|name| name == OsStr::new("pack"))
                && path.extension().is_some_and(|extension| {
                    extension == OsStr::new("pack") || extension == OsStr::new("idx")
                })
            {
                stats.packs = stats.packs.saturating_add(1);
            }
            match role {
                GraphTreeRole::Refs => stats.refs = stats.refs.saturating_add(1),
                GraphTreeRole::Logs => stats.logs = stats.logs.saturating_add(1),
                GraphTreeRole::Worktrees => stats.worktrees = stats.worktrees.saturating_add(1),
                GraphTreeRole::Object => {}
            }
        } else if metadata.is_dir() {
            collect_graph_tree_stats(&path, role, stats, deadline, depth + 1)?;
        } else {
            return Err("repository graph contains a non-regular entry".to_string());
        }
        if stats.nodes > HARD_MAX_GRAPH_NODES {
            return Ok(());
        }
    }
    Ok(())
}

fn check_graph_deadline(deadline: OperationDeadline) -> Result<(), String> {
    if deadline.is_expired() {
        Err("repository graph admission exceeded the operation deadline".to_string())
    } else {
        Ok(())
    }
}

const HARD_MAX_MUTABLE_GRAPH_ENTRIES: usize = 4096;

fn mutable_directory_identity(path: &Path) -> Result<FileIdentity, String> {
    mutable_directory_identity_with_deadline(path, None)
}

fn mutable_directory_identity_with_deadline(
    path: &Path,
    deadline: Option<OperationDeadline>,
) -> Result<FileIdentity, String> {
    let mut identity = directory_identity_with_deadline(path, deadline)?;
    let entries = mutable_directory_snapshot_with_deadline(path, deadline)?;
    let mut hasher = Sha256::new();
    for (relative, is_file, entry_identity) in entries {
        update_os_string_digest(&mut hasher, &relative);
        hasher.update([is_file as u8]);
        hasher.update(identity_token(&entry_identity).as_bytes());
    }
    let digest = hasher.finalize();
    identity.content_digest.copy_from_slice(&digest);
    if deadline.is_some_and(OperationDeadline::is_expired) {
        return Err("repository graph validation exceeded the operation deadline".to_string());
    }
    Ok(identity)
}

fn mutable_directory_snapshot_with_deadline(
    path: &Path,
    deadline: Option<OperationDeadline>,
) -> Result<MutableDirectorySnapshot, String> {
    let mut entries = Vec::new();
    collect_mutable_directory_entries(path, path, 0, &mut entries, deadline)?;
    entries.sort_by(|left, right| left.0.cmp(&right.0));
    Ok(entries)
}

fn mutable_directory_snapshot_for_node(
    node: &GraphNode,
    deadline: Option<OperationDeadline>,
) -> Result<MutableDirectorySnapshot, String> {
    if node.mutable_recursive {
        return mutable_directory_snapshot_with_deadline(&node.path, deadline);
    }
    let children = fs::read_dir(&node.path)
        .map_err(|_| "mutable Git graph directory cannot be read".to_string())?;
    let mut entries = Vec::new();
    for entry in children {
        if deadline.is_some_and(OperationDeadline::is_expired) {
            return Err("repository validation exceeded the operation deadline".to_string());
        }
        if entries.len() >= HARD_MAX_MUTABLE_GRAPH_ENTRIES {
            return Err("mutable Git graph exceeds the immutable entry limit".to_string());
        }
        let path = entry
            .map_err(|_| "mutable Git graph directory entry cannot be read".to_string())?
            .path();
        reject_reparse_components(&path)?;
        let metadata = fs::symlink_metadata(&path)
            .map_err(|_| "mutable Git graph entry metadata is unavailable".to_string())?;
        let relative = path
            .strip_prefix(&node.path)
            .unwrap_or(&path)
            .as_os_str()
            .to_os_string();
        if metadata.is_dir() {
            entries.push((
                relative,
                false,
                directory_identity_with_deadline(&path, deadline)?,
            ));
        } else if metadata.is_file() {
            let identity = deadline.map_or_else(
                || data_file_identity(&path),
                |deadline| data_file_identity_with_deadline(&path, deadline),
            )?;
            entries.push((relative, true, identity));
        } else {
            return Err("mutable Git graph contains a non-regular entry".to_string());
        }
    }
    entries.sort_by(|left, right| left.0.cmp(&right.0));
    Ok(entries)
}

fn graph_identity_matches(expected: &FileIdentity, actual: &FileIdentity) -> bool {
    // A strict/read transition compares the complete identity, including the
    // bounded content digest.  Same-inode writes to HEAD/config/refs are still
    // attacker-controlled changes and must not pass a read validation merely
    // because the platform file id was retained.  Authorized transitions are
    // the only place where a new identity is adopted into the baseline.
    expected == actual
}

fn graph_container_identity_matches(expected: &FileIdentity, actual: &FileIdentity) -> bool {
    #[cfg(windows)]
    {
        expected.volume_serial_number == actual.volume_serial_number
            && expected.file_index == actual.file_index
    }

    #[cfg(unix)]
    {
        expected.device == actual.device && expected.inode == actual.inode
    }

    #[cfg(not(any(unix, windows)))]
    {
        true
    }
}

fn collect_mutable_directory_entries(
    root: &Path,
    directory: &Path,
    depth: usize,
    entries: &mut Vec<(OsString, bool, FileIdentity)>,
    deadline: Option<OperationDeadline>,
) -> Result<(), String> {
    if depth > HARD_MAX_GRAPH_DEPTH {
        return Err("mutable Git graph is too deep".to_string());
    }
    let children = fs::read_dir(directory)
        .map_err(|_| "mutable Git graph directory cannot be read".to_string())?;
    for entry in children {
        if deadline.is_some_and(OperationDeadline::is_expired) {
            return Err("repository validation exceeded the operation deadline".to_string());
        }
        if entries.len() >= HARD_MAX_MUTABLE_GRAPH_ENTRIES {
            return Err("mutable Git graph exceeds the immutable entry limit".to_string());
        }
        let path = entry
            .map_err(|_| "mutable Git graph directory entry cannot be read".to_string())?
            .path();
        reject_reparse_components(&path)?;
        let metadata = fs::symlink_metadata(&path)
            .map_err(|_| "mutable Git graph entry metadata is unavailable".to_string())?;
        let relative = path
            .strip_prefix(root)
            .unwrap_or(&path)
            .as_os_str()
            .to_os_string();
        if metadata.is_dir() {
            let identity = directory_identity_with_deadline(&path, deadline)?;
            entries.push((relative, false, identity));
            collect_mutable_directory_entries(root, &path, depth + 1, entries, deadline)?;
        } else if metadata.is_file() {
            let identity = deadline.map_or_else(
                || data_file_identity(&path),
                |deadline| data_file_identity_with_deadline(&path, deadline),
            )?;
            entries.push((relative, true, identity));
        } else {
            return Err("mutable Git graph contains a non-regular entry".to_string());
        }
    }
    Ok(())
}

fn reject_unsafe_local_config(
    path: &Path,
    deadline: Option<OperationDeadline>,
) -> Result<(), String> {
    let contents = read_file_bounded_with_deadline(path, HARD_MAX_STDERR_BYTES, deadline)
        .map_err(|_| "repository Git config cannot be read safely".to_string())?;
    let mut section = String::new();
    for line in String::from_utf8_lossy(&contents).lines() {
        if deadline.is_some_and(OperationDeadline::is_expired) {
            return Err(
                "repository Git config validation exceeded the operation deadline".to_string(),
            );
        }
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        if let Some(header) = line
            .strip_prefix('[')
            .and_then(|line| line.strip_suffix(']'))
        {
            section = header
                .split_whitespace()
                .next()
                .unwrap_or_default()
                .trim()
                .to_ascii_lowercase();
            continue;
        }
        let key = line
            .split_once('=')
            .map_or(line, |(key, _)| key)
            .trim()
            .to_ascii_lowercase();
        let qualified = if section.is_empty() {
            key.clone()
        } else {
            format!("{section}.{key}")
        };
        let unsafe_key = qualified == "include"
            || qualified.starts_with("include.")
            || qualified.starts_with("includeif.")
            || qualified.starts_with("core.hookspath")
            || qualified.starts_with("core.fsmonitor")
            || qualified == "core.attributesfile"
            || qualified == "core.excludesfile"
            || qualified == "core.mailmap"
            || qualified == "core.worktree"
            || qualified == "core.sshcommand"
            || qualified == "core.gitproxy"
            || qualified == "core.editor"
            || qualified == "core.pager"
            || qualified.starts_with("credential.")
            || qualified.starts_with("filter.")
            || qualified.starts_with("diff.")
            || qualified.starts_with("url.")
            || qualified.starts_with("protocol.")
            || qualified.starts_with("http.")
            || qualified.starts_with("https.")
            || qualified.starts_with("interactive.")
            || qualified.starts_with("merge.")
            || qualified.starts_with("mergetool.")
            || qualified.starts_with("gpg.")
            || qualified.starts_with("submodule.")
            || qualified == "commit.gpgsign"
            || qualified == "tag.gpgsign"
            || qualified == "sequence.editor"
            || qualified.starts_with("remote.") && qualified.ends_with(".proxy")
            || qualified.starts_with("remote.") && qualified.ends_with(".vcs")
            || qualified.ends_with(".uploadpack")
            || qualified.ends_with(".receivepack")
            || qualified.ends_with(".insteadof")
            || qualified.ends_with(".pushinsteadof");
        if unsafe_key {
            return Err(
                "repository Git config contains an unsafe executable or external path".to_string(),
            );
        }
    }
    if deadline.is_some_and(OperationDeadline::is_expired) {
        return Err("repository Git config validation exceeded the operation deadline".to_string());
    }
    Ok(())
}

fn read_file_bounded_with_deadline(
    path: &Path,
    max_bytes: usize,
    deadline: Option<OperationDeadline>,
) -> io::Result<Vec<u8>> {
    if deadline.is_some_and(OperationDeadline::is_expired) {
        return Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "bounded file read exceeded the operation deadline",
        ));
    }
    let metadata = fs::metadata(path)?;
    if metadata.len() > max_bytes as u64 {
        return Err(io::Error::new(
            io::ErrorKind::FileTooLarge,
            "file exceeds bounded read size",
        ));
    }
    let mut file = fs::File::open(path)?;
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    let mut buffer = [0u8; 64 * 1024];
    loop {
        if deadline.is_some_and(OperationDeadline::is_expired) {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "bounded file read exceeded the operation deadline",
            ));
        }
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        bytes.extend_from_slice(&buffer[..count]);
        if bytes.len() > max_bytes {
            return Err(io::Error::new(
                io::ErrorKind::FileTooLarge,
                "file exceeds bounded read size",
            ));
        }
    }
    if deadline.is_some_and(OperationDeadline::is_expired) {
        return Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "bounded file read exceeded the operation deadline",
        ));
    }
    if bytes.len() > max_bytes {
        return Err(io::Error::new(
            io::ErrorKind::FileTooLarge,
            "file exceeds bounded read size",
        ));
    }
    Ok(bytes)
}

#[derive(Clone, Copy)]
enum RepositoryValidationMode {
    Strict,
    TransitionDuring(GraphTransition),
    Transition(GraphTransition),
}

#[derive(Clone)]
struct RepositoryRoot {
    path: PathBuf,
    identity: FileIdentity,
    handle: Arc<fs::File>,
    /// Static graph and ancestor handles remain live for the complete child
    /// lifetime.  On Windows these handles do not share write/delete, which
    /// closes the final path-validation/CreateProcess replacement window.
    pinned_handles: Arc<Vec<fs::File>>,
    graph: RepositoryGraph,
}

impl RepositoryRoot {
    fn open(requested: &Path) -> Result<Self, String> {
        Self::open_with_approved_external_roots_and_deadline(
            requested,
            &[],
            OperationDeadline::from_now(HARD_MAX_TIMEOUT),
        )
    }

    fn open_with_approved_external_roots(
        requested: &Path,
        approved_external_roots: &[PathBuf],
    ) -> Result<Self, String> {
        Self::open_with_approved_external_roots_and_deadline(
            requested,
            approved_external_roots,
            OperationDeadline::from_now(HARD_MAX_TIMEOUT),
        )
    }

    fn open_with_approved_external_roots_and_deadline(
        requested: &Path,
        approved_external_roots: &[PathBuf],
        deadline: OperationDeadline,
    ) -> Result<Self, String> {
        check_graph_deadline(deadline)?;
        reject_reparse_components(requested)?;
        let path = fs::canonicalize(requested)
            .map_err(|_| "repository root cannot be canonicalized".to_string())?;
        let identity = directory_identity_with_deadline(&path, Some(deadline))?;
        check_graph_deadline(deadline)?;
        let graph = RepositoryGraph::open_with_deadline(&path, approved_external_roots, deadline)?;
        check_graph_deadline(deadline)?;
        #[cfg(windows)]
        let handle = open_directory_handle(&path)?;
        #[cfg(unix)]
        let handle = fs::File::open(&path)
            .map_err(|_| "repository root cannot be held by handle".to_string())?;
        let pinned_handles = pin_repository_graph(&path, &graph, deadline)?;
        Ok(Self {
            path,
            identity,
            handle: Arc::new(handle),
            pinned_handles: Arc::new(pinned_handles),
            graph,
        })
    }

    fn revalidate(&self) -> Result<(), String> {
        self.revalidate_internal(RepositoryValidationMode::Strict, None)
    }

    fn revalidate_with_deadline(&self, deadline: OperationDeadline) -> Result<(), String> {
        self.revalidate_internal(RepositoryValidationMode::Strict, Some(deadline))
    }

    fn revalidate_during_transition_with_deadline(
        &self,
        transition: GraphTransition,
        deadline: OperationDeadline,
    ) -> Result<(), String> {
        self.revalidate_internal(
            RepositoryValidationMode::TransitionDuring(transition),
            Some(deadline),
        )
    }

    fn revalidate_after_transition(&self, transition: GraphTransition) -> Result<(), String> {
        self.revalidate_internal(RepositoryValidationMode::Transition(transition), None)
    }

    fn revalidate_after_transition_with_deadline(
        &self,
        transition: GraphTransition,
        deadline: OperationDeadline,
    ) -> Result<(), String> {
        self.revalidate_internal(
            RepositoryValidationMode::Transition(transition),
            Some(deadline),
        )
    }

    fn revalidate_internal(
        &self,
        mode: RepositoryValidationMode,
        deadline: Option<OperationDeadline>,
    ) -> Result<(), String> {
        if deadline.is_some_and(OperationDeadline::is_expired) {
            return Err("repository validation exceeded the operation deadline".to_string());
        }
        let canonical =
            fs::canonicalize(&self.path).map_err(|_| "repository root path changed".to_string())?;
        if deadline.is_some_and(OperationDeadline::is_expired) {
            return Err("repository validation exceeded the operation deadline".to_string());
        }
        if !same_path(&canonical, &self.path) {
            return Err("repository root canonical path changed".to_string());
        }
        if deadline.is_some_and(OperationDeadline::is_expired) {
            return Err("repository validation exceeded the operation deadline".to_string());
        }
        if directory_identity_with_deadline(&canonical, deadline)? != self.identity {
            return Err("repository root file identity changed".to_string());
        }
        if deadline.is_some_and(OperationDeadline::is_expired) {
            return Err("repository validation exceeded the operation deadline".to_string());
        }
        let graph_result = match mode {
            RepositoryValidationMode::Strict => self.graph.revalidate_with_deadline(deadline),
            RepositoryValidationMode::TransitionDuring(transition) => self
                .graph
                .revalidate_during_transition_with_deadline(transition, deadline),
            RepositoryValidationMode::Transition(transition) => self
                .graph
                .revalidate_after_transition_with_deadline(transition, deadline),
        };
        graph_result?;
        if deadline.is_some_and(OperationDeadline::is_expired) {
            return Err("repository validation exceeded the operation deadline".to_string());
        }
        #[cfg(windows)]
        {
            let mut held_identity = windows_handle_identity(&self.handle, [0; 32])?;
            held_identity.file_size = 0;
            held_identity.last_write_time = 0;
            if held_identity != self.identity {
                return Err("held repository root handle identity changed".to_string());
            }
        }
        #[cfg(unix)]
        {
            let metadata = self
                .handle
                .metadata()
                .map_err(|_| "held repository root handle is unavailable".to_string())?;
            let held_identity = FileIdentity {
                device: metadata.dev(),
                inode: metadata.ino(),
                number_of_links: metadata.nlink(),
                file_size: metadata.size(),
                modified_seconds: metadata.mtime(),
                modified_nanos: metadata.mtime_nsec(),
                content_digest: [0; 32],
            };
            if held_identity != self.identity {
                return Err("held repository root handle identity changed".to_string());
            }
        }
        if deadline.is_some_and(OperationDeadline::is_expired) {
            return Err("repository validation exceeded the operation deadline".to_string());
        }
        Ok(())
    }

    fn state_exists(&self, relative: &str) -> Result<bool, String> {
        self.state_exists_with_deadline(relative, None)
    }

    fn state_exists_with_deadline(
        &self,
        relative: &str,
        deadline: Option<OperationDeadline>,
    ) -> Result<bool, String> {
        self.graph
            .state_exists_with_deadline(&self.path, relative, deadline)
    }

    fn configure_command(&self, command: &mut Command) {
        command.current_dir(&self.path);
        #[cfg(unix)]
        {
            let file_descriptor = self.handle.as_raw_fd();
            unsafe {
                command.pre_exec(move || {
                    if fchdir(file_descriptor) == 0 {
                        Ok(())
                    } else {
                        Err(io::Error::last_os_error())
                    }
                });
            }
        }
    }
}

fn pin_repository_graph(
    root: &Path,
    graph: &RepositoryGraph,
    deadline: OperationDeadline,
) -> Result<Vec<fs::File>, String> {
    let mut paths = Vec::new();
    let mut ancestor = Some(root);
    let mut depth = 0;
    while let Some(path) = ancestor {
        if depth >= HARD_MAX_GRAPH_DEPTH {
            return Err("repository root ancestor graph is too deep".to_string());
        }
        depth += 1;
        check_graph_deadline(deadline)?;
        paths.push(path.to_path_buf());
        ancestor = path.parent();
    }
    for node in &graph.nodes {
        check_graph_deadline(deadline)?;
        // Mutable files are intentionally not held read-only: Git must be
        // able to perform the operation-specific replacement admitted by the
        // graph transition. Retain every directory (including mutable object,
        // refs, logs, and worktree roots) plus static-file ancestors so the
        // graph owns the container handles for the child lifetime.
        if !node.is_file || !node.mutable {
            let mut ancestor = Some(node.path.as_path());
            let mut depth = 0;
            while let Some(path) = ancestor {
                if depth >= HARD_MAX_GRAPH_DEPTH {
                    return Err("repository graph ancestor path is too deep".to_string());
                }
                depth += 1;
                paths.push(path.to_path_buf());
                ancestor = path.parent();
            }
        }
    }
    for input in graph
        .optional_static_inputs
        .iter()
        .filter(|input| input.initial_identity.is_some())
    {
        check_graph_deadline(deadline)?;
        let mut ancestor = Some(input.path.as_path());
        let mut depth = 0;
        while let Some(path) = ancestor {
            if depth >= HARD_MAX_GRAPH_DEPTH {
                return Err("repository optional graph ancestor path is too deep".to_string());
            }
            depth += 1;
            paths.push(path.to_path_buf());
            ancestor = path.parent();
        }
    }
    paths.sort_by(|left, right| left.as_os_str().cmp(right.as_os_str()));
    paths.dedup_by(|left, right| same_path(left, right));
    let handles = paths
        .into_iter()
        .map(|path| {
            check_graph_deadline(deadline)?;
            let metadata = fs::symlink_metadata(&path)
                .map_err(|_| "pinned repository graph path is unavailable".to_string())?;
            if metadata.is_dir() {
                open_directory_handle_for_pin(&path)
            } else if metadata.is_file() {
                open_file_handle_for_pin(&path)
            } else {
                Err("pinned repository graph path is not regular".to_string())
            }
        })
        .collect::<Result<Vec<_>, _>>()?;
    check_graph_deadline(deadline)?;
    Ok(handles)
}

#[cfg(windows)]
fn open_directory_handle_for_pin(path: &Path) -> Result<fs::File, String> {
    // Retained graph and executable ancestors must block rename/delete of
    // the admitted directory. Git may still mutate files inside it; only
    // the directory container itself is held against replacement.
    open_windows_directory_handle(path, FILE_SHARE_READ | FILE_SHARE_WRITE)
}

#[cfg(windows)]
fn open_file_handle_for_pin(path: &Path) -> Result<fs::File, String> {
    let mut wide = path.as_os_str().encode_wide().collect::<Vec<_>>();
    wide.push(0);
    let handle = unsafe {
        CreateFileW(
            wide.as_ptr(),
            GENERIC_READ | FILE_READ_ATTRIBUTES | SYNCHRONIZE,
            FILE_SHARE_READ,
            std::ptr::null_mut(),
            OPEN_EXISTING,
            0,
            std::ptr::null_mut(),
        )
    };
    if handle.is_null() || handle == INVALID_HANDLE_VALUE {
        return Err("repository graph file cannot be held by handle".to_string());
    }
    Ok(unsafe { fs::File::from_raw_handle(handle) })
}

#[cfg(unix)]
fn open_directory_handle_for_pin(path: &Path) -> Result<fs::File, String> {
    fs::File::open(path).map_err(|_| "repository graph directory cannot be held".to_string())
}

#[cfg(unix)]
fn open_file_handle_for_pin(path: &Path) -> Result<fs::File, String> {
    fs::File::open(path).map_err(|_| "repository graph file cannot be held".to_string())
}

#[cfg(not(any(unix, windows)))]
fn open_directory_handle_for_pin(path: &Path) -> Result<fs::File, String> {
    fs::File::open(path).map_err(|_| "repository graph directory cannot be held".to_string())
}

#[cfg(not(any(unix, windows)))]
fn open_file_handle_for_pin(path: &Path) -> Result<fs::File, String> {
    fs::File::open(path).map_err(|_| "repository graph file cannot be held".to_string())
}

fn repository_graph_identity(root: &RepositoryRoot) -> String {
    repository_graph_identity_with_filter(root, true)
}

fn repository_static_graph_identity(root: &RepositoryRoot) -> String {
    repository_graph_identity_with_filter(root, false)
}

fn repository_graph_identity_with_filter(root: &RepositoryRoot, include_mutable: bool) -> String {
    let mut hasher = Sha256::new();
    update_os_string_digest(&mut hasher, &root.path.as_os_str().to_os_string());
    hasher.update(identity_token(&root.identity).as_bytes());
    for node in &root.graph.nodes {
        if !include_mutable && node.mutable {
            continue;
        }
        update_os_string_digest(&mut hasher, &node.path.as_os_str().to_os_string());
        hasher.update([node.is_file as u8]);
        hasher.update([node.mutable as u8]);
        hasher.update([node.mutable_recursive as u8]);
        hasher.update([node.content_bound as u8]);
        hasher.update(identity_token(&node.identity).as_bytes());
    }
    for input in &root.graph.optional_static_inputs {
        update_os_string_digest(&mut hasher, &input.path.as_os_str().to_os_string());
        hasher.update([input.is_file as u8]);
        match &input.initial_identity {
            Some(identity) => hasher.update(identity_token(identity).as_bytes()),
            None => hasher.update(b"<absent-static>"),
        }
    }
    if include_mutable {
        for input in &root.graph.optional_mutable_inputs {
            update_os_string_digest(&mut hasher, &input.path.as_os_str().to_os_string());
            hasher.update([input.is_file as u8]);
            match &input.initial_identity {
                Some(identity) => hasher.update(identity_token(identity).as_bytes()),
                None => hasher.update(b"<absent>"),
            }
        }
    }
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

struct GitSandbox {
    root: PathBuf,
    hooks_dir: PathBuf,
    config_file: PathBuf,
    home_dir: PathBuf,
    xdg_config_dir: PathBuf,
    xdg_cache_dir: PathBuf,
    attributes_file: PathBuf,
}

impl fmt::Debug for GitSandbox {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("GitSandbox")
            .field("root", &"<git-sandbox>")
            .field("hooks_dir", &"<git-hooks>")
            .field("config_file", &"<git-config>")
            .finish()
    }
}

impl GitSandbox {
    #[cfg(test)]
    fn new() -> Result<Self, String> {
        Self::new_with_deadline(None)
    }

    fn new_with_deadline(deadline: Option<OperationDeadline>) -> Result<Self, String> {
        let check_deadline = |deadline: Option<OperationDeadline>| {
            deadline.is_some_and(OperationDeadline::is_expired)
        };
        if check_deadline(deadline) {
            return Err("Git sandbox setup exceeded the operation deadline".to_string());
        }
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or_default();
        let base = env::temp_dir();
        let mut root = None;
        for attempt in 0..16u32 {
            let candidate = base.join(format!(
                "devmanager-git-sandbox-{}-{nonce}-{attempt}",
                std::process::id()
            ));
            if fs::create_dir(&candidate).is_ok() {
                root = Some(candidate);
                break;
            }
        }
        let root = root.ok_or_else(|| "Git sandbox could not be created".to_string())?;
        if check_deadline(deadline) {
            let _ = fs::remove_dir_all(&root);
            return Err("Git sandbox setup exceeded the operation deadline".to_string());
        }
        let hooks_dir = root.join("hooks");
        let home_dir = root.join("home");
        let xdg_config_dir = root.join("xdg-config");
        let xdg_cache_dir = root.join("xdg-cache");
        if fs::create_dir(&hooks_dir).is_err()
            || fs::create_dir(&home_dir).is_err()
            || fs::create_dir(&xdg_config_dir).is_err()
            || fs::create_dir(&xdg_cache_dir).is_err()
        {
            let _ = fs::remove_dir_all(&root);
            return Err("Git hooks sandbox could not be created".to_string());
        }
        let config_file = root.join("config");
        let attributes_file = root.join("attributes");
        if fs::write(&config_file, b"").is_err() || fs::write(&attributes_file, b"").is_err() {
            let _ = fs::remove_dir_all(&root);
            return Err("Git config sandbox could not be created".to_string());
        }
        if check_deadline(deadline) {
            let _ = fs::remove_dir_all(&root);
            return Err("Git sandbox setup exceeded the operation deadline".to_string());
        }
        Ok(Self {
            root,
            hooks_dir,
            config_file,
            home_dir,
            xdg_config_dir,
            xdg_cache_dir,
            attributes_file,
        })
    }
}

impl Drop for GitSandbox {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn apply_git_policy(
    command: &mut Command,
    policy: &GitExecutionPolicy,
    sandbox: &GitSandbox,
    executable: &Path,
) {
    let path = explicit_child_path(executable);
    let allowed_protocols = match policy {
        GitExecutionPolicy::ReadOnly => "https:ssh",
        GitExecutionPolicy::AuthorizedMutation {
            remote: Some(remote),
            ..
        } => match remote.transport() {
            RemoteTransport::Local | RemoteTransport::File => "file",
            RemoteTransport::Https => "https",
            RemoteTransport::Ssh => "ssh",
        },
        GitExecutionPolicy::ServiceMutation {
            remote: Some(remote),
            ..
        } => match remote.transport() {
            RemoteTransport::Local | RemoteTransport::File => "file",
            RemoteTransport::Https => "https",
            RemoteTransport::Ssh => "ssh",
        },
        GitExecutionPolicy::AuthorizedMutation { remote: None, .. } => "https:ssh",
        GitExecutionPolicy::ServiceMutation { remote: None, .. } => "https:ssh",
    };
    command
        .env_clear()
        .env("PATH", path)
        .env("HOME", &sandbox.home_dir)
        .env("USERPROFILE", &sandbox.home_dir)
        .env("XDG_CONFIG_HOME", &sandbox.xdg_config_dir)
        .env("XDG_CACHE_HOME", &sandbox.xdg_cache_dir)
        .env("TMPDIR", &sandbox.root)
        .env("TEMP", &sandbox.root)
        .env("TMP", &sandbox.root)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_PAGER", "cat")
        .env("GIT_EDITOR", ":")
        .env("GIT_SEQUENCE_EDITOR", ":")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", &sandbox.config_file)
        .env("GIT_CONFIG_SYSTEM", &sandbox.config_file)
        .env("GIT_CONFIG_COUNT", "0")
        .env("GIT_ATTR_NOSYSTEM", "1")
        .env("GIT_ATTRIBUTES_FILE", &sandbox.attributes_file)
        .env("GIT_ALLOW_PROTOCOL", allowed_protocols)
        .env("LC_ALL", "C")
        .env("LANG", "C");

    #[cfg(windows)]
    if Path::new(r"C:\Windows").is_dir() {
        command.env("SystemRoot", r"C:\Windows");
    }
}

fn explicit_child_path(executable: &Path) -> OsString {
    let mut directories = Vec::new();
    if let Some(parent) = executable.parent() {
        directories.push(parent.to_path_buf());
    }
    #[cfg(windows)]
    {
        for path in [
            PathBuf::from(r"C:\Windows\System32"),
            PathBuf::from(r"C:\Windows\System32\OpenSSH"),
        ] {
            if path.is_dir() {
                directories.push(path);
            }
        }
    }
    #[cfg(unix)]
    {
        for path in [PathBuf::from("/usr/bin"), PathBuf::from("/bin")] {
            if path.is_dir() {
                directories.push(path);
            }
        }
    }
    env::join_paths(directories).unwrap_or_default()
}

fn apply_git_options(command: &mut Command, sandbox: &GitSandbox, policy: &GitExecutionPolicy) {
    command.args([
        OsString::from("-c"),
        OsString::from(format!("core.hooksPath={}", sandbox.hooks_dir.display())),
        OsString::from("-c"),
        OsString::from("core.fsmonitor=false"),
        OsString::from("-c"),
        OsString::from(format!(
            "core.attributesFile={}",
            sandbox.attributes_file.display()
        )),
        OsString::from("-c"),
        OsString::from("core.askPass="),
        OsString::from("-c"),
        OsString::from("diff.external="),
        OsString::from("-c"),
        OsString::from("credential.helper="),
        OsString::from("-c"),
        OsString::from("http.proxy="),
        OsString::from("-c"),
        OsString::from("https.proxy="),
        OsString::from("-c"),
        OsString::from("protocol.ext.allow=never"),
        OsString::from("-c"),
        OsString::from("protocol.file.allow=never"),
    ]);

    if matches!(
        policy,
        GitExecutionPolicy::AuthorizedMutation {
            remote: Some(RemotePolicy { .. }),
            ..
        } | GitExecutionPolicy::ServiceMutation {
            remote: Some(RemotePolicy { .. }),
            ..
        }
    ) {
        // The environment allow-list and the exact remote policy are the
        // authority for transport selection. This override only prevents a
        // repository-local protocol default from disabling that authority.
        let (remote, remote_name) = match policy {
            GitExecutionPolicy::AuthorizedMutation {
                remote: Some(remote),
                remote_name,
                ..
            }
            | GitExecutionPolicy::ServiceMutation {
                remote: Some(remote),
                remote_name,
                ..
            } => (remote, remote_name),
            _ => unreachable!("remote policy match must carry a remote"),
        };
        {
            command.args([
                OsString::from("-c"),
                OsString::from(format!("protocol.{}.allow=always", remote.transport())),
            ]);
            if remote.transport() == RemoteTransport::Ssh {
                command.args([OsString::from("-c"), OsString::from("core.sshCommand=ssh")]);
            }
            if let Some(remote_name) = remote_name {
                command.args([
                    OsString::from("-c"),
                    OsString::from(format!("remote.{remote_name}.url={}", remote.endpoint())),
                    OsString::from("-c"),
                    OsString::from(format!(
                        "remote.{remote_name}.pushurl={}",
                        remote.endpoint()
                    )),
                ]);
            }
        }
    }
}

fn service_mutation_digest(
    arguments: &[OsString],
    remote: Option<&RemotePolicy>,
    remote_name: Option<&str>,
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"devmanager-git-service-mutation-v1\0");
    hasher.update((arguments.len() as u64).to_le_bytes());
    for argument in arguments {
        update_os_string_digest(&mut hasher, argument);
    }
    if let Some(remote) = remote {
        hasher.update([1]);
        hasher.update((remote.digest_material().len() as u64).to_le_bytes());
        hasher.update(remote.digest_material().as_bytes());
    } else {
        hasher.update([0]);
    }
    if let Some(remote_name) = remote_name {
        hasher.update([1]);
        hasher.update((remote_name.len() as u64).to_le_bytes());
        hasher.update(remote_name.as_bytes());
    } else {
        hasher.update([0]);
    }
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn remote_policy_matches(
    expected: Option<&RemotePolicy>,
    requested: Option<&RemotePolicy>,
) -> bool {
    match (expected, requested) {
        (None, None) => true,
        (Some(expected), Some(requested)) if expected == requested => {
            if matches!(
                expected.transport(),
                RemoteTransport::Local | RemoteTransport::File
            ) {
                match (expected.endpoint_lease(), requested.endpoint_lease()) {
                    (Some(expected), Some(requested)) => Arc::ptr_eq(expected, requested),
                    _ => false,
                }
            } else {
                true
            }
        }
        _ => false,
    }
}

fn operation_label(arguments: &[OsString]) -> String {
    match arguments.first().and_then(|argument| argument.to_str()) {
        Some("add") => "stage".to_string(),
        Some("restore") => "unstage".to_string(),
        Some("commit") => "commit".to_string(),
        Some("push") => "push".to_string(),
        Some("pull") => "pull".to_string(),
        Some("fetch") => "fetch".to_string(),
        Some("switch") | Some("branch") => "branch".to_string(),
        Some("diff") => "diff".to_string(),
        Some("hash-object") => "hash-object".to_string(),
        Some("rev-parse") => "repository validation".to_string(),
        Some("status") => "status".to_string(),
        _ => "command".to_string(),
    }
}

fn graph_transition_for(arguments: &[OsString], policy: &GitExecutionPolicy) -> GraphTransition {
    let command = arguments.first().and_then(|argument| argument.to_str());
    if matches!(policy, GitExecutionPolicy::ReadOnly) {
        return if command == Some("status") {
            GraphTransition::StatusRefresh
        } else {
            GraphTransition::ReadOnly
        };
    }
    match command {
        Some("status") => GraphTransition::StatusRefresh,
        Some("add") | Some("restore") => GraphTransition::Stage,
        Some("commit") => GraphTransition::Commit,
        Some("reset") => GraphTransition::Reset,
        Some("branch") | Some("switch") => GraphTransition::Branch,
        Some("fetch") => GraphTransition::Fetch,
        Some("pull") => GraphTransition::Pull,
        Some("push") => GraphTransition::Push,
        _ => GraphTransition::ReadOnly,
    }
}

/// Resolve the trusted Git installation without starting an ambient process.
pub fn git_available() -> bool {
    TrustedExecutable::resolve_git_with_deadline(OperationDeadline::from_now(Duration::from_secs(
        1,
    )))
    .is_ok()
}

fn validate_argument_budget(arguments: &[OsString]) -> Result<(), GitError> {
    if arguments.len() > HARD_MAX_ARGUMENTS {
        return Err(GitError::InvalidRequest {
            message: format!("Git argument count exceeds the {HARD_MAX_ARGUMENTS}-argument limit"),
        });
    }
    let bytes = arguments
        .iter()
        .map(argument_exact_len)
        .try_fold(0usize, |total, length| total.checked_add(length))
        .ok_or_else(|| GitError::InvalidRequest {
            message: "Git argument byte size overflowed".to_string(),
        })?;
    if bytes > HARD_MAX_ARGUMENT_BYTES {
        return Err(GitError::InvalidRequest {
            message: format!(
                "Git argument byte size exceeds the {HARD_MAX_ARGUMENT_BYTES}-byte limit"
            ),
        });
    }
    if arguments.iter().any(argument_contains_nul) {
        return Err(GitError::InvalidRequest {
            message: "Git arguments must be NUL-free".to_string(),
        });
    }
    Ok(())
}

fn argument_exact_len(argument: &OsString) -> usize {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        argument.as_os_str().as_bytes().len()
    }
    #[cfg(windows)]
    {
        argument.as_os_str().encode_wide().count() * 2
    }
    #[cfg(not(any(unix, windows)))]
    {
        argument.to_string_lossy().len()
    }
}

fn argument_contains_nul(argument: &OsString) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        argument.as_os_str().as_bytes().contains(&0)
    }
    #[cfg(windows)]
    {
        argument.as_os_str().encode_wide().any(|unit| unit == 0)
    }
    #[cfg(not(any(unix, windows)))]
    {
        argument.to_string_lossy().contains('\0')
    }
}

fn service_mutation_allowed(arguments: &[OsString]) -> bool {
    match arguments.first().and_then(|argument| argument.to_str()) {
        Some("add") | Some("commit") | Some("fetch") | Some("pull") | Some("push")
        | Some("reset") | Some("restore") | Some("switch") | Some("branch") => true,
        _ => false,
    }
}

fn service_mutation_requires_remote_policy(arguments: &[OsString]) -> bool {
    matches!(
        arguments.first().and_then(|argument| argument.to_str()),
        Some("fetch") | Some("pull") | Some("push")
    )
}

fn service_remote_binding_matches(
    arguments: &[OsString],
    remote: &RemotePolicy,
    remote_name: &str,
) -> bool {
    let command = arguments.first().and_then(|argument| argument.to_str());
    let mut after_separator = false;
    let mut positional = Vec::new();
    for argument in arguments.iter().skip(1) {
        let Some(argument) = argument.to_str() else {
            return false;
        };
        if after_separator {
            positional.push(argument);
        } else if argument == "--" {
            after_separator = true;
        } else if argument.starts_with('-') {
            let allowed = match command {
                Some("push") => argument == "--set-upstream",
                Some("pull") => argument == "--rebase",
                Some("fetch") => false,
                _ => false,
            };
            if !allowed {
                // Never let an option value be mistaken for the remote
                // selector.  The service seam accepts only its typed,
                // argument-array forms; future flags need an explicit
                // authority/parser update rather than becoming ambient.
                return false;
            }
        } else {
            positional.push(argument);
        }
    }
    if after_separator {
        command == Some("push")
            && positional
                .first()
                .is_some_and(|endpoint| *endpoint == remote.endpoint())
    } else {
        positional.first().is_some_and(|name| *name == remote_name)
    }
}

fn capability_matches_arguments(capability: GitCapability, arguments: &[OsString]) -> bool {
    let command = arguments.first().and_then(|argument| argument.to_str());
    match capability {
        GitCapability::Stage => command == Some("add"),
        GitCapability::Unstage => command == Some("restore"),
        GitCapability::Commit => command == Some("commit"),
        GitCapability::Push => command == Some("push"),
        GitCapability::CreatePullRequest => false,
    }
}

#[cfg(windows)]
fn verify_windows_child_image(
    pid: u32,
    expected: &TrustedExecutable,
    deadline: OperationDeadline,
) -> Result<(), String> {
    if deadline.is_expired() {
        return Err("spawned Git image verification exceeded the operation deadline".to_string());
    }
    let process = unsafe {
        OpenProcess(
            PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
            0,
            pid,
        )
    };
    if process.is_null() {
        return Err("spawned Git image cannot be opened for identity".to_string());
    }
    let mut buffer = vec![0u16; 32_768];
    let mut length = buffer.len() as u32;
    let result =
        unsafe { QueryFullProcessImageNameW(process, 0, buffer.as_mut_ptr(), &mut length) };
    let close_result = unsafe { CloseHandle(process) };
    if result == 0 || close_result == 0 {
        return Err("spawned Git image identity could not be queried".to_string());
    }
    if deadline.is_expired() {
        return Err("spawned Git image verification exceeded the operation deadline".to_string());
    }
    let image = PathBuf::from(OsString::from_wide(&buffer[..length as usize]));
    let canonical = fs::canonicalize(&image)
        .map_err(|_| "spawned Git image path could not be canonicalized".to_string())?;
    if !same_path(&canonical, &expected.path) {
        return Err("spawned image path does not match the trusted executable".to_string());
    }
    #[cfg(test)]
    let identity = if expected.test_fixture {
        test_file_identity_with_deadline(&canonical, deadline)?
    } else {
        file_identity_with_deadline(&canonical, deadline)?
    };
    #[cfg(not(test))]
    let identity = file_identity_with_deadline(&canonical, deadline)?;
    if identity != expected.identity {
        return Err(
            "spawned image file identity does not match the trusted executable".to_string(),
        );
    }
    Ok(())
}

struct ManagedGitChild {
    child: Child,
    /// Keep the exact executable file/ancestor binding alive until the child
    /// settles.  On Unix this preserves the `/proc/self/fd` launch target
    /// during setup; on Windows it preserves the no-delete-share image and
    /// ancestor handles through the complete Job lifetime.
    executable_binding: ExecutableBinding,
    /// The sealed authority is owned by the child guard itself, so a live
    /// WorkspaceService lease cannot be shortened while any process remains.
    authority: GitRepositoryAuthority,
    /// Keep the exact authority/cwd graph alive until the child is settled.
    root: RepositoryRoot,
    /// Local/file remotes retain their endpoint and ancestor handles for the
    /// same child lifetime as the repository graph.
    endpoint: Option<Arc<RemoteEndpointLease>>,
    deadline: OperationDeadline,
    settled: bool,
    #[cfg(windows)]
    job: Option<ManagedProcessJob>,
}

impl ManagedGitChild {
    fn spawn(
        mut command: Command,
        executable: &TrustedExecutable,
        operation: &str,
        graph_transition: GraphTransition,
        deadline: OperationDeadline,
        root: &RepositoryRoot,
        executable_binding: ExecutableBinding,
        authority: GitRepositoryAuthority,
        endpoint: Option<Arc<RemoteEndpointLease>>,
    ) -> Result<Self, GitError> {
        if deadline.is_expired() {
            return Err(GitError::TimedOut {
                operation: operation.to_string(),
                timeout: deadline.timeout,
            });
        }
        let mut child = command.spawn().map_err(|error| GitError::CommandStart {
            operation: operation.to_string(),
            message: sanitize_message(&error.to_string(), Some(&root.path)),
        })?;
        if deadline.is_expired() {
            let cleanup = cleanup_unmanaged_child(&mut child, deadline);
            return Err(command_start_with_cleanup(
                operation,
                "Git process spawn exceeded the operation deadline",
                cleanup,
                &root.path,
            ));
        }
        if let Err(error) =
            root.revalidate_during_transition_with_deadline(graph_transition, deadline)
        {
            let cleanup = cleanup_unmanaged_child(&mut child, deadline);
            return Err(command_start_with_cleanup(
                operation, &error, cleanup, &root.path,
            ));
        }

        #[cfg(windows)]
        {
            if let Err(error) = verify_windows_child_image(child.id(), executable, deadline) {
                let cleanup = cleanup_unmanaged_child(&mut child, deadline);
                return Err(command_start_with_cleanup(
                    operation, &error, cleanup, &root.path,
                ));
            }
            if deadline.is_expired() {
                let cleanup = cleanup_unmanaged_child(&mut child, deadline);
                return Err(command_start_with_cleanup(
                    operation,
                    "Git process setup exceeded the operation deadline",
                    cleanup,
                    &root.path,
                ));
            }
            if let Err(error) =
                root.revalidate_during_transition_with_deadline(graph_transition, deadline)
            {
                let cleanup = cleanup_unmanaged_child(&mut child, deadline);
                return Err(command_start_with_cleanup(
                    operation, &error, cleanup, &root.path,
                ));
            }
            let job = match claim_suspended_process(child.id()) {
                Ok(Some(job)) => job,
                Ok(None) => {
                    let cleanup = cleanup_unmanaged_child(&mut child, deadline);
                    return Err(command_start_with_cleanup(
                        operation,
                        "managed Windows Job was unavailable",
                        cleanup,
                        &root.path,
                    ));
                }
                Err(error) => {
                    let cleanup = cleanup_unmanaged_child(&mut child, deadline);
                    return Err(command_start_with_cleanup(
                        operation, &error, cleanup, &root.path,
                    ));
                }
            };
            let mut managed = Self {
                child,
                executable_binding,
                authority,
                root: root.clone(),
                endpoint,
                deadline,
                settled: false,
                job: Some(job),
            };
            if deadline.is_expired() {
                let cleanup = managed.cleanup(deadline);
                return Err(command_start_with_cleanup(
                    operation,
                    "Git process resume exceeded the operation deadline",
                    cleanup,
                    &root.path,
                ));
            }
            Ok(managed)
        }

        #[cfg(not(windows))]
        {
            let _ = deadline;
            Ok(Self {
                child,
                executable_binding,
                authority,
                root: root.clone(),
                endpoint,
                deadline,
                settled: false,
            })
        }
    }

    fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        self.child.try_wait()
    }

    fn authority_is_live(&self) -> bool {
        match &self.authority {
            GitRepositoryAuthority::Host(binding) => binding.capability.is_live(),
            #[cfg(test)]
            GitRepositoryAuthority::Test => true,
        }
    }

    fn release_job(&mut self) {
        #[cfg(windows)]
        drop(self.job.take());
    }

    fn cleanup(&mut self, deadline: OperationDeadline) -> Option<String> {
        self.cleanup_with_deadline(deadline.with_cleanup_reserve())
    }

    fn cleanup_with_deadline(&mut self, deadline: OperationDeadline) -> Option<String> {
        let mut errors = Vec::new();

        #[cfg(windows)]
        {
            if let Some(job) = self.job.as_ref() {
                if let Err(error) = terminate_windows_job(job) {
                    errors.push(error);
                }
            } else {
                errors.push(
                    "managed Windows Job authority was unavailable; child cleanup refused"
                        .to_string(),
                );
            }
        }

        #[cfg(unix)]
        if let Some(error) = terminate_unix_process_group(self.child.id(), deadline) {
            errors.push(error);
        }

        if let Err(error) = wait_for_child(&mut self.child, deadline) {
            errors.push(error);
        }

        match self.wait_for_settlement(deadline) {
            Ok(()) if errors.is_empty() => self.settled = true,
            Ok(()) => {}
            Err(error) => errors.push(error),
        }
        (!errors.is_empty()).then(|| errors.join("; "))
    }

    fn wait_for_settlement(&self, deadline: OperationDeadline) -> Result<(), String> {
        #[cfg(windows)]
        {
            if let Some(job) = self.job.as_ref() {
                return wait_for_active_process_zero(job, deadline);
            }
            return Ok(());
        }

        #[cfg(unix)]
        {
            return wait_for_unix_process_group_zero(self.child.id(), deadline);
        }

        #[cfg(not(any(unix, windows)))]
        {
            let _ = deadline;
            Ok(())
        }
    }
}

impl Drop for ManagedGitChild {
    fn drop(&mut self) {
        if !self.settled {
            // A setup/error path can drop the guard before the normal runner
            // cleanup. Reuse the owned, bounded process-group/Job cleanup so
            // no child or descendant survives guard destruction.
            let _ = self.cleanup_with_deadline(self.deadline.with_cleanup_reserve());
        }
    }
}

struct ReaderWorker {
    stream: &'static str,
    handle: Option<JoinHandle<io::Result<BoundedRead>>>,
    cancelled: Arc<AtomicBool>,
    cancel_hook: Arc<dyn Fn() + Send + Sync>,
    slot_reserved: bool,
    deadline: OperationDeadline,
    #[cfg(windows)]
    thread_id: Arc<AtomicU32>,
}

type ReaderJoinHandle = JoinHandle<io::Result<BoundedRead>>;

static ACTIVE_READER_SLOTS: AtomicUsize = AtomicUsize::new(0);
static READER_REAPER: OnceLock<Mutex<Vec<ReaderJoinHandle>>> = OnceLock::new();

fn reader_reaper() -> &'static Mutex<Vec<ReaderJoinHandle>> {
    READER_REAPER.get_or_init(|| Mutex::new(Vec::new()))
}

fn reap_finished_reader_workers() {
    let mut finished = Vec::new();
    let mut workers = reader_reaper()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let mut pending = Vec::with_capacity(workers.len());
    for worker in workers.drain(..) {
        if worker.is_finished() {
            finished.push(worker);
        } else {
            pending.push(worker);
        }
    }
    *workers = pending;
    drop(workers);
    for worker in finished {
        let _ = worker.join();
        ACTIVE_READER_SLOTS.fetch_sub(1, Ordering::AcqRel);
    }
}

fn reserve_reader_slot() -> io::Result<()> {
    reap_finished_reader_workers();
    let mut current = ACTIVE_READER_SLOTS.load(Ordering::Acquire);
    loop {
        if current >= HARD_MAX_READER_REAPERS {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "bounded Git reader reaper capacity is exhausted",
            ));
        }
        match ACTIVE_READER_SLOTS.compare_exchange_weak(
            current,
            current + 1,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => return Ok(()),
            Err(observed) => current = observed,
        }
    }
}

fn release_reader_slot() {
    ACTIVE_READER_SLOTS.fetch_sub(1, Ordering::AcqRel);
}

fn defer_reader_worker(handle: ReaderJoinHandle) {
    reap_finished_reader_workers();
    let mut workers = reader_reaper()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    // Every deferred worker still owns one ACTIVE_READER_SLOTS entry. The
    // slot reservation is capped at HARD_MAX_READER_REAPERS, so the current
    // worker always has room in this queue after finished workers are reaped.
    debug_assert!(workers.len() < HARD_MAX_READER_REAPERS);
    workers.push(handle);
}

impl ReaderWorker {
    fn spawn<R: Read + Send + 'static>(
        stream: &'static str,
        reader: R,
        limit: usize,
    ) -> io::Result<Self> {
        Self::spawn_with_deadline(
            stream,
            reader,
            limit,
            OperationDeadline::from_now(READER_DROP_TIMEOUT),
        )
    }

    fn spawn_with_deadline<R: Read + Send + 'static>(
        stream: &'static str,
        reader: R,
        limit: usize,
        deadline: OperationDeadline,
    ) -> io::Result<Self> {
        Self::spawn_with_deadline_and_cancel(stream, reader, limit, deadline, || {})
    }

    fn spawn_with_cancel<R, F>(
        stream: &'static str,
        reader: R,
        limit: usize,
        cancel_hook: F,
    ) -> io::Result<Self>
    where
        R: Read + Send + 'static,
        F: Fn() + Send + Sync + 'static,
    {
        Self::spawn_with_deadline_and_cancel(
            stream,
            reader,
            limit,
            OperationDeadline::from_now(READER_DROP_TIMEOUT),
            cancel_hook,
        )
    }

    fn spawn_with_deadline_and_cancel<R, F>(
        stream: &'static str,
        reader: R,
        limit: usize,
        deadline: OperationDeadline,
        cancel_hook: F,
    ) -> io::Result<Self>
    where
        R: Read + Send + 'static,
        F: Fn() + Send + Sync + 'static,
    {
        reserve_reader_slot()?;
        let cancelled = Arc::new(AtomicBool::new(false));
        let cancel_hook: Arc<dyn Fn() + Send + Sync> = Arc::new(cancel_hook);
        #[cfg(windows)]
        let thread_id = Arc::new(AtomicU32::new(0));
        let cancelled_for_worker = Arc::clone(&cancelled);
        #[cfg(windows)]
        let thread_id_for_worker = Arc::clone(&thread_id);
        let handle = match thread::Builder::new()
            .name(format!("devmanager-git-{stream}-reader"))
            .spawn(move || {
                #[cfg(windows)]
                thread_id_for_worker.store(unsafe { GetCurrentThreadId() }, Ordering::Release);
                read_bounded(reader, limit, &cancelled_for_worker)
            }) {
            Ok(handle) => handle,
            Err(error) => {
                release_reader_slot();
                return Err(error);
            }
        };
        Ok(Self {
            stream,
            handle: Some(handle),
            cancelled,
            cancel_hook,
            slot_reserved: true,
            deadline,
            #[cfg(windows)]
            thread_id,
        })
    }

    fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
        (self.cancel_hook)();
        #[cfg(windows)]
        {
            let thread_id = self.thread_id.load(Ordering::Acquire);
            if thread_id != 0 {
                let thread = unsafe { OpenThread(THREAD_TERMINATE, 0, thread_id) };
                if !thread.is_null() {
                    let _ = unsafe { CancelSynchronousIo(thread) };
                    let _ = unsafe { CloseHandle(thread) };
                }
            }
        }
    }

    fn join(&mut self, deadline: OperationDeadline) -> Result<BoundedRead, String> {
        let stream = self.stream;
        if !self
            .handle
            .as_ref()
            .is_some_and(|handle| handle.is_finished())
        {
            if deadline.is_expired() {
                self.cancel();
            } else {
                while !self
                    .handle
                    .as_ref()
                    .is_some_and(|handle| handle.is_finished())
                {
                    if deadline.is_expired() {
                        self.cancel();
                        break;
                    }
                    deadline.sleep();
                }
            }
        }

        let handle = self
            .handle
            .take()
            .ok_or_else(|| format!("{stream} reader worker was already joined"))?;
        if !handle.is_finished() {
            defer_reader_worker(handle);
            // The bounded reaper owns the handle and its slot now. Keep the
            // slot reserved until the reaper joins the worker, so a
            // permanently blocked reader consumes visible capacity and
            // prevents new operations.
            self.slot_reserved = false;
            return Err(format!("{stream} reader worker retained by bounded reaper"));
        }
        let result = handle.join();
        if self.slot_reserved {
            self.slot_reserved = false;
            release_reader_slot();
        }
        match result {
            Err(_) => Err(format!("{stream} reader worker panicked")),
            Ok(result) => result.map_err(|error| format!("{stream} reader failed: {error}")),
        }
    }
}

impl Drop for ReaderWorker {
    fn drop(&mut self) {
        // Every production pipe is non-blocking on Unix or cancellable with
        // CancelSynchronousIo on Windows. Test-only blocking readers provide
        // an explicit close hook. Cancel first, then use the same bounded
        // join path as the runner; a still-live worker is retained by the
        // bounded visible reaper rather than detached.
        if self.handle.is_some() {
            self.cancel();
            let _ = self.join(self.deadline.with_cleanup_reserve());
        }
    }
}

#[cfg(test)]
#[derive(Clone, Eq, PartialEq)]
pub(crate) struct GitInvocation {
    executable: OsString,
    cwd: PathBuf,
    arguments: Vec<OsString>,
}

#[cfg(test)]
impl fmt::Debug for GitInvocation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("GitInvocation")
            .field("executable", &"<trusted-git>")
            .field("cwd", &"<workspace>")
            .field("argument_count", &self.arguments.len())
            .finish()
    }
}

#[derive(Clone, Eq, PartialEq)]
pub struct PullRequestPlan {
    workspace: WorkspaceIdentity,
    pub provider: PullRequestProvider,
    pub remote: String,
    pub head: BranchName,
    pub base: BranchName,
    pub title: String,
    pub body: String,
    pub expected: crate::git::model::RepoFingerprint,
    executable: OsString,
    arguments: Vec<OsString>,
}

impl fmt::Debug for PullRequestPlan {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PullRequestPlan")
            .field("provider", &self.provider)
            .field("workspace", &"<workspace>")
            .field("argument_count", &self.arguments.len())
            .finish()
    }
}

impl PullRequestPlan {
    pub fn workspace(&self) -> &WorkspaceIdentity {
        &self.workspace
    }

    #[cfg(test)]
    pub(crate) fn executable(&self) -> String {
        self.executable.to_string_lossy().into_owned()
    }

    #[cfg(test)]
    pub(crate) fn arguments(&self) -> Vec<String> {
        self.arguments
            .iter()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect()
    }

    #[cfg(test)]
    pub(crate) fn invocation(&self) -> GitInvocation {
        GitInvocation {
            executable: self.executable.clone(),
            cwd: self.workspace.cwd().to_path_buf(),
            arguments: self.arguments.clone(),
        }
    }
}

impl MutationPlan for PullRequestPlan {
    fn workspace(&self) -> &WorkspaceIdentity {
        &self.workspace
    }

    fn expected_fingerprint(&self) -> &crate::git::model::RepoFingerprint {
        &self.expected
    }

    fn capability(&self) -> GitCapability {
        GitCapability::CreatePullRequest
    }

    fn arguments_for_digest(&self) -> &[OsString] {
        &self.arguments
    }
}

#[derive(Clone)]
pub struct GitRepository {
    root: RepositoryRoot,
    workspace: WorkspaceIdentity,
    limits: GitLimits,
    cancellation: GitCancellation,
    authority: GitRepositoryAuthority,
    read_permit: Arc<Mutex<Option<GitOperationPermit>>>,
}

#[derive(Clone)]
enum GitRepositoryAuthority {
    Host(GitHostBinding),
    /// Raw fixtures are deliberately available only to in-crate tests. No
    /// production constructor can create an authorityless repository.
    #[cfg(test)]
    Test,
}

impl fmt::Debug for GitRepository {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("GitRepository")
            .field("root", &"<repo>")
            .field("workspace", &"<workspace>")
            .field("limits", &self.limits)
            .field("cancellation", &self.cancellation)
            .field("authority", &"<host-binding>")
            .finish()
    }
}

impl GitRepository {
    pub(crate) fn from_optional_host_binding(
        binding: Option<GitHostBinding>,
        cancellation: GitCancellation,
    ) -> Result<Self, GitError> {
        let Some(binding) = binding else {
            return Err(GitError::AuthorityUnavailable);
        };
        Self::from_host_binding(binding, cancellation)
    }

    pub(crate) fn from_host_binding(
        binding: GitHostBinding,
        cancellation: GitCancellation,
    ) -> Result<Self, GitError> {
        if !binding.capability.is_live() {
            return Err(GitError::InvalidRepositoryRoot {
                path: "<bound-root>".to_string(),
                reason: "host Git authority capability is unavailable".to_string(),
            });
        }
        let capability = Arc::clone(&binding.capability);
        let open_deadline = OperationDeadline::from_host_authority(
            capability.authority_deadline,
            capability.limits.timeout,
        );
        let root = RepositoryRoot::open_with_approved_external_roots_and_deadline(
            &capability.root,
            &capability.approved_external_roots,
            open_deadline,
        )
        .map_err(|reason| GitError::InvalidRepositoryRoot {
            path: "<bound-root>".to_string(),
            reason,
        })?;
        let bound_repository_identity = capability
            .repository_identity
            .lock()
            .map_err(|_| GitError::InvalidRepositoryRoot {
                path: "<bound-root>".to_string(),
                reason: "host Git repository identity is unavailable".to_string(),
            })?
            .clone();
        if repository_graph_identity(&root) != bound_repository_identity {
            return Err(GitError::InvalidRepositoryRoot {
                path: "<bound-root>".to_string(),
                reason: "repository graph identity does not match the host binding".to_string(),
            });
        }
        if repository_static_graph_identity(&root) != capability.repository_static_identity {
            return Err(GitError::InvalidRepositoryRoot {
                path: "<bound-root>".to_string(),
                reason: "static repository graph identity does not match the host binding"
                    .to_string(),
            });
        }
        let workspace = WorkspaceIdentity::from_canonical_root(root.path.clone());
        let read_permit = GitOperationPermit::host_read(Arc::clone(&capability));
        Ok(Self {
            root,
            workspace,
            limits: capability.limits.clone().bounded(),
            cancellation,
            authority: GitRepositoryAuthority::Host(binding),
            read_permit: Arc::new(Mutex::new(Some(read_permit))),
        })
    }

    #[cfg(test)]
    pub(crate) fn test_open(root: impl AsRef<Path>) -> Result<Self, GitError> {
        Self::test_open_with_limits(root, GitLimits::default(), GitCancellation::new())
    }

    #[cfg(test)]
    pub(crate) fn test_open_with_approved_external_roots(
        root: impl AsRef<Path>,
        approved_external_roots: Vec<PathBuf>,
    ) -> Result<Self, GitError> {
        let root = RepositoryRoot::open_with_approved_external_roots(
            root.as_ref(),
            &approved_external_roots,
        )
        .map_err(|reason| GitError::InvalidRepositoryRoot {
            path: "<test-root>".to_string(),
            reason,
        })?;
        let workspace = WorkspaceIdentity::from_canonical_root(root.path.clone());
        let limits = GitLimits::default();
        Ok(Self {
            root,
            workspace,
            limits: limits.clone(),
            cancellation: GitCancellation::new(),
            authority: GitRepositoryAuthority::Test,
            read_permit: Arc::new(Mutex::new(Some(
                GitOperationPermit::test_read_with_timeout(limits.timeout),
            ))),
        })
    }

    #[cfg(test)]
    pub(crate) fn test_open_with_limits(
        root: impl AsRef<Path>,
        limits: GitLimits,
        cancellation: GitCancellation,
    ) -> Result<Self, GitError> {
        let root = RepositoryRoot::open(root.as_ref()).map_err(|reason| {
            GitError::InvalidRepositoryRoot {
                path: "<test-root>".to_string(),
                reason,
            }
        })?;
        let workspace = WorkspaceIdentity::from_canonical_root(root.path.clone());
        let limits = limits.bounded();
        Ok(Self {
            root,
            workspace,
            limits: limits.clone(),
            cancellation,
            authority: GitRepositoryAuthority::Test,
            read_permit: Arc::new(Mutex::new(Some(
                GitOperationPermit::test_read_with_timeout(limits.timeout),
            ))),
        })
    }

    #[cfg(test)]
    pub(crate) fn with_limits(root: impl AsRef<Path>, limits: GitLimits) -> Result<Self, GitError> {
        Self::test_open_with_limits(root, limits, GitCancellation::new())
    }

    #[cfg(test)]
    pub(crate) fn with_cancellation(
        root: impl AsRef<Path>,
        cancellation: GitCancellation,
    ) -> Result<Self, GitError> {
        Self::test_open_with_limits(root, GitLimits::default(), cancellation)
    }

    #[cfg(test)]
    pub(crate) fn with_limits_and_cancellation(
        root: impl AsRef<Path>,
        limits: GitLimits,
        cancellation: GitCancellation,
    ) -> Result<Self, GitError> {
        let root = RepositoryRoot::open(root.as_ref()).map_err(|reason| {
            GitError::InvalidRepositoryRoot {
                path: "<requested-root>".to_string(),
                reason,
            }
        })?;

        let workspace = WorkspaceIdentity::from_canonical_root(root.path.clone());
        let limits = limits.bounded();
        let repository = Self {
            root,
            workspace,
            limits: limits.clone(),
            cancellation,
            authority: GitRepositoryAuthority::Test,
            read_permit: Arc::new(Mutex::new(Some(
                GitOperationPermit::test_read_with_timeout(limits.timeout),
            ))),
        };
        let output = repository
            .run_read_args(&[arg("rev-parse"), arg("--show-toplevel")])
            .map_err(|error| GitError::InvalidRepositoryRoot {
                path: "<requested-root>".to_string(),
                reason: error.to_string(),
            })?;
        let reported_output = output.stdout.strip_suffix(b"\n").unwrap_or(&output.stdout);
        let reported_output = reported_output
            .strip_suffix(b"\r")
            .unwrap_or(reported_output);
        let reported = std::fs::canonicalize(String::from_utf8_lossy(reported_output).as_ref())
            .map_err(|_| GitError::InvalidRepositoryRoot {
                path: "<requested-root>".to_string(),
                reason: "Git did not return a canonical worktree root".to_string(),
            })?;
        if !same_path(&reported, &repository.root.path) {
            return Err(GitError::InvalidRepositoryRoot {
                path: "<requested-root>".to_string(),
                reason: "Git worktree root does not match the selected root".to_string(),
            });
        }
        Ok(repository)
    }

    #[cfg(test)]
    pub(crate) fn root(&self) -> &Path {
        &self.root.path
    }

    pub fn workspace(&self) -> &WorkspaceIdentity {
        &self.workspace
    }

    #[cfg(test)]
    fn has_live_authority_for_test(&self) -> bool {
        match &self.authority {
            GitRepositoryAuthority::Host(binding) => binding.has_live_authority_for_test(),
            GitRepositoryAuthority::Test => true,
        }
    }

    fn take_read_permit(&self) -> Result<GitOperationPermit, GitError> {
        let _previous = self
            .read_permit
            .lock()
            .map_err(|_| GitError::AuthorityUnavailable)?
            .take()
            .ok_or(GitError::AuthorityUnavailable)?;

        // A read permit owns one operation deadline, not the repository's
        // lifetime. Always issue a new permit at the operation boundary while
        // retaining the same host authority (and its absolute host expiry);
        // otherwise a slow, bounded graph read would poison every later
        // status/read until the old permit happened to expire.
        match &self.authority {
            GitRepositoryAuthority::Host(binding) => {
                if !binding.capability.is_live() {
                    Err(GitError::AuthorityUnavailable)
                } else {
                    Ok(GitOperationPermit::host_read(Arc::clone(
                        &binding.capability,
                    )))
                }
            }
            #[cfg(test)]
            GitRepositoryAuthority::Test => Ok(GitOperationPermit::test_read_with_timeout(
                self.limits.timeout,
            )),
        }
    }

    fn restore_read_permit(&self, permit: GitOperationPermit) -> Result<(), GitError> {
        let mut slot = self
            .read_permit
            .lock()
            .map_err(|_| GitError::AuthorityUnavailable)?;
        if slot.is_some() {
            return Err(GitError::AuthorityUnavailable);
        }
        *slot = Some(permit);
        Ok(())
    }

    fn with_read_permit<T>(
        &self,
        operation: impl FnOnce(&GitOperationPermit) -> Result<T, GitError>,
    ) -> Result<T, GitError> {
        let permit = self.take_read_permit()?;
        let result = operation(&permit);
        let restore = self.restore_read_permit(permit);
        match (result, restore) {
            (Ok(value), Ok(())) => Ok(value),
            (Err(error), Ok(())) => Err(error),
            (_, Err(error)) => Err(error),
        }
    }

    fn validate_operation_permit(
        &self,
        permit: &GitOperationPermit,
        policy: &GitExecutionPolicy,
        arguments: &[OsString],
    ) -> Result<(), GitError> {
        if !permit.is_live() || !permit.operation_matches_policy(policy, arguments) {
            return Err(GitError::AuthorityUnavailable);
        }
        match (&permit.authority, &self.authority) {
            (
                GitPermitAuthority::Host(permit_capability),
                GitRepositoryAuthority::Host(binding),
            ) => {
                if !Arc::ptr_eq(permit_capability, &binding.capability)
                    || permit_capability.identity.workspace != self.workspace
                    || permit_capability.repository_static_identity
                        != repository_static_graph_identity(&self.root)
                    || permit_capability.identity.repository_static_identity
                        != permit_capability.repository_static_identity
                {
                    return Err(GitError::AuthorityUnavailable);
                }
            }
            #[cfg(test)]
            (GitPermitAuthority::Test, GitRepositoryAuthority::Test) => {}
            _ => return Err(GitError::AuthorityUnavailable),
        }
        Ok(())
    }

    /// Cancel work already admitted for this repository without exposing the
    /// underlying cancellation token to callers.
    pub fn cancel(&self) {
        self.cancellation.cancel();
    }

    pub fn plan_status(&self) -> StatusPlan {
        StatusPlan::new(
            self.workspace.clone(),
            self.limits.max_stdout_bytes,
            vec![
                arg("status"),
                arg("--porcelain=v2"),
                arg("--branch"),
                arg("--untracked-files=all"),
                arg("-z"),
            ],
        )
    }

    pub fn status(&self) -> Result<RepositoryStatus, GitError> {
        let plan = self.plan_status();
        self.with_read_permit(|permit| self.status_with_permit(&plan, permit))
    }

    fn status_with_permit(
        &self,
        plan: &StatusPlan,
        permit: &GitOperationPermit,
    ) -> Result<RepositoryStatus, GitError> {
        // Status is one logical read operation even though Git needs several
        // bounded invocations to build its fingerprint. Reuse the same permit
        // so graph admission, reads, enumeration, and all child effects share
        // one absolute deadline/work budget.
        let output =
            self.run_read_plan_with_permit(plan.workspace(), plan.raw_arguments(), permit)?;
        let mut status =
            crate::git::model::parse_porcelain_v2_z_limited(&output.stdout, plan.max_bytes)
                .map_err(|message| GitError::Parse { message })?;
        for entry in &status.entries {
            validate_repository_path(&self.root.path, &entry.path)?;
            if let Some(original_path) = &entry.original_path {
                validate_repository_path(&self.root.path, original_path)?;
            }
        }
        if permit.deadline.is_expired() {
            return Err(GitError::TimedOut {
                operation: "status".to_string(),
                timeout: permit.deadline.timeout,
            });
        }
        let unstaged = self.run_read_args_with_permit(
            &[
                arg("diff"),
                arg("--binary"),
                arg("--full-index"),
                arg("--no-color"),
                arg("--no-ext-diff"),
                arg("--no-textconv"),
                arg("--"),
            ],
            permit,
        )?;
        let staged = self.run_read_args_with_permit(
            &[
                arg("diff"),
                arg("--cached"),
                arg("--binary"),
                arg("--full-index"),
                arg("--no-color"),
                arg("--no-ext-diff"),
                arg("--no-textconv"),
                arg("--"),
            ],
            permit,
        )?;
        let mut hasher = Sha256::new();
        hasher.update(status.fingerprint.status_digest.as_bytes());
        hasher.update((unstaged.stdout.len() as u64).to_le_bytes());
        hasher.update(&unstaged.stdout);
        hasher.update((staged.stdout.len() as u64).to_le_bytes());
        hasher.update(&staged.stdout);
        for entry in status
            .entries
            .iter()
            .filter(|entry| entry.kind == StatusKind::Untracked)
        {
            entry
                .path
                .validate_relative()
                .map_err(|reason| GitError::InvalidPath {
                    path: entry.path.display_lossy().into_owned(),
                    reason,
                })?;
            let content = self.run_read_args_with_permit(
                &[
                    arg("hash-object"),
                    arg("--no-filters"),
                    arg("--"),
                    entry.path.to_os_string(),
                ],
                permit,
            )?;
            hasher.update((entry.path.as_bytes().len() as u64).to_le_bytes());
            hasher.update(entry.path.as_bytes());
            hasher.update(&content.stdout);
        }
        status.fingerprint.status_digest = hasher
            .finalize()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        if permit.deadline.is_expired() {
            return Err(GitError::TimedOut {
                operation: "status".to_string(),
                timeout: permit.deadline.timeout,
            });
        }
        Ok(status)
    }

    pub fn operation_state(&self) -> Result<(bool, bool), GitError> {
        self.with_read_permit(|permit| {
            if permit.deadline.is_expired() {
                return Err(GitError::TimedOut {
                    operation: "operation state".to_string(),
                    timeout: permit.deadline.timeout,
                });
            }
            self.root
                .revalidate_with_deadline(permit.deadline)
                .map_err(|reason| GitError::InvalidRepositoryRoot {
                    path: "<repository>".to_string(),
                    reason,
                })?;
            let state_exists = |relative| {
                self.root
                    .state_exists_with_deadline(relative, Some(permit.deadline))
                    .map_err(|reason| GitError::InvalidRepositoryRoot {
                        path: "<repository>".to_string(),
                        reason,
                    })
            };
            let state = (
                state_exists("MERGE_HEAD")?,
                state_exists("rebase-merge")? || state_exists("rebase-apply")?,
            );
            self.root
                .revalidate_with_deadline(permit.deadline)
                .map_err(|reason| GitError::InvalidRepositoryRoot {
                    path: "<repository>".to_string(),
                    reason,
                })?;
            if permit.deadline.is_expired() {
                return Err(GitError::TimedOut {
                    operation: "operation state".to_string(),
                    timeout: permit.deadline.timeout,
                });
            }
            Ok(state)
        })
    }

    pub fn plan_diff(&self, staged: bool, max_bytes: usize) -> Result<DiffPlan, GitError> {
        if max_bytes == 0 {
            return Err(GitError::InvalidRequest {
                message: "diff byte bound must be greater than zero".to_string(),
            });
        }
        if max_bytes > HARD_MAX_DIFF_BYTES {
            return Err(GitError::InvalidRequest {
                message: format!("diff byte bound exceeds the {HARD_MAX_DIFF_BYTES}-byte limit"),
            });
        }
        Ok(DiffPlan::new(
            self.workspace.clone(),
            staged,
            max_bytes,
            diff_arguments(staged),
        ))
    }

    pub fn plan_review(&self, staged: bool, max_bytes: usize) -> Result<ReviewPlan, GitError> {
        if max_bytes == 0 {
            return Err(GitError::InvalidRequest {
                message: "review byte bound must be greater than zero".to_string(),
            });
        }
        if max_bytes > HARD_MAX_DIFF_BYTES {
            return Err(GitError::InvalidRequest {
                message: format!("review byte bound exceeds the {HARD_MAX_DIFF_BYTES}-byte limit"),
            });
        }
        Ok(ReviewPlan::new(
            self.workspace.clone(),
            staged,
            max_bytes,
            diff_arguments(staged),
        ))
    }

    pub fn diff(&self, staged: bool, max_bytes: usize) -> Result<DiffDocument, GitError> {
        let plan = self.plan_diff(staged, max_bytes)?;
        self.execute_diff_plan(&plan)
    }

    pub fn review(&self, plan: &ReviewPlan) -> Result<DiffDocument, GitError> {
        self.execute_review_plan(plan)
    }

    pub fn plan_stage(&self, files: &[RepoPath]) -> Result<StagePlan, GitError> {
        let status = self.status()?;
        let files = validate_files(&self.root.path, &status, files)?;
        let mut arguments = vec![arg("add"), arg("--")];
        arguments.extend(files.iter().map(RepoPath::to_os_string));
        Ok(StagePlan::new(
            self.workspace.clone(),
            files,
            status.fingerprint,
            arguments,
        ))
    }

    /// The legacy self-authorization entry point is retained only as a sealed
    /// fail-closed shim while the host issuer is integrated.  It cannot mint a
    /// permit from a repository path or plan alone.
    pub(crate) fn confirm<P: MutationPlan>(&self, _plan: &P) -> Result<GitConfirmation, GitError> {
        Err(GitError::AuthorityUnavailable)
    }

    #[cfg(test)]
    pub(crate) fn test_confirm<P: MutationPlan>(
        &self,
        plan: &P,
    ) -> Result<GitConfirmation, GitError> {
        if plan.workspace() != &self.workspace {
            return Err(GitError::WorkspaceMismatch {
                expected: self.workspace.id().to_string(),
                actual: plan.workspace().id().to_string(),
            });
        }
        let mut gate = GitCapabilityGate::new([plan.capability()]);
        if let Some(remote) = plan.remote_policy() {
            gate.authorize_remote(remote.clone());
        }
        let permit = match &self.authority {
            GitRepositoryAuthority::Host(binding) => {
                GitOperationPermit::host_mutation(Arc::clone(&binding.capability), plan)
            }
            GitRepositoryAuthority::Test => {
                GitOperationPermit::test_mutation_with_timeout(plan, self.limits.timeout)
            }
        };
        gate.confirm(plan, permit)
    }

    pub fn stage(&self, plan: &StagePlan, confirmation: &GitConfirmation) -> Result<(), GitError> {
        self.execute_expected(plan, confirmation).map(|_| ())
    }

    pub fn plan_unstage(&self, files: &[RepoPath]) -> Result<UnstagePlan, GitError> {
        let status = self.status()?;
        let files = validate_files(&self.root.path, &status, files)?;
        let mut arguments = vec![arg("restore"), arg("--staged"), arg("--")];
        arguments.extend(files.iter().map(RepoPath::to_os_string));
        Ok(UnstagePlan::new(
            self.workspace.clone(),
            files,
            status.fingerprint,
            arguments,
        ))
    }

    pub fn unstage(
        &self,
        plan: &UnstagePlan,
        confirmation: &GitConfirmation,
    ) -> Result<(), GitError> {
        self.execute_expected(plan, confirmation).map(|_| ())
    }

    pub fn plan_commit(&self, message: impl Into<String>) -> Result<CommitPlan, GitError> {
        let message = message.into();
        if message.trim().is_empty() || message.contains('\0') {
            return Err(GitError::InvalidRequest {
                message: "commit message must be non-empty and NUL-free".to_string(),
            });
        }
        if message.len() > 64 * 1024 {
            return Err(GitError::InvalidRequest {
                message: "commit message exceeds the 64KiB bound".to_string(),
            });
        }

        let status = self.status()?;
        let files = status
            .entries
            .iter()
            .filter(|entry| entry.is_staged())
            .map(|entry| entry.path.clone())
            .collect::<Vec<_>>();
        if files.is_empty() {
            return Err(GitError::InvalidRequest {
                message: "cannot commit without staged files".to_string(),
            });
        }
        let arguments = vec![arg("commit"), arg("-m"), OsString::from(message.as_str())];
        Ok(CommitPlan::new(
            self.workspace.clone(),
            files,
            message,
            status.fingerprint,
            arguments,
        ))
    }

    pub fn commit(
        &self,
        plan: &CommitPlan,
        confirmation: &GitConfirmation,
    ) -> Result<(), GitError> {
        self.execute_expected(plan, confirmation).map(|_| ())
    }

    pub fn plan_push(
        &self,
        remote: Option<&str>,
        branch: Option<&str>,
    ) -> Result<PushPlan, GitError> {
        let status = self.status()?;
        let branch = branch
            .map(|branch| {
                BranchName::new(branch).map_err(|message| GitError::InvalidRequest { message })
            })
            .transpose()?
            .or_else(|| status.branch.clone())
            .ok_or_else(|| GitError::InvalidRequest {
                message: "cannot push a detached HEAD without an explicit branch".to_string(),
            })?;
        let (remote, set_upstream) = match remote {
            Some(remote) if !remote.is_empty() => {
                let expected_upstream = format!("{remote}/{}", branch.as_str());
                (
                    remote.to_string(),
                    status.upstream.as_deref() != Some(expected_upstream.as_str()),
                )
            }
            Some(_) => {
                return Err(GitError::InvalidRequest {
                    message: "push remote must be non-empty".to_string(),
                })
            }
            None => {
                let upstream = status
                    .upstream
                    .as_deref()
                    .ok_or_else(|| GitError::NoUpstream {
                        branch: status.branch.clone(),
                    })?;
                let (remote, _) = upstream
                    .split_once('/')
                    .ok_or_else(|| GitError::NoUpstream {
                        branch: status.branch.clone(),
                    })?;
                (remote.to_string(), false)
            }
        };
        validate_remote(&remote, "push remote")?;
        let remote_policy = self.resolve_remote_policy(&remote)?;

        let mut arguments = vec![arg("push")];
        if set_upstream {
            arguments.push(arg("--set-upstream"));
        }
        arguments.extend([
            arg("--"),
            OsString::from(remote_policy.endpoint()),
            OsString::from(branch.as_str()),
        ]);
        Ok(PushPlan::new(
            self.workspace.clone(),
            remote,
            branch,
            set_upstream,
            status.fingerprint,
            remote_policy,
            arguments,
        ))
    }

    pub fn plan_pull_request(
        &self,
        provider: PullRequestProvider,
        remote: &str,
        head: Option<&str>,
        base: &str,
        title: impl Into<String>,
        body: impl Into<String>,
    ) -> Result<PullRequestPlan, GitError> {
        validate_remote(remote, "pull request repository")?;
        let title = validate_text(title.into(), "pull request title", 256)?;
        let body = validate_text(body.into(), "pull request body", 256 * 1024)?;
        let status = self.status()?;
        let head = head
            .map(|head| {
                BranchName::new(head).map_err(|message| GitError::InvalidRequest { message })
            })
            .transpose()?
            .or_else(|| status.branch.clone())
            .ok_or_else(|| GitError::InvalidRequest {
                message:
                    "cannot create a pull request from detached HEAD without an explicit branch"
                        .to_string(),
            })?;
        let base = BranchName::new(base).map_err(|message| GitError::InvalidRequest { message })?;
        let (executable, arguments) = match provider {
            PullRequestProvider::GitHub => (
                "gh",
                vec![
                    arg("pr"),
                    arg("create"),
                    arg("--repo"),
                    OsString::from(remote),
                    arg("--head"),
                    OsString::from(head.as_str()),
                    arg("--base"),
                    OsString::from(base.as_str()),
                    arg("--title"),
                    OsString::from(title.as_str()),
                    arg("--body"),
                    OsString::from(body.as_str()),
                ],
            ),
            PullRequestProvider::GitLab => (
                "glab",
                vec![
                    arg("mr"),
                    arg("create"),
                    arg("--repo"),
                    OsString::from(remote),
                    arg("--source-branch"),
                    OsString::from(head.as_str()),
                    arg("--target-branch"),
                    OsString::from(base.as_str()),
                    arg("--title"),
                    OsString::from(title.as_str()),
                    arg("--description"),
                    OsString::from(body.as_str()),
                ],
            ),
            PullRequestProvider::Bitbucket => (
                "bb",
                vec![
                    arg("pr"),
                    arg("create"),
                    arg("--repository"),
                    OsString::from(remote),
                    arg("--source"),
                    OsString::from(head.as_str()),
                    arg("--destination"),
                    OsString::from(base.as_str()),
                    arg("--title"),
                    OsString::from(title.as_str()),
                    arg("--description"),
                    OsString::from(body.as_str()),
                ],
            ),
            PullRequestProvider::AzureDevOps => (
                "az",
                vec![
                    arg("repos"),
                    arg("pr"),
                    arg("create"),
                    arg("--repository"),
                    OsString::from(remote),
                    arg("--source-branch"),
                    OsString::from(head.as_str()),
                    arg("--target-branch"),
                    OsString::from(base.as_str()),
                    arg("--title"),
                    OsString::from(title.as_str()),
                    arg("--description"),
                    OsString::from(body.as_str()),
                ],
            ),
        };
        Ok(PullRequestPlan {
            workspace: self.workspace.clone(),
            provider,
            remote: remote.to_string(),
            head,
            base,
            title,
            body,
            expected: status.fingerprint,
            executable: OsString::from(executable),
            arguments,
        })
    }

    pub fn push(&self, plan: &PushPlan, confirmation: &GitConfirmation) -> Result<(), GitError> {
        self.execute_expected(plan, confirmation).map(|_| ())
    }

    fn execute_expected<P: MutationPlan>(
        &self,
        plan: &P,
        confirmation: &GitConfirmation,
    ) -> Result<GitOutput, GitError> {
        if confirmation.capability != plan.capability()
            || confirmation.workspace != self.workspace
            || confirmation.plan_digest != plan.plan_digest()
            || !remote_policy_matches(confirmation.remote_policy.as_ref(), plan.remote_policy())
        {
            return Err(GitError::ConfirmationMismatch {
                capability: plan.capability(),
            });
        }
        if !confirmation.permit.is_live() || !confirmation.permit.plan_matches(plan) {
            return Err(GitError::AuthorityUnavailable);
        }
        let actual = self.status()?.fingerprint;
        if &actual != plan.expected_fingerprint() {
            return Err(GitError::FingerprintMismatch {
                expected: plan.expected_fingerprint().clone(),
                actual,
            });
        }
        let execution_permit = confirmation.permit.renewed_for_execution()?;
        self.run_mutation_args(
            plan.arguments_for_digest(),
            plan.capability(),
            plan.remote_policy().cloned(),
            plan.remote_name().map(str::to_string),
            &execution_permit,
        )
    }

    fn resolve_remote_policy(&self, remote: &str) -> Result<RemotePolicy, GitError> {
        self.with_read_permit(|permit| self.resolve_remote_policy_with_permit(remote, permit))
    }

    fn resolve_remote_policy_with_permit(
        &self,
        remote: &str,
        permit: &GitOperationPermit,
    ) -> Result<RemotePolicy, GitError> {
        validate_remote(remote, "push remote")?;
        validate_remote_name(remote)?;
        let pushurl_key = format!("remote.{remote}.pushurl");
        let pushurls = self.read_remote_values_with_permit(&pushurl_key, permit)?;
        let value = if pushurls.is_empty() {
            let urls =
                self.read_remote_values_with_permit(&format!("remote.{remote}.url"), permit)?;
            if urls.len() != 1 {
                return Err(GitError::InvalidRequest {
                    message: "push remote must have exactly one configured endpoint".to_string(),
                });
            }
            urls.into_iter().next().unwrap()
        } else if pushurls.len() == 1 {
            pushurls.into_iter().next().unwrap()
        } else {
            return Err(GitError::InvalidRequest {
                message: "push remote must have exactly one configured push endpoint".to_string(),
            });
        };
        remote_policy_from_url_with_deadline(&self.root.path, &value, Some(permit.deadline))
    }

    fn read_remote_values_with_permit(
        &self,
        key: &str,
        permit: &GitOperationPermit,
    ) -> Result<Vec<String>, GitError> {
        let output = self.run_read_args_with_permit(
            &[
                arg("config"),
                arg("--local"),
                arg("--get-all"),
                OsString::from(key),
            ],
            permit,
        );
        match output {
            Ok(output) => Ok(String::from_utf8_lossy(&output.stdout)
                .lines()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_string)
                .collect()),
            Err(GitError::CommandFailed { .. }) => Ok(Vec::new()),
            Err(error) => Err(error),
        }
    }

    fn run_read_plan(
        &self,
        workspace: &WorkspaceIdentity,
        arguments: &[OsString],
    ) -> Result<GitOutput, GitError> {
        self.with_read_permit(|permit| self.run_read_plan_with_permit(workspace, arguments, permit))
    }

    fn run_read_plan_with_permit(
        &self,
        workspace: &WorkspaceIdentity,
        arguments: &[OsString],
        permit: &GitOperationPermit,
    ) -> Result<GitOutput, GitError> {
        if workspace != &self.workspace {
            return Err(GitError::WorkspaceMismatch {
                expected: self.workspace.id().to_string(),
                actual: workspace.id().to_string(),
            });
        }
        self.run_read_args_with_permit(arguments, permit)
    }

    fn execute_diff_plan(&self, plan: &DiffPlan) -> Result<DiffDocument, GitError> {
        let output = self.run_read_plan(plan.workspace(), plan.raw_arguments())?;
        parse_unified_diff_limited(&output.stdout, plan.max_bytes)
            .map_err(|message| GitError::Parse { message })
    }

    fn execute_review_plan(&self, plan: &ReviewPlan) -> Result<DiffDocument, GitError> {
        let output = self.run_read_plan(plan.workspace(), plan.raw_arguments())?;
        parse_unified_diff_limited(&output.stdout, plan.max_bytes)
            .map_err(|message| GitError::Parse { message })
    }

    fn run_read_args(&self, arguments: &[OsString]) -> Result<GitOutput, GitError> {
        self.with_read_permit(|permit| self.run_read_args_with_permit(arguments, permit))
    }

    fn run_read_args_with_permit(
        &self,
        arguments: &[OsString],
        permit: &GitOperationPermit,
    ) -> Result<GitOutput, GitError> {
        self.run_invocation(None, arguments, GitExecutionPolicy::ReadOnly, true, permit)
    }

    /// Internal bridge for the legacy presentation adapter. The adapter never
    /// receives a process handle or a caller-owned executable; all reads still
    /// pass through this repository's bound root, graph, sandbox, and runner.
    pub(crate) fn run_service_read(&self, arguments: Vec<OsString>) -> Result<Vec<u8>, GitError> {
        Ok(self.run_read_args(&arguments)?.stdout)
    }

    pub(crate) fn validate_service_path(&self, path: &RepoPath) -> Result<(), GitError> {
        self.with_read_permit(|permit| {
            if permit.deadline.is_expired() {
                return Err(GitError::TimedOut {
                    operation: "service path validation".to_string(),
                    timeout: permit.deadline.timeout,
                });
            }
            self.root
                .revalidate_with_deadline(permit.deadline)
                .map_err(|reason| GitError::InvalidRepositoryRoot {
                    path: "<repository>".to_string(),
                    reason,
                })?;
            validate_repository_path(&self.root.path, path)?;
            if permit.deadline.is_expired() {
                return Err(GitError::TimedOut {
                    operation: "service path validation".to_string(),
                    timeout: permit.deadline.timeout,
                });
            }
            Ok(())
        })
    }

    pub(crate) fn validate_service_paths(&self, paths: &[RepoPath]) -> Result<(), GitError> {
        let status = self.status()?;
        self.with_read_permit(|permit| {
            if permit.deadline.is_expired() {
                return Err(GitError::TimedOut {
                    operation: "service path validation".to_string(),
                    timeout: permit.deadline.timeout,
                });
            }
            self.root
                .revalidate_with_deadline(permit.deadline)
                .map_err(|reason| GitError::InvalidRepositoryRoot {
                    path: "<repository>".to_string(),
                    reason,
                })?;
            let _ = validate_files(&self.root.path, &status, paths)?;
            if permit.deadline.is_expired() {
                return Err(GitError::TimedOut {
                    operation: "service path validation".to_string(),
                    timeout: permit.deadline.timeout,
                });
            }
            Ok(())
        })
    }

    pub(crate) fn service_remote_policy(&self, remote: &str) -> Result<RemotePolicy, GitError> {
        self.resolve_remote_policy(remote)
    }

    pub(crate) fn read_service_file(
        &self,
        path: &RepoPath,
        max_bytes: usize,
    ) -> Result<Vec<u8>, GitError> {
        path.validate_relative()
            .map_err(|reason| GitError::InvalidPath {
                path: path.display_lossy().into_owned(),
                reason,
            })?;
        if max_bytes == 0 || max_bytes > HARD_MAX_DIFF_BYTES {
            return Err(GitError::InvalidRequest {
                message: "Git file read bound is outside the immutable limit".to_string(),
            });
        }
        self.with_read_permit(|permit| {
            self.root
                .revalidate_with_deadline(permit.deadline)
                .map_err(|reason| GitError::InvalidRepositoryRoot {
                    path: "<repository>".to_string(),
                    reason,
                })?;
            let candidate = self.root.path.join(path.to_path_buf());
            validate_repository_path(&self.root.path, path)?;
            if permit.deadline.is_expired() {
                return Err(GitError::TimedOut {
                    operation: "file read".to_string(),
                    timeout: permit.deadline.timeout,
                });
            }
            let metadata = fs::symlink_metadata(&candidate).map_err(|_| GitError::InvalidPath {
                path: path.display_lossy().into_owned(),
                reason: "repository file is unavailable".to_string(),
            })?;
            if !metadata.is_file() {
                return Err(GitError::InvalidPath {
                    path: path.display_lossy().into_owned(),
                    reason: "repository path is not a regular file".to_string(),
                });
            }
            let _ = data_file_identity_with_deadline(&candidate, permit.deadline).map_err(
                |reason| GitError::InvalidPath {
                    path: path.display_lossy().into_owned(),
                    reason,
                },
            )?;
            let bytes =
                read_file_bounded_with_deadline(&candidate, max_bytes, Some(permit.deadline))
                    .map_err(|error| {
                        if error.kind() == io::ErrorKind::FileTooLarge {
                            GitError::OutputLimitExceeded {
                                stream: "file",
                                limit: max_bytes,
                            }
                        } else {
                            GitError::InvalidPath {
                                path: path.display_lossy().into_owned(),
                                reason: "repository file cannot be read safely".to_string(),
                            }
                        }
                    })?;
            if permit.deadline.is_expired() {
                return Err(GitError::TimedOut {
                    operation: "file read".to_string(),
                    timeout: permit.deadline.timeout,
                });
            }
            Ok(bytes)
        })
    }

    /// Internal bridge for legacy UI mutations while the host authority seam
    /// is integrated. The command allow-list and repository graph remain the
    /// source of truth; no caller-provided capability or limit is accepted.
    pub(crate) fn run_service_mutation(
        &self,
        _arguments: Vec<OsString>,
        _remote: Option<RemotePolicy>,
        _remote_name: Option<String>,
    ) -> Result<GitOutput, GitError> {
        // The legacy service adapter has no host-issued permit parameter. It
        // remains a visible, typed-unavailable boundary until the later
        // Config/Workspace union supplies one; it must never self-authorize.
        Err(GitError::AuthorityUnavailable)
    }

    pub(crate) fn run_service_mutation_with_permit(
        &self,
        arguments: Vec<OsString>,
        remote: Option<RemotePolicy>,
        remote_name: Option<String>,
        permit: GitOperationPermit,
    ) -> Result<GitOutput, GitError> {
        if !service_mutation_allowed(&arguments) {
            return Err(GitError::InvalidRequest {
                message: "Git service mutation is not authorized for this operation".to_string(),
            });
        }
        if service_mutation_requires_remote_policy(&arguments) {
            let Some(remote) = remote.as_ref() else {
                return Err(GitError::AuthorityUnavailable);
            };
            let Some(remote_name) = remote_name.as_deref() else {
                return Err(GitError::AuthorityUnavailable);
            };
            validate_remote_name(remote_name)?;
            if !service_remote_binding_matches(&arguments, remote, remote_name) {
                return Err(GitError::RemoteNotAuthorized);
            }
        }
        self.run_invocation(
            None,
            &arguments,
            GitExecutionPolicy::ServiceMutation {
                remote,
                remote_name,
            },
            true,
            &permit,
        )
    }

    fn run_mutation_args(
        &self,
        arguments: &[OsString],
        capability: GitCapability,
        remote: Option<RemotePolicy>,
        remote_name: Option<String>,
        permit: &GitOperationPermit,
    ) -> Result<GitOutput, GitError> {
        self.run_invocation(
            None,
            arguments,
            GitExecutionPolicy::AuthorizedMutation {
                capability,
                remote,
                remote_name,
            },
            true,
            permit,
        )
    }

    #[cfg(test)]
    fn run_test_process(
        &self,
        executable: &Path,
        arguments: &[OsString],
        policy: GitExecutionPolicy,
    ) -> Result<GitOutput, GitError> {
        let executable = TrustedExecutable::test_fixture(executable).map_err(|message| {
            GitError::CommandStart {
                operation: operation_label(arguments),
                message,
            }
        })?;
        self.with_read_permit(|permit| {
            self.run_invocation(
                Some(&executable.executable),
                arguments,
                policy,
                false,
                permit,
            )
        })
    }

    fn run_invocation(
        &self,
        provided_executable: Option<&TrustedExecutable>,
        arguments: &[OsString],
        policy: GitExecutionPolicy,
        apply_global_options: bool,
        permit: &GitOperationPermit,
    ) -> Result<GitOutput, GitError> {
        let operation = operation_label(arguments);
        let graph_transition = graph_transition_for(arguments, &policy);
        validate_argument_budget(arguments)?;
        self.validate_operation_permit(permit, &policy, arguments)?;
        let deadline = permit.deadline;
        if self.cancellation.is_cancelled() {
            return Err(GitError::Cancelled { operation });
        }
        if deadline.is_expired() {
            return Err(GitError::TimedOut {
                operation,
                timeout: deadline.timeout,
            });
        }

        if let GitExecutionPolicy::AuthorizedMutation { capability, .. } = &policy {
            if !capability_matches_arguments(*capability, arguments) {
                return Err(GitError::InvalidRequest {
                    message: "mutation capability does not match the Git operation".to_string(),
                });
            }
        }

        let resolved_executable = if provided_executable.is_none() {
            match TrustedExecutable::resolve_git_with_deadline(deadline) {
                Ok(executable) => Some(executable),
                Err(_message) if deadline.is_expired() => {
                    return Err(GitError::TimedOut {
                        operation,
                        timeout: deadline.timeout,
                    })
                }
                Err(message) => {
                    return Err(GitError::CommandStart {
                        operation,
                        message: sanitize_message(&message, Some(&self.root.path)),
                    });
                }
            }
        } else {
            None
        };
        if self.cancellation.is_cancelled() {
            return Err(GitError::Cancelled { operation });
        }
        if deadline.is_expired() {
            return Err(GitError::TimedOut {
                operation,
                timeout: deadline.timeout,
            });
        }
        let executable = provided_executable
            .or(resolved_executable.as_ref())
            .expect("Git invocation must have an executable");

        if let Err(reason) = self.root.revalidate_with_deadline(deadline) {
            return Err(GitError::InvalidRepositoryRoot {
                path: "<repository>".to_string(),
                reason,
            });
        }
        if let Err(reason) = self.validate_host_authority(false, deadline) {
            return Err(GitError::InvalidRepositoryRoot {
                path: "<repository>".to_string(),
                reason,
            });
        }
        let remote_for_revalidation = match &policy {
            GitExecutionPolicy::AuthorizedMutation {
                remote: Some(remote),
                ..
            }
            | GitExecutionPolicy::ServiceMutation {
                remote: Some(remote),
                ..
            } => Some(remote),
            _ => None,
        };
        if let Some(remote) = remote_for_revalidation {
            if revalidate_remote_policy_with_deadline(&self.root.path, remote, Some(deadline))
                .is_err()
            {
                return Err(GitError::InvalidRequest {
                    message: "authorized local/file remote could not be revalidated".to_string(),
                });
            }
        }

        let binding = executable
            .bind_with_deadline(Some(deadline))
            .map_err(|message| {
                if deadline.is_expired() {
                    GitError::TimedOut {
                        operation: operation.clone(),
                        timeout: deadline.timeout,
                    }
                } else {
                    GitError::CommandStart {
                        operation: operation.clone(),
                        message: sanitize_message(&message, Some(&self.root.path)),
                    }
                }
            })?;
        let sandbox = GitSandbox::new_with_deadline(Some(deadline)).map_err(|message| {
            if deadline.is_expired() {
                GitError::TimedOut {
                    operation: operation.clone(),
                    timeout: deadline.timeout,
                }
            } else {
                GitError::CommandStart {
                    operation: operation.clone(),
                    message,
                }
            }
        })?;
        if self.cancellation.is_cancelled() {
            return Err(GitError::Cancelled { operation });
        }
        if deadline.is_expired() {
            return Err(GitError::TimedOut {
                operation,
                timeout: deadline.timeout,
            });
        }
        let executable_verification = executable.verify_with_deadline(Some(deadline));
        let graph_verification = self.root.revalidate_with_deadline(deadline);
        if executable_verification.is_err() || graph_verification.is_err() {
            return Err(GitError::CommandStart {
                operation,
                message: "trusted Git executable identity could not be verified".to_string(),
            });
        }

        let mut command = Command::new(&binding.command_path);
        self.root.configure_command(&mut command);
        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        apply_git_policy(&mut command, &policy, &sandbox, &executable.path);
        if apply_global_options {
            apply_git_options(&mut command, &sandbox, &policy);
        }
        command.args(arguments);
        #[cfg(unix)]
        command.process_group(0);
        #[cfg(windows)]
        command.creation_flags(MANAGED_PROCESS_CREATION_FLAGS);

        if self.cancellation.is_cancelled() {
            return Err(GitError::Cancelled { operation });
        }
        if deadline.is_expired() {
            return Err(GitError::TimedOut {
                operation,
                timeout: deadline.timeout,
            });
        }
        let mut process = ManagedGitChild::spawn(
            command,
            executable,
            &operation,
            graph_transition,
            deadline,
            &self.root,
            binding,
            self.authority.clone(),
            execution_endpoint_lease(&policy),
        )?;
        let cleanup_deadline = deadline.with_cleanup_reserve();
        if let Some(remote) = remote_for_revalidation {
            if let Err(reason) =
                revalidate_remote_policy_with_deadline(&self.root.path, remote, Some(deadline))
            {
                let cleanup = process.cleanup_with_deadline(cleanup_deadline);
                return Err(match cleanup {
                    Some(cleanup) => GitError::CleanupFailed {
                        operation: operation.clone(),
                        reason: format!(
                            "authorized local/file remote changed: {reason}; {cleanup}"
                        ),
                    },
                    None => GitError::InvalidRequest {
                        message: format!("authorized local/file remote changed: {reason}"),
                    },
                });
            }
        }
        let stdout = match process.child.stdout.take() {
            Some(stdout) => stdout,
            None => {
                let cleanup = process.cleanup_with_deadline(cleanup_deadline);
                return Err(command_start_with_cleanup(
                    &operation,
                    "Git stdout pipe was not available",
                    cleanup,
                    &self.root.path,
                ));
            }
        };
        let stderr = match process.child.stderr.take() {
            Some(stderr) => stderr,
            None => {
                let cleanup = process.cleanup_with_deadline(cleanup_deadline);
                return Err(command_start_with_cleanup(
                    &operation,
                    "Git stderr pipe was not available",
                    cleanup,
                    &self.root.path,
                ));
            }
        };

        #[cfg(unix)]
        if let Err(error) =
            make_pipe_nonblocking(&stdout).and_then(|_| make_pipe_nonblocking(&stderr))
        {
            let cleanup = process.cleanup_with_deadline(cleanup_deadline);
            return Err(command_start_with_cleanup(
                &operation,
                &format!("could not configure Git output pipes: {error}"),
                cleanup,
                &self.root.path,
            ));
        }

        let stdout_limit = self.limits.max_stdout_bytes;
        let stderr_limit = self.limits.max_stderr_bytes;
        let mut stdout_worker =
            match ReaderWorker::spawn_with_deadline("stdout", stdout, stdout_limit, deadline) {
                Ok(worker) => Some(worker),
                Err(error) => {
                    let cleanup = process.cleanup_with_deadline(cleanup_deadline);
                    return Err(command_start_with_cleanup(
                        &operation,
                        &format!("could not start stdout reader: {error}"),
                        cleanup,
                        &self.root.path,
                    ));
                }
            };
        let mut stderr_worker =
            match ReaderWorker::spawn_with_deadline("stderr", stderr, stderr_limit, deadline) {
                Ok(worker) => Some(worker),
                Err(error) => {
                    if let Some(worker) = stdout_worker.as_ref() {
                        worker.cancel();
                    }
                    let cleanup = process.cleanup_with_deadline(cleanup_deadline);
                    let reader_cleanup = stdout_worker
                        .take()
                        .and_then(|mut worker| worker.join(cleanup_deadline).err());
                    let reason = [
                        Some(format!("could not start stderr reader: {error}")),
                        cleanup,
                        reader_cleanup,
                    ]
                    .into_iter()
                    .flatten()
                    .collect::<Vec<_>>()
                    .join("; ");
                    return Err(GitError::CleanupFailed { operation, reason });
                }
            };

        let mut exit_status = None;
        let mut failure: Option<GitError> = None;
        let mut stdout_result = None;
        let mut stderr_result = None;
        let mut stdout_overflow = false;
        let mut stderr_overflow = false;
        loop {
            if deadline.is_expired() {
                failure = Some(GitError::TimedOut {
                    operation: operation.clone(),
                    timeout: deadline.timeout,
                });
                break;
            }
            if let Err(reason) = self
                .root
                .revalidate_during_transition_with_deadline(graph_transition, deadline)
            {
                failure = Some(GitError::InvalidRepositoryRoot {
                    path: "<repository>".to_string(),
                    reason,
                });
                break;
            }
            if let Err(reason) = self.validate_host_authority(false, deadline) {
                failure = Some(GitError::InvalidRepositoryRoot {
                    path: "<repository>".to_string(),
                    reason,
                });
                break;
            }
            if let Some(remote) = remote_for_revalidation {
                if matches!(
                    remote.transport(),
                    RemoteTransport::Local | RemoteTransport::File
                ) && revalidate_remote_policy_with_deadline(
                    &self.root.path,
                    remote,
                    Some(deadline),
                )
                .is_err()
                {
                    failure = Some(GitError::InvalidRequest {
                        message: "authorized local/file remote changed".to_string(),
                    });
                    break;
                }
            }
            if self.cancellation.is_cancelled() {
                failure = Some(GitError::Cancelled {
                    operation: operation.clone(),
                });
                break;
            }
            if stdout_result.is_none() {
                if stdout_worker.as_ref().is_some_and(|worker| {
                    worker
                        .handle
                        .as_ref()
                        .is_some_and(|handle| handle.is_finished())
                }) {
                    let mut worker = stdout_worker
                        .take()
                        .expect("finished stdout worker must still be owned");
                    match worker.join(deadline) {
                        Ok(result) => {
                            if result.truncated {
                                stdout_overflow = true;
                            } else {
                                stdout_result = Some(result);
                            }
                        }
                        Err(reason) => {
                            note_cleanup_failure(&mut failure, &operation, reason);
                        }
                    }
                }
            }
            if stderr_result.is_none() {
                if stderr_worker.as_ref().is_some_and(|worker| {
                    worker
                        .handle
                        .as_ref()
                        .is_some_and(|handle| handle.is_finished())
                }) {
                    let mut worker = stderr_worker
                        .take()
                        .expect("finished stderr worker must still be owned");
                    match worker.join(deadline) {
                        Ok(result) => {
                            if result.truncated {
                                stderr_overflow = true;
                            } else {
                                stderr_result = Some(result);
                            }
                        }
                        Err(reason) => {
                            note_cleanup_failure(&mut failure, &operation, reason);
                        }
                    }
                }
            }
            if failure.is_none() && stdout_overflow {
                failure = Some(GitError::OutputLimitExceeded {
                    stream: "stdout",
                    limit: stdout_limit,
                });
                break;
            }
            if failure.is_none() && stderr_overflow && stdout_worker.is_none() {
                failure = Some(GitError::OutputLimitExceeded {
                    stream: "stderr",
                    limit: stderr_limit,
                });
                break;
            }
            if failure.is_some() {
                break;
            }
            match process.try_wait() {
                Ok(Some(status)) => {
                    exit_status = Some(status);
                }
                Ok(None) => {}
                Err(error) => {
                    failure = Some(GitError::CommandStart {
                        operation: operation.clone(),
                        message: sanitize_message(&error.to_string(), Some(&self.root.path)),
                    });
                    break;
                }
            }
            if exit_status.is_some() && stdout_result.is_some() && stderr_result.is_some() {
                if deadline.is_expired() {
                    failure = Some(GitError::TimedOut {
                        operation: operation.clone(),
                        timeout: deadline.timeout,
                    });
                }
                break;
            }
            if deadline.is_expired() {
                failure = Some(GitError::TimedOut {
                    operation: operation.clone(),
                    timeout: deadline.timeout,
                });
                break;
            }
            deadline.sleep();
        }

        // Close/terminate the owned child first.  Closing the child pipe
        // endpoints is the authoritative cancellation for a blocking reader;
        // the cooperative flag and platform interrupt then let every worker
        // observe cancellation before it is joined.
        if failure.is_some() {
            if let Some(worker) = stdout_worker.as_ref() {
                worker.cancel();
            }
            if let Some(worker) = stderr_worker.as_ref() {
                worker.cancel();
            }
        }
        let mut cleanup = if failure.is_some() {
            process.cleanup_with_deadline(cleanup_deadline)
        } else {
            None
        };

        // The reader workers joined here are settled while the process/job
        // settlement contract is
        // still owned here; no pipe-owning worker may outlive this runner.
        let stdout_join = stdout_worker
            .take()
            .map(|mut worker| worker.join(cleanup_deadline));
        let stderr_join = stderr_worker
            .take()
            .map(|mut worker| worker.join(cleanup_deadline));
        if let Some(result) = stdout_join {
            match result {
                Ok(result) if !result.truncated => stdout_result = Some(result),
                Ok(_) => {
                    stdout_overflow = true;
                }
                Err(reason) => {
                    note_cleanup_failure(&mut failure, &operation, reason);
                }
            }
        }
        if let Some(result) = stderr_join {
            match result {
                Ok(result) if !result.truncated => stderr_result = Some(result),
                Ok(_) => {
                    stderr_overflow = true;
                }
                Err(reason) => {
                    note_cleanup_failure(&mut failure, &operation, reason);
                }
            }
        }

        if failure.is_none() {
            if stdout_overflow {
                failure = Some(GitError::OutputLimitExceeded {
                    stream: "stdout",
                    limit: stdout_limit,
                });
            } else if stderr_overflow {
                failure = Some(GitError::OutputLimitExceeded {
                    stream: "stderr",
                    limit: stderr_limit,
                });
            }
        }

        if failure.is_some() && cleanup.is_none() {
            cleanup = process.cleanup_with_deadline(cleanup_deadline);
        }
        if cleanup.is_none() {
            match process.wait_for_settlement(cleanup_deadline) {
                Ok(()) => process.settled = true,
                Err(reason) => {
                    note_cleanup_failure(&mut failure, &operation, reason);
                    cleanup = process.cleanup_with_deadline(cleanup_deadline);
                }
            }
        }

        if let Some(cleanup) = cleanup {
            let primary = failure.map(|error| error.to_string()).unwrap_or_else(|| {
                "Git process cleanup was required after an unexpected runner state".to_string()
            });
            return Err(GitError::CleanupFailed {
                operation,
                reason: format!("{primary}; {cleanup}"),
            });
        }

        if deadline.is_expired() {
            process.release_job();
            return Err(GitError::TimedOut {
                operation,
                timeout: deadline.timeout,
            });
        }
        let graph_validation = match graph_transition {
            GraphTransition::ReadOnly => self.root.revalidate_with_deadline(deadline),
            transition => self
                .root
                .revalidate_after_transition_with_deadline(transition, deadline),
        };
        let child_authority_validation = (!process.authority_is_live())
            .then(|| "child Git authority capability expired".to_string());
        let endpoint_validation = remote_for_revalidation
            .filter(|remote| {
                matches!(
                    remote.transport(),
                    RemoteTransport::Local | RemoteTransport::File
                )
            })
            .and_then(|remote| {
                revalidate_remote_policy_with_deadline(&self.root.path, remote, Some(deadline))
                    .err()
            });
        // Advance the host's mutable graph identity only after every
        // post-effect proof succeeds. A failed graph, endpoint, child, or
        // operation validation must never turn an observed substitution into
        // the next authority baseline.
        let authority_validation = if graph_validation.is_ok()
            && child_authority_validation.is_none()
            && endpoint_validation.is_none()
            && failure.is_none()
        {
            self.validate_host_authority(true, deadline).err()
        } else {
            self.validate_host_authority(false, deadline).err()
        };
        process.release_job();

        if let Err(reason) = graph_validation {
            return Err(GitError::InvalidRepositoryRoot {
                path: "<repository>".to_string(),
                reason,
            });
        }
        if let Some(reason) = authority_validation {
            return Err(GitError::InvalidRepositoryRoot {
                path: "<repository>".to_string(),
                reason,
            });
        }
        if let Some(reason) = child_authority_validation {
            return Err(GitError::InvalidRepositoryRoot {
                path: "<repository>".to_string(),
                reason,
            });
        }
        if let Some(reason) = endpoint_validation {
            return Err(GitError::InvalidRequest {
                message: format!("authorized local/file remote changed: {reason}"),
            });
        }

        if let Some(failure) = failure {
            return Err(failure);
        }

        let status = exit_status.ok_or_else(|| GitError::CommandStart {
            operation: operation.clone(),
            message: "Git did not report an exit status".to_string(),
        })?;
        if !status.success() {
            let stderr_bytes = stderr_result
                .as_ref()
                .map(|result| result.bytes.as_slice())
                .unwrap_or_default();
            return Err(GitError::CommandFailed {
                operation,
                code: status.code(),
                stderr: sanitize_message(
                    &String::from_utf8_lossy(stderr_bytes),
                    Some(&self.root.path),
                ),
            });
        }
        Ok(GitOutput {
            stdout: stdout_result
                .expect("successful Git execution must settle stdout")
                .bytes,
            stderr: stderr_result
                .expect("successful Git execution must settle stderr")
                .bytes,
            status,
        })
    }

    fn authority_binding(&self) -> Option<&GitHostBinding> {
        match &self.authority {
            GitRepositoryAuthority::Host(binding) => Some(binding),
            #[cfg(test)]
            GitRepositoryAuthority::Test => None,
        }
    }

    fn validate_host_authority(
        &self,
        refresh_graph_identity: bool,
        deadline: OperationDeadline,
    ) -> Result<(), String> {
        if deadline.is_expired() {
            return Err(
                "host Git authority validation exceeded the operation deadline".to_string(),
            );
        }
        let Some(binding) = self.authority_binding() else {
            return Ok(());
        };
        if !binding.capability.is_live() {
            return Err("host Git authority capability is unavailable".to_string());
        }
        let current_repository_identity = repository_graph_identity(&self.root);
        if deadline.is_expired() {
            return Err(
                "host Git authority validation exceeded the operation deadline".to_string(),
            );
        }
        let mut bound_repository_identity = binding
            .capability
            .repository_identity
            .lock()
            .map_err(|_| "host Git repository identity is unavailable".to_string())?;
        if *bound_repository_identity != current_repository_identity {
            if !refresh_graph_identity {
                return Err("host Git repository graph binding is stale".to_string());
            }
            // The graph transition has already been revalidated by the caller;
            // advance the opaque binding's mutable identity only after that
            // proof, so a subsequent action cannot replay the old graph.
            *bound_repository_identity = current_repository_identity;
        }
        if repository_static_graph_identity(&self.root)
            != binding.capability.repository_static_identity
        {
            return Err("host Git repository static binding is stale".to_string());
        }
        if deadline.is_expired() {
            return Err(
                "host Git authority validation exceeded the operation deadline".to_string(),
            );
        }
        Ok(())
    }
}

#[cfg(windows)]
fn terminate_windows_job(job: &ManagedProcessJob) -> Result<(), String> {
    let active_processes = job
        .active_process_ids()
        .map_err(|error| format!("managed Job active-process query failed: {error}"))?;
    if active_processes.is_empty() {
        return Ok(());
    }
    let result = unsafe { TerminateJobObject(job.borrowed_handle().as_raw_handle(), 1) };
    if result == 0 {
        Err(format!(
            "TerminateJobObject failed: {}",
            io::Error::last_os_error()
        ))
    } else {
        Ok(())
    }
}

#[cfg(windows)]
fn wait_for_active_process_zero(
    job: &ManagedProcessJob,
    deadline: OperationDeadline,
) -> Result<(), String> {
    loop {
        let process_ids = job
            .active_process_ids()
            .map_err(|error| format!("managed Job active-process query failed: {error}"))?;
        if process_ids.is_empty() {
            return Ok(());
        }
        if deadline.is_expired() {
            return Err(format!(
                "managed Job did not reach ACTIVE_PROCESS_ZERO before the operation deadline ({process_ids:?})"
            ));
        }
        deadline.sleep();
    }
}

#[cfg(unix)]
fn unix_process_group_target(pid: u32) -> Result<i32, String> {
    let pid = i32::try_from(pid).map_err(|_| format!("process ID {pid} exceeds Unix limits"))?;
    Ok(-pid)
}

#[cfg(unix)]
fn send_unix_process_group_signal(pid: u32, signal: i32) -> Result<(), String> {
    let target = unix_process_group_target(pid)?;
    let result = unsafe { kill(target, signal) };
    if result == 0 {
        return Ok(());
    }
    let error = io::Error::last_os_error();
    if error.raw_os_error() == Some(ESRCH) {
        Ok(())
    } else {
        Err(format!("kill({target}, {signal}) failed: {error}"))
    }
}

#[cfg(unix)]
fn unix_process_group_exists(pid: u32) -> Result<bool, String> {
    let target = unix_process_group_target(pid)?;
    if unsafe { kill(target, 0) } == 0 {
        return Ok(true);
    }
    let error = io::Error::last_os_error();
    if error.raw_os_error() == Some(ESRCH) {
        Ok(false)
    } else {
        Err(format!("kill({target}, 0) failed: {error}"))
    }
}

#[cfg(unix)]
fn terminate_unix_process_group(pid: u32, deadline: OperationDeadline) -> Option<String> {
    let mut errors = Vec::new();
    let term_error = send_unix_process_group_signal(pid, SIGTERM).err();
    let mut group_remaining = true;
    let term_grace_deadline = Instant::now()
        .checked_add(deadline.remaining().min(Duration::from_millis(100)))
        .unwrap_or_else(Instant::now);

    loop {
        match unix_process_group_exists(pid) {
            Ok(false) => {
                group_remaining = false;
                break;
            }
            Ok(true) if deadline.is_expired() || Instant::now() >= term_grace_deadline => break,
            Ok(true) => deadline.sleep(),
            Err(error) => {
                errors.push(error);
                break;
            }
        }
    }

    let kill_error = if group_remaining {
        send_unix_process_group_signal(pid, SIGKILL).err()
    } else {
        None
    };
    if group_remaining {
        while group_remaining && !deadline.is_expired() {
            match unix_process_group_exists(pid) {
                Ok(false) => group_remaining = false,
                Ok(true) => deadline.sleep(),
                Err(error) => {
                    errors.push(error);
                    break;
                }
            }
        }
    }

    if group_remaining {
        if let Some(error) = term_error {
            errors.push(error);
        }
        if let Some(error) = kill_error {
            errors.push(error);
        }
        errors.push(
            "owned Unix process group did not reach zero before the operation deadline".to_string(),
        );
    }
    (!errors.is_empty()).then(|| errors.join("; "))
}

#[cfg(unix)]
fn wait_for_unix_process_group_zero(pid: u32, deadline: OperationDeadline) -> Result<(), String> {
    loop {
        if !unix_process_group_exists(pid)? {
            return Ok(());
        }
        if deadline.is_expired() {
            return Err(
                "owned Unix process group did not reach zero before the operation deadline"
                    .to_string(),
            );
        }
        deadline.sleep();
    }
}

fn wait_for_child(child: &mut Child, deadline: OperationDeadline) -> Result<ExitStatus, String> {
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Ok(status),
            Ok(None) => {}
            Err(error) => return Err(format!("wait failed: {error}")),
        }
        if deadline.is_expired() {
            return Err("child did not exit before the operation deadline".to_string());
        }
        deadline.sleep();
    }
}

fn cleanup_unmanaged_child(child: &mut Child, deadline: OperationDeadline) -> Option<String> {
    let deadline = deadline.with_cleanup_reserve();
    let mut errors = Vec::new();
    #[cfg(unix)]
    if let Some(error) = terminate_unix_process_group(child.id(), deadline) {
        errors.push(error);
    }

    #[cfg(windows)]
    match child.try_wait() {
        Ok(Some(_)) => {}
        Ok(None) => {
            // Setup can fail before the suspended child is assigned to the
            // managed Job (for example, image or graph validation).  The
            // Child owns the exact OS process handle, so this fallback is
            // bounded and cannot target a reused PID.  Once a Job exists,
            // ManagedGitChild::cleanup always uses the Job instead.
            if let Err(error) = child.kill() {
                if !matches!(child.try_wait(), Ok(Some(_))) {
                    errors.push(format!("unmanaged child termination failed: {error}"));
                }
            }
        }
        Err(error) => errors.push(format!("status check failed: {error}")),
    }
    if let Err(error) = wait_for_child(child, deadline) {
        errors.push(error);
    }
    (!errors.is_empty()).then(|| errors.join("; "))
}

fn command_start_with_cleanup(
    operation: &str,
    message: &str,
    cleanup: Option<String>,
    root: &Path,
) -> GitError {
    match cleanup {
        Some(cleanup) => GitError::CleanupFailed {
            operation: operation.to_string(),
            reason: format!("{}; {cleanup}", sanitize_message(message, Some(root))),
        },
        None => GitError::CommandStart {
            operation: operation.to_string(),
            message: sanitize_message(message, Some(root)),
        },
    }
}

fn note_cleanup_failure(slot: &mut Option<GitError>, operation: &str, reason: String) {
    let primary = slot
        .take()
        .map(|error| format!("{error}; "))
        .unwrap_or_default();
    *slot = Some(GitError::CleanupFailed {
        operation: operation.to_string(),
        reason: format!("{primary}{reason}"),
    });
}

impl StatusPlan {
    #[cfg(test)]
    pub(crate) fn invocation(&self) -> GitInvocation {
        GitInvocation {
            executable: OsString::from("git"),
            cwd: self.workspace().cwd().to_path_buf(),
            arguments: self.raw_arguments().to_vec(),
        }
    }
}

impl DiffPlan {
    #[cfg(test)]
    pub(crate) fn invocation(&self) -> GitInvocation {
        GitInvocation {
            executable: OsString::from("git"),
            cwd: self.workspace().cwd().to_path_buf(),
            arguments: self.raw_arguments().to_vec(),
        }
    }
}

impl ReviewPlan {
    #[cfg(test)]
    pub(crate) fn invocation(&self) -> GitInvocation {
        GitInvocation {
            executable: OsString::from("git"),
            cwd: self.workspace().cwd().to_path_buf(),
            arguments: self.raw_arguments().to_vec(),
        }
    }
}

impl StagePlan {
    #[cfg(test)]
    pub(crate) fn invocation(&self) -> GitInvocation {
        GitInvocation {
            executable: OsString::from("git"),
            cwd: self.workspace().cwd().to_path_buf(),
            arguments: self.raw_arguments().to_vec(),
        }
    }
}

impl UnstagePlan {
    #[cfg(test)]
    pub(crate) fn invocation(&self) -> GitInvocation {
        GitInvocation {
            executable: OsString::from("git"),
            cwd: self.workspace().cwd().to_path_buf(),
            arguments: self.raw_arguments().to_vec(),
        }
    }
}

impl CommitPlan {
    #[cfg(test)]
    pub(crate) fn invocation(&self) -> GitInvocation {
        GitInvocation {
            executable: OsString::from("git"),
            cwd: self.workspace().cwd().to_path_buf(),
            arguments: self.raw_arguments().to_vec(),
        }
    }
}

impl PushPlan {
    #[cfg(test)]
    pub(crate) fn invocation(&self) -> GitInvocation {
        GitInvocation {
            executable: OsString::from("git"),
            cwd: self.workspace().cwd().to_path_buf(),
            arguments: self.raw_arguments().to_vec(),
        }
    }
}

fn arg(value: &str) -> OsString {
    OsString::from(value)
}

fn diff_arguments(staged: bool) -> Vec<OsString> {
    let mut arguments = vec![
        arg("diff"),
        arg("--binary"),
        arg("--full-index"),
        arg("--no-color"),
        arg("--no-ext-diff"),
        arg("--no-textconv"),
    ];
    if staged {
        arguments.push(arg("--cached"));
    }
    arguments.push(arg("--"));
    arguments
}

fn validate_files(
    root: &Path,
    status: &RepositoryStatus,
    files: &[RepoPath],
) -> Result<Vec<RepoPath>, GitError> {
    if files.is_empty() {
        return Err(GitError::InvalidRequest {
            message: "at least one exact repository file is required".to_string(),
        });
    }
    if files.len() > HARD_MAX_STAGE_FILES {
        return Err(GitError::InvalidRequest {
            message: format!("mutation file list exceeds the {HARD_MAX_STAGE_FILES}-file limit"),
        });
    }
    let argument_bytes = files
        .iter()
        .map(|file| file.as_bytes().len())
        .try_fold(0usize, |total, length| total.checked_add(length))
        .ok_or_else(|| GitError::InvalidRequest {
            message: "mutation file list byte size overflowed".to_string(),
        })?;
    if argument_bytes > HARD_MAX_STAGE_ARGUMENT_BYTES {
        return Err(GitError::InvalidRequest {
            message: format!(
                "mutation file list exceeds the {HARD_MAX_STAGE_ARGUMENT_BYTES}-byte limit"
            ),
        });
    }
    let mut result = Vec::with_capacity(files.len());
    for file in files {
        file.validate_relative()
            .map_err(|reason| GitError::InvalidPath {
                path: file.display_lossy().into_owned(),
                reason,
            })?;
        let candidate = root.join(file.to_path_buf());
        validate_repository_path(root, file)?;
        let is_submodule = status
            .entries
            .iter()
            .any(|entry| entry.path == *file && entry.kind == StatusKind::Submodule);
        if candidate.is_dir() && !is_submodule {
            return Err(GitError::InvalidPath {
                path: file.display_lossy().into_owned(),
                reason: "directories are not exact file targets".to_string(),
            });
        }
        if result.iter().any(|existing| existing == file) {
            return Err(GitError::InvalidRequest {
                message: "mutation file list contains a duplicate path".to_string(),
            });
        }
        result.push(file.clone());
    }
    Ok(result)
}

fn validate_repository_path(root: &Path, path: &RepoPath) -> Result<(), GitError> {
    path.validate_relative()
        .map_err(|reason| GitError::InvalidPath {
            path: path.display_lossy().into_owned(),
            reason,
        })?;

    let candidate = root.join(path.to_path_buf());
    canonical_target(root, &candidate).map_err(|reason| GitError::InvalidPath {
        path: path.display_lossy().into_owned(),
        reason,
    })?;
    Ok(())
}

fn canonical_target(root: &Path, candidate: &Path) -> Result<PathBuf, String> {
    reject_reparse_components(candidate)?;
    let metadata = std::fs::symlink_metadata(candidate).ok();
    let mut probe = candidate.to_path_buf();
    let mut missing = Vec::new();

    if metadata
        .as_ref()
        .is_some_and(|metadata| metadata.file_type().is_symlink())
    {
        let resolved = std::fs::canonicalize(candidate)
            .map_err(|_| "repository path cannot be resolved safely".to_string())?;
        if !is_within(root, &resolved) {
            return Err("resolved repository path is outside the workspace".to_string());
        }
        return Ok(resolved);
    }

    while !probe.exists() {
        let name = probe
            .file_name()
            .ok_or_else(|| "repository path cannot be resolved safely".to_string())?
            .to_os_string();
        missing.push(name);
        if !probe.pop() {
            return Err("repository path cannot be resolved safely".to_string());
        }
    }

    let mut resolved = std::fs::canonicalize(&probe)
        .map_err(|_| "repository path cannot be resolved safely".to_string())?;
    for name in missing.iter().rev() {
        resolved.push(name);
    }
    if !is_within(root, &resolved) {
        return Err("resolved repository path is outside the workspace".to_string());
    }
    Ok(resolved)
}

fn is_within(root: &Path, candidate: &Path) -> bool {
    #[cfg(windows)]
    {
        let root = windows_path_units(root);
        let candidate = windows_path_units(candidate);
        candidate == root
            || candidate.starts_with(&root) && candidate.get(root.len()) == Some(&u16::from(b'\\'))
    }

    #[cfg(not(windows))]
    {
        candidate == root || candidate.strip_prefix(root).is_ok()
    }
}

fn canonicalize_approved_graph_roots(
    workspace_root: &Path,
    approved: &[PathBuf],
) -> Result<Vec<PathBuf>, String> {
    canonicalize_approved_graph_roots_with_deadline(
        workspace_root,
        approved,
        OperationDeadline::from_now(HARD_MAX_TIMEOUT),
    )
}

fn canonicalize_approved_graph_roots_with_deadline(
    workspace_root: &Path,
    approved: &[PathBuf],
    deadline: OperationDeadline,
) -> Result<Vec<PathBuf>, String> {
    if approved.len() > HARD_MAX_APPROVED_GRAPH_ROOTS {
        return Err(format!(
            "approved Git graph roots exceed the {HARD_MAX_APPROVED_GRAPH_ROOTS}-entry limit"
        ));
    }
    let mut canonical_roots: Vec<PathBuf> = Vec::with_capacity(approved.len());
    for requested in approved {
        check_graph_deadline(deadline)?;
        reject_reparse_components(requested)?;
        check_graph_deadline(deadline)?;
        let canonical = fs::canonicalize(requested)
            .map_err(|_| "approved external Git graph root cannot be canonicalized".to_string())?;
        if is_within(workspace_root, &canonical) {
            continue;
        }
        check_graph_deadline(deadline)?;
        let metadata = fs::symlink_metadata(&canonical)
            .map_err(|_| "approved external Git graph root is unavailable".to_string())?;
        if !metadata.is_dir() || !same_path(&canonical, requested) {
            return Err("approved external Git graph root must be a stable directory".to_string());
        }
        // Resolve and retain the directory identity during graph admission;
        // descendants are separately represented by graph nodes and handles.
        check_graph_deadline(deadline)?;
        let _ = directory_identity_with_deadline(&canonical, Some(deadline))?;
        if !canonical_roots
            .iter()
            .any(|root| same_path(root, &canonical))
        {
            canonical_roots.push(canonical);
        }
    }
    check_graph_deadline(deadline)?;
    Ok(canonical_roots)
}

fn graph_path_allowed(root: &Path, candidate: &Path, approved: &[PathBuf]) -> bool {
    is_within(root, candidate) || approved.iter().any(|allowed| is_within(allowed, candidate))
}

#[derive(Default)]
struct BoundedRead {
    bytes: Vec<u8>,
    truncated: bool,
}

impl fmt::Debug for BoundedRead {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("BoundedRead")
            .field("bytes", &self.bytes.len())
            .field("truncated", &self.truncated)
            .finish()
    }
}

fn read_bounded<R: Read>(
    mut reader: R,
    limit: usize,
    cancelled: &AtomicBool,
) -> io::Result<BoundedRead> {
    let mut bytes = Vec::with_capacity(limit.min(64 * 1024));
    let cap = limit.saturating_add(1);
    let mut buffer = [0u8; 8192];
    while bytes.len() < cap {
        if cancelled.load(Ordering::Acquire) {
            break;
        }
        let remaining = cap - bytes.len();
        let size = remaining.min(buffer.len());
        match reader.read(&mut buffer[..size]) {
            Ok(0) => break,
            Ok(count) => bytes.extend_from_slice(&buffer[..count]),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                if cancelled.load(Ordering::Acquire) {
                    break;
                }
                thread::sleep(Duration::from_millis(1));
            }
            Err(error) => return Err(error),
        }
    }
    let truncated = bytes.len() > limit;
    if truncated {
        bytes.truncate(limit);
    }
    Ok(BoundedRead { bytes, truncated })
}

#[cfg(unix)]
fn make_pipe_nonblocking<T: AsRawFd>(reader: &T) -> io::Result<()> {
    let file_descriptor = reader.as_raw_fd();
    let flags = unsafe { fcntl(file_descriptor, F_GETFL) };
    if flags < 0 {
        return Err(io::Error::last_os_error());
    }
    if unsafe { fcntl(file_descriptor, F_SETFL, flags | O_NONBLOCK) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn sanitize_message(message: &str, root: Option<&Path>) -> String {
    let mut value = message.replace('\0', "").replace('\r', "");
    if let Some(root) = root {
        let root_display = root.to_string_lossy();
        value = value.replace(root_display.as_ref(), "<repo>");
        value = value.replace(&root_display.replace('\\', "/"), "<repo>");
    }

    let mut sanitized = String::new();
    for (index, token) in value.split_whitespace().enumerate() {
        if index > 0 {
            sanitized.push(' ');
        }
        let trimmed = token.trim_matches(|character| matches!(character, '\'' | '"' | ',' | ';'));
        if looks_like_path(trimmed) {
            sanitized.push_str("<path>");
        } else {
            sanitized.push_str(&redact_credentials(token));
        }
    }
    sanitized.truncate(2048);
    sanitized
}

fn sanitize_command_output(output: &str) -> String {
    output
        .split_whitespace()
        .filter_map(|token| {
            let trimmed =
                token.trim_matches(|character| matches!(character, '\'' | '"' | ',' | ';'));
            if looks_like_path(trimmed) {
                Some("<path>".to_string())
            } else if trimmed.contains("://") {
                Some(redact_credentials(trimmed))
            } else {
                None
            }
        })
        .take(8)
        .collect::<Vec<_>>()
        .join(" ")
}

fn redact_credentials(value: &str) -> String {
    let Some(scheme_end) = value.find("://") else {
        return value.to_string();
    };
    let authority_start = scheme_end + 3;
    let authority_end = value[authority_start..]
        .find(|character: char| character.is_whitespace())
        .map_or(value.len(), |index| authority_start + index);
    let authority = &value[authority_start..authority_end];
    if authority.rfind('@').is_some() {
        format!("{}://<secret>@<host>/<path>", &value[..scheme_end])
    } else {
        "<url>".to_string()
    }
}

fn validate_remote(value: &str, label: &str) -> Result<(), GitError> {
    if value.is_empty() {
        return Err(GitError::InvalidRequest {
            message: format!("{label} must be non-empty"),
        });
    }
    if value.contains('\0') || value.chars().any(|character| character.is_control()) {
        return Err(GitError::InvalidRequest {
            message: format!("{label} contains a control character"),
        });
    }
    if value.chars().any(char::is_whitespace) {
        return Err(GitError::InvalidRequest {
            message: format!("{label} must not contain whitespace"),
        });
    }
    if contains_embedded_credentials(value) {
        return Err(GitError::InvalidRequest {
            message: format!("{label} must not contain embedded credentials"),
        });
    }
    Ok(())
}

fn validate_remote_name(value: &str) -> Result<(), GitError> {
    if value.is_empty()
        || value.starts_with('-')
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
    {
        return Err(GitError::InvalidRequest {
            message: "Git remote name is not a typed configuration subsection".to_string(),
        });
    }
    Ok(())
}

fn remote_policy_from_url(root: &Path, value: &str) -> Result<RemotePolicy, GitError> {
    remote_policy_from_url_with_deadline(root, value, None)
}

fn remote_policy_from_url_with_deadline(
    root: &Path,
    value: &str,
    deadline: Option<OperationDeadline>,
) -> Result<RemotePolicy, GitError> {
    if deadline.is_some_and(OperationDeadline::is_expired) {
        return Err(GitError::TimedOut {
            operation: "remote endpoint admission".to_string(),
            timeout: deadline.map_or(Duration::ZERO, |deadline| deadline.timeout),
        });
    }
    if value.is_empty() {
        return Err(GitError::InvalidRequest {
            message: "configured push remote is empty".to_string(),
        });
    }
    if value.starts_with("https://") {
        return RemotePolicy::https(value).map_err(|message| GitError::InvalidRequest { message });
    }
    if value.starts_with("ssh://") {
        return RemotePolicy::ssh(value).map_err(|message| GitError::InvalidRequest { message });
    }
    if !looks_like_path(value) && value.contains('@') && value.contains(':') {
        if let Ok(policy) = RemotePolicy::ssh(value) {
            return Ok(policy);
        }
    }

    let (transport, path) = if looks_like_path(value) {
        (RemoteTransport::Local, PathBuf::from(value))
    } else if let Ok(url) = url::Url::parse(value) {
        if url.scheme() != "file" {
            return Err(GitError::InvalidRequest {
                message: "push remote transport is not authorized".to_string(),
            });
        }
        let path = url.to_file_path().map_err(|_| GitError::InvalidRequest {
            message: "file push remote path is invalid".to_string(),
        })?;
        (RemoteTransport::File, path)
    } else {
        (RemoteTransport::Local, PathBuf::from(value))
    };
    let candidate = if path.is_absolute() {
        path
    } else {
        root.join(path)
    };
    if deadline.is_some_and(OperationDeadline::is_expired) {
        return Err(GitError::TimedOut {
            operation: "remote endpoint admission".to_string(),
            timeout: deadline.map_or(Duration::ZERO, |deadline| deadline.timeout),
        });
    }
    reject_reparse_components(&candidate)
        .map_err(|message| GitError::InvalidRequest { message })?;
    let canonical = fs::canonicalize(&candidate).map_err(|_| GitError::InvalidRequest {
        message: "local push remote cannot be resolved safely".to_string(),
    })?;
    if deadline.is_some_and(OperationDeadline::is_expired) {
        return Err(GitError::TimedOut {
            operation: "remote endpoint admission".to_string(),
            timeout: deadline.map_or(Duration::ZERO, |deadline| deadline.timeout),
        });
    }
    if !is_within(root, &canonical) || same_path(root, &canonical) {
        return Err(GitError::InvalidRequest {
            message: "local push remote must be contained by the repository workspace".to_string(),
        });
    }
    let identity = directory_identity_with_deadline(&canonical, deadline)
        .map_err(|message| GitError::InvalidRequest { message })?;
    if deadline.is_some_and(OperationDeadline::is_expired) {
        return Err(GitError::TimedOut {
            operation: "remote endpoint admission".to_string(),
            timeout: deadline.map_or(Duration::ZERO, |deadline| deadline.timeout),
        });
    }
    let (endpoint_handles, endpoint_ancestors) =
        retain_endpoint_handles_with_deadline(root, &canonical, deadline)
            .map_err(|message| GitError::InvalidRequest { message })?;
    if deadline.is_some_and(OperationDeadline::is_expired) {
        return Err(GitError::TimedOut {
            operation: "remote endpoint admission".to_string(),
            timeout: deadline.map_or(Duration::ZERO, |deadline| deadline.timeout),
        });
    }
    let endpoint = match transport {
        RemoteTransport::Local => canonical.to_string_lossy().into_owned(),
        RemoteTransport::File => url::Url::from_file_path(&canonical)
            .map_err(|_| GitError::InvalidRequest {
                message: "file push remote path cannot be encoded safely".to_string(),
            })?
            .to_string(),
        RemoteTransport::Https | RemoteTransport::Ssh => {
            return Err(GitError::InvalidRequest {
                message: "local push remote transport is invalid".to_string(),
            })
        }
    };
    RemotePolicy::local_with_lease(
        transport,
        endpoint,
        identity_token(&identity),
        Some(Arc::new(RemoteEndpointLease::new(
            canonical,
            identity_token(&identity),
            endpoint_handles,
            endpoint_ancestors,
        ))),
    )
    .map_err(|message| GitError::InvalidRequest { message })
}

fn retain_endpoint_handles_with_deadline(
    root: &Path,
    endpoint: &Path,
    deadline: Option<OperationDeadline>,
) -> Result<(Arc<Vec<fs::File>>, Arc<Vec<(PathBuf, String)>>), String> {
    if deadline.is_some_and(OperationDeadline::is_expired) {
        return Err("remote endpoint admission exceeded the operation deadline".to_string());
    }
    if !is_within(root, endpoint) || same_path(root, endpoint) {
        return Err("local push remote must be contained by the repository workspace".to_string());
    }
    let mut paths = Vec::new();
    let mut current = Some(endpoint);
    let mut depth = 0;
    while let Some(path) = current {
        if depth >= HARD_MAX_GRAPH_DEPTH {
            return Err("remote endpoint ancestor graph is too deep".to_string());
        }
        depth += 1;
        paths.push(path.to_path_buf());
        if same_path(path, root) {
            break;
        }
        current = path.parent();
    }
    if !paths.iter().any(|path| same_path(path, root)) {
        return Err("local push remote ancestor is outside the workspace".to_string());
    }
    paths.sort_by(|left, right| left.as_os_str().cmp(right.as_os_str()));
    paths.dedup_by(|left, right| same_path(left, right));
    let mut ancestors = Vec::with_capacity(paths.len());
    let mut handles = Vec::with_capacity(paths.len());
    for path in paths {
        if deadline.is_some_and(OperationDeadline::is_expired) {
            return Err("remote endpoint admission exceeded the operation deadline".to_string());
        }
        reject_reparse_components(&path)?;
        let identity = directory_identity_with_deadline(&path, deadline)?;
        ancestors.push((path.clone(), identity_token(&identity)));
        handles.push(open_endpoint_directory_handle(&path)?);
    }
    Ok((Arc::new(handles), Arc::new(ancestors)))
}

#[cfg(windows)]
fn open_endpoint_directory_handle(path: &Path) -> Result<fs::File, String> {
    // Endpoint/ancestor leases intentionally do not share delete. A local
    // or file remote must not be renamed or replaced while a child owns it.
    open_directory_handle(path)
}

#[cfg(not(windows))]
fn open_endpoint_directory_handle(path: &Path) -> Result<fs::File, String> {
    fs::File::open(path).map_err(|_| "local push remote ancestor cannot be held".to_string())
}

fn revalidate_remote_policy(root: &Path, policy: &RemotePolicy) -> Result<(), String> {
    revalidate_remote_policy_with_deadline(root, policy, None)
}

fn revalidate_remote_policy_with_deadline(
    root: &Path,
    policy: &RemotePolicy,
    deadline: Option<OperationDeadline>,
) -> Result<(), String> {
    if deadline.is_some_and(OperationDeadline::is_expired) {
        return Err("remote endpoint validation exceeded the operation deadline".to_string());
    }
    if !matches!(
        policy.transport(),
        RemoteTransport::Local | RemoteTransport::File
    ) {
        return Ok(());
    }
    let lease = policy
        .endpoint_lease()
        .ok_or_else(|| "local push remote has no retained endpoint lease".to_string())?;
    if lease.handles().is_empty() {
        return Err("local push remote endpoint handles are unavailable".to_string());
    }
    for (ancestor, expected_identity) in lease.ancestors().iter() {
        if deadline.is_some_and(OperationDeadline::is_expired) {
            return Err("remote endpoint validation exceeded the operation deadline".to_string());
        }
        reject_reparse_components(ancestor)?;
        let canonical_ancestor = fs::canonicalize(ancestor)
            .map_err(|_| "local push remote ancestor path changed".to_string())?;
        if !same_path(&canonical_ancestor, ancestor) {
            return Err("local push remote ancestor canonical path changed".to_string());
        }
        let identity = directory_identity_with_deadline(&canonical_ancestor, deadline)?;
        if deadline.is_some_and(OperationDeadline::is_expired) {
            return Err("remote endpoint validation exceeded the operation deadline".to_string());
        }
        if identity_token(&identity) != *expected_identity {
            return Err("local push remote ancestor identity changed".to_string());
        }
    }
    let path = match policy.transport() {
        RemoteTransport::Local => PathBuf::from(policy.endpoint()),
        RemoteTransport::File => url::Url::parse(policy.endpoint())
            .map_err(|_| "local push remote file URL changed".to_string())?
            .to_file_path()
            .map_err(|_| "local push remote file URL changed".to_string())?,
        RemoteTransport::Https | RemoteTransport::Ssh => return Ok(()),
    };
    let canonical =
        fs::canonicalize(&path).map_err(|_| "local push remote path changed".to_string())?;
    if !is_within(root, &canonical)
        || !same_path(&path, &canonical)
        || !same_path(&canonical, lease.path())
    {
        return Err("local push remote canonical path changed".to_string());
    }
    let identity = directory_identity_with_deadline(&canonical, deadline)?;
    if deadline.is_some_and(OperationDeadline::is_expired) {
        return Err("remote endpoint validation exceeded the operation deadline".to_string());
    }
    if policy.identity() != Some(identity_token(&identity).as_str())
        || lease.identity() != identity_token(&identity)
    {
        return Err("local push remote file identity changed".to_string());
    }
    if deadline.is_some_and(OperationDeadline::is_expired) {
        return Err("remote endpoint validation exceeded the operation deadline".to_string());
    }
    Ok(())
}

fn identity_token(identity: &FileIdentity) -> String {
    let mut hasher = Sha256::new();
    #[cfg(windows)]
    {
        hasher.update(identity.volume_serial_number.to_le_bytes());
        hasher.update(identity.file_index.to_le_bytes());
        hasher.update(identity.number_of_links.to_le_bytes());
        hasher.update(identity.file_size.to_le_bytes());
        hasher.update(identity.last_write_time.to_le_bytes());
    }
    #[cfg(unix)]
    {
        hasher.update(identity.device.to_le_bytes());
        hasher.update(identity.inode.to_le_bytes());
        hasher.update(identity.number_of_links.to_le_bytes());
        hasher.update(identity.file_size.to_le_bytes());
        hasher.update(identity.modified_seconds.to_le_bytes());
        hasher.update(identity.modified_nanos.to_le_bytes());
    }
    hasher.update(identity.content_digest);
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn validate_text(value: String, label: &str, max_bytes: usize) -> Result<String, GitError> {
    if value.trim().is_empty() || value.contains('\0') {
        return Err(GitError::InvalidRequest {
            message: format!("{label} must be non-empty and NUL-free"),
        });
    }
    if value.len() > max_bytes {
        return Err(GitError::InvalidRequest {
            message: format!("{label} exceeds the {max_bytes}-byte bound"),
        });
    }
    Ok(value)
}

fn contains_embedded_credentials(value: &str) -> bool {
    let Some(scheme_end) = value.find("://") else {
        return false;
    };
    let authority_start = scheme_end + 3;
    let authority_end = value[authority_start..]
        .find(|character: char| character == '/' || character == '?' || character == '#')
        .map_or(value.len(), |index| authority_start + index);
    value[authority_start..authority_end].contains('@')
}

fn looks_like_path(value: &str) -> bool {
    value.starts_with('/')
        || value.starts_with("\\\\")
        || (value.len() >= 3
            && value.as_bytes()[1] == b':'
            && matches!(value.as_bytes()[2], b'/' | b'\\'))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::{Mutex, OnceLock};

    #[test]
    fn read_only_policy_blocks_interactive_git_surfaces_without_removing_mutation_helpers() {
        let sandbox = GitSandbox::new().expect("create Git sandbox");
        let mut command = Command::new("git");
        command
            .env("GIT_ASKPASS", "credential-helper")
            .env("GIT_SSH_COMMAND", "ssh-with-helper");

        let executable = TrustedExecutable::resolve_git().expect("resolve installed Git");
        apply_git_policy(
            &mut command,
            &GitExecutionPolicy::ReadOnly,
            &sandbox,
            &executable.path,
        );
        apply_git_options(&mut command, &sandbox, &GitExecutionPolicy::ReadOnly);

        let environment = command
            .get_envs()
            .filter_map(|(key, value)| {
                value.map(|value| (key.to_os_string(), value.to_os_string()))
            })
            .collect::<std::collections::HashMap<_, _>>();
        assert_eq!(
            environment.get(&OsString::from("GIT_TERMINAL_PROMPT")),
            Some(&OsString::from("0"))
        );
        assert_eq!(
            environment.get(&OsString::from("GIT_PAGER")),
            Some(&OsString::from("cat"))
        );
        assert_eq!(
            environment.get(&OsString::from("GIT_EDITOR")),
            Some(&OsString::from(":"))
        );
        assert_eq!(
            environment.get(&OsString::from("GIT_SEQUENCE_EDITOR")),
            Some(&OsString::from(":"))
        );
        assert_eq!(environment.get(&OsString::from("GIT_ASKPASS")), None);
        assert_eq!(environment.get(&OsString::from("GIT_SSH_COMMAND")), None);
        assert_ne!(
            environment.get(&OsString::from("GIT_ALLOW_PROTOCOL")),
            Some(&OsString::from("file")),
            "read-only policy must not globally authorize file transport"
        );

        let arguments = command
            .get_args()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert!(arguments
            .iter()
            .any(|argument| argument.starts_with("core.hooksPath=")));
        assert!(arguments
            .windows(2)
            .any(|pair| pair == ["-c", "core.fsmonitor=false"]));

        let mut mutation = Command::new("git");
        mutation
            .env("GIT_ASKPASS", "credential-helper")
            .env("GIT_SSH_COMMAND", "ssh-with-helper")
            .env("GIT_DIR", "attacker-git-dir")
            .env("GIT_WORK_TREE", "attacker-work-tree")
            .env("GIT_INDEX_FILE", "attacker-index")
            .env("GIT_COMMON_DIR", "attacker-common-dir")
            .env("GIT_CONFIG_PARAMETERS", "attacker.config=helper")
            .env("GIT_EXTERNAL_DIFF", "attacker-diff")
            .env("GIT_SSH", "attacker-ssh")
            .env("GIT_SSH_VARIANT", "ssh");
        apply_git_policy(
            &mut mutation,
            &GitExecutionPolicy::AuthorizedMutation {
                capability: GitCapability::Push,
                remote: None,
                remote_name: None,
            },
            &sandbox,
            &executable.path,
        );
        let mutation_environment = mutation
            .get_envs()
            .filter_map(|(key, value)| {
                value.map(|value| (key.to_os_string(), value.to_os_string()))
            })
            .collect::<std::collections::HashMap<_, _>>();
        for key in [
            "GIT_ASKPASS",
            "GIT_SSH",
            "GIT_SSH_COMMAND",
            "GIT_SSH_VARIANT",
            "GIT_DIR",
            "GIT_WORK_TREE",
            "GIT_INDEX_FILE",
            "GIT_COMMON_DIR",
            "GIT_CONFIG_PARAMETERS",
            "GIT_EXTERNAL_DIFF",
        ] {
            assert_eq!(
                mutation_environment.get(&OsString::from(key)),
                None,
                "{key}"
            );
        }
    }

    #[test]
    fn trusted_git_resolution_rejects_ambiguous_path_and_replacement_identity() {
        let fixture = tempfile::tempdir().expect("create Git shim fixture");
        let executable_name = if cfg!(windows) { "git.exe" } else { "git" };
        let first_directory = fixture.path().join("first");
        let second_directory = fixture.path().join("second");
        fs::create_dir(&first_directory).expect("create first shim directory");
        fs::create_dir(&second_directory).expect("create second shim directory");
        let first_executable = first_directory.join(executable_name);
        let second_executable = second_directory.join(executable_name);
        fs::write(&first_executable, b"first Git shim").expect("write first shim");
        fs::write(&second_executable, b"second Git shim").expect("write second shim");

        let ambiguous_path =
            env::join_paths([&first_directory, &second_directory]).expect("encode ambiguous PATH");
        let error = TrustedExecutable::resolve_from_path(&ambiguous_path)
            .expect_err("multiple Git installations must be rejected");
        assert!(error.contains("multiple installations") || error.contains("trusted"));

        let trusted = TrustedExecutable::issue_test_fixture(&first_executable)
            .expect("issue explicit test-only trusted fixture");
        fs::write(
            &first_executable,
            b"replacement Git shim with a different identity",
        )
        .expect("replace shim contents");
        assert!(
            trusted.verify().is_err(),
            "replacement at the trusted path must fail identity verification"
        );
    }

    #[test]
    fn graph_digest_rejects_content_beyond_its_hard_byte_cap() {
        let fixture = tempfile::tempdir().expect("create graph digest fixture");
        let path = fixture.path().join("graph-input");
        fs::write(&path, b"0123456789").expect("write graph input");
        let mut file = fs::File::open(&path).expect("open graph input");

        let error = digest_file(&mut file, None, Some(9))
            .expect_err("graph digest must enforce the hard byte cap while reading");
        assert!(
            error.contains("size limit"),
            "unexpected graph digest error: {error}"
        );
    }

    #[test]
    fn reparse_component_walk_rejects_an_unbounded_path_depth() {
        let mut path = if cfg!(windows) {
            PathBuf::from(r"C:\Temp")
        } else {
            PathBuf::from("/tmp")
        };
        for index in 0..=HARD_MAX_GRAPH_DEPTH {
            path.push(format!("depth-{index}"));
        }

        assert!(
            reject_reparse_components(&path).is_err(),
            "path component inspection must enforce the graph depth cap"
        );
    }

    #[test]
    fn native_executable_header_read_honors_an_expired_deadline() {
        let fixture = tempfile::tempdir().expect("create executable identity fixture");
        let path = fixture.path().join("git-image");
        fs::write(&path, b"MZ").expect("write executable header");

        assert!(
            validate_native_image(&path, Some(OperationDeadline::from_now(Duration::ZERO)))
                .is_err(),
            "native executable validation must not read after its deadline expires"
        );
    }

    #[test]
    fn trusted_git_resolution_rejects_invalid_path_entries_instead_of_skipping_them() {
        let fixture = tempfile::tempdir().expect("create PATH fixture");
        let executable_name = if cfg!(windows) { "git.exe" } else { "git" };
        let directory = fixture.path().join("native");
        fs::create_dir(&directory).expect("create PATH directory");
        fs::write(directory.join(executable_name), b"not a native Git image")
            .expect("write fake Git image");

        for path_value in [
            OsString::new(),
            OsString::from("."),
            env::join_paths([Path::new(".")]).expect("encode current-directory PATH"),
            env::join_paths([Path::new("relative")]).expect("encode relative PATH"),
            env::join_paths([directory.clone()]).expect("encode fake PATH"),
        ] {
            let error = TrustedExecutable::resolve_from_path(&path_value)
                .expect_err("untrusted PATH entries must fail closed");
            assert!(
                error.contains("PATH entry") || error.contains("trusted"),
                "unexpected resolver error: {error}"
            );
        }
    }

    #[cfg(windows)]
    #[test]
    fn windows_executable_binding_blocks_path_replacement_until_spawn_binding_drops() {
        fs::create_dir_all(r"C:\Temp").expect("create bounded test root");
        let fixture = tempfile::Builder::new()
            .prefix("devmanager-phase66-git-executable-")
            .tempdir_in(Path::new(r"C:\Temp"))
            .expect("create executable binding fixture");
        let source = TrustedExecutable::resolve_git()
            .expect("resolve installed Git")
            .path;
        let executable_path = fixture.path().join("git.exe");
        fs::copy(&source, &executable_path).expect("copy Git executable fixture");
        let executable = TrustedExecutable::issue_test_fixture(&executable_path)
            .expect("issue trusted executable fixture");
        let binding = executable
            .bind_with_deadline(None)
            .expect("bind executable for spawn");
        let replacement = fixture.path().join("git-replacement.exe");

        assert!(
            fs::rename(&executable_path, &replacement).is_err(),
            "the spawn binding must hold a no-delete-share handle against path replacement"
        );
        drop(binding);
        fs::rename(&executable_path, &replacement)
            .expect("path replacement must be possible after the spawn binding is released");
    }

    #[cfg(windows)]
    #[test]
    fn windows_executable_binding_holds_ancestor_directories_until_spawn_binding_drops() {
        fs::create_dir_all(r"C:\Temp").expect("create bounded test root");
        let fixture = tempfile::Builder::new()
            .prefix("devmanager-phase66-git-executable-ancestor-")
            .tempdir_in(Path::new(r"C:\Temp"))
            .expect("create executable ancestor fixture");
        let source = TrustedExecutable::resolve_git()
            .expect("resolve installed Git")
            .path;
        let executable_path = fixture.path().join("git.exe");
        fs::copy(&source, &executable_path).expect("copy Git executable fixture");
        let executable = TrustedExecutable::issue_test_fixture(&executable_path)
            .expect("issue trusted executable fixture");
        let binding = executable
            .bind_with_deadline(None)
            .expect("bind executable for spawn");
        assert!(
            !binding.ancestor_handles.is_empty(),
            "the spawn binding must retain executable ancestor handles"
        );
        let moved = fixture.path().with_file_name(format!(
            "{}-moved",
            fixture
                .path()
                .file_name()
                .expect("fixture directory name")
                .to_string_lossy()
        ));

        let rename_result = fs::rename(fixture.path(), &moved);
        if rename_result.is_ok() {
            fs::rename(&moved, fixture.path()).expect("restore renamed fixture directory");
        }
        assert!(
            rename_result.is_err(),
            "the spawn binding must hold executable ancestors against replacement"
        );
        drop(binding);
    }

    #[test]
    fn git_policy_executes_with_only_explicit_environment_and_transport() {
        let _guard = process_test_guard();
        let sandbox = GitSandbox::new().expect("create Git sandbox");
        let mut command = if cfg!(windows) {
            let mut command = Command::new("cmd.exe");
            command.args(["/c", "set"]);
            command
        } else {
            let mut command = Command::new("sh");
            command.args(["-c", "env"]);
            command
        };
        command
            .env("GIT_TRACE", "ambient-trace-secret")
            .env("GIT_TRACE2", "ambient-trace2-secret")
            .env("GIT_TRACE_PACKET", "ambient-trace-packet-secret")
            .env("GIT_CONFIG_PARAMETERS", "ambient.config=helper")
            .env("GIT_CONFIG_GLOBAL", "ambient-global-config")
            .env("GIT_CONFIG_SYSTEM", "ambient-system-config")
            .env("GIT_ATTRIBUTES_FILE", "ambient-attributes")
            .env("GIT_OBJECT_DIRECTORY", "ambient-object-directory")
            .env("GIT_ALTERNATE_OBJECT_DIRECTORIES", "ambient-alternates")
            .env("GIT_SSH_COMMAND", "ambient-ssh-secret")
            .env("GIT_PROXY_COMMAND", "ambient-proxy-secret")
            .env("HTTP_PROXY", "ambient-http-proxy-secret")
            .env("HTTPS_PROXY", "ambient-https-proxy-secret")
            .env("ALL_PROXY", "ambient-all-proxy-secret")
            .env("DEVMANAGER_GIT_AMBIENT_SENTINEL", "ambient-secret");

        let executable = TrustedExecutable::resolve_git().expect("resolve installed Git");
        apply_git_policy(
            &mut command,
            &GitExecutionPolicy::ReadOnly,
            &sandbox,
            &executable.path,
        );
        let output = command.output().expect("run environment probe");
        assert!(output.status.success());
        let environment = String::from_utf8_lossy(&output.stdout);
        for value in [
            "ambient-trace-secret",
            "ambient.config=helper",
            "ambient-ssh-secret",
            "ambient-proxy-secret",
            "ambient-secret",
        ] {
            assert!(
                !environment.contains(value),
                "ambient environment leaked into Git child: {value}"
            );
        }
        assert!(environment.contains("GIT_CONFIG_NOSYSTEM=1"));
        assert!(environment.contains("GIT_TERMINAL_PROMPT=0"));
        assert!(!environment.contains("GIT_ALLOW_PROTOCOL=file"));
    }

    #[test]
    fn fake_config_attributes_helpers_and_network_hooks_cannot_escape_the_sandbox() {
        let _guard = process_test_guard();
        let fixture = tempfile::tempdir().expect("create fake Git input fixture");
        let fake_config = fixture.path().join("fake-config");
        let fake_attributes = fixture.path().join("fake-attributes");
        let marker = "external-git-policy-marker";
        fs::write(
            &fake_config,
            format!(
                "[alias]\n\tstatus = !echo {marker}\n[credential]\n\thelper = !echo {marker}\n[core]\n\tsshCommand = echo {marker}\n[http]\n\tproxy = http://{marker}.invalid\n"
            ),
        )
        .expect("write fake global config");
        fs::write(&fake_attributes, format!("*.txt diff={marker}\n"))
            .expect("write fake global attributes");

        let executable = TrustedExecutable::resolve_git().expect("resolve installed Git");
        let sandbox = GitSandbox::new().expect("create Git sandbox");
        let mut config_probe = Command::new(&executable.path);
        config_probe
            .env("GIT_CONFIG_GLOBAL", &fake_config)
            .env("GIT_CONFIG_SYSTEM", &fake_config)
            .env("GIT_ATTRIBUTES_FILE", &fake_attributes)
            .env("GIT_TRACE", marker)
            .env("GIT_SSH_COMMAND", marker)
            .env("GIT_PROXY_COMMAND", marker)
            .env("HTTP_PROXY", marker)
            .env("HTTPS_PROXY", marker)
            .env("GIT_CREDENTIAL_HELPER", marker);
        apply_git_policy(
            &mut config_probe,
            &GitExecutionPolicy::ReadOnly,
            &sandbox,
            &executable.path,
        );
        apply_git_options(&mut config_probe, &sandbox, &GitExecutionPolicy::ReadOnly);
        config_probe.args(["config", "--list", "--show-origin"]);
        let config_output = config_probe.output().expect("run fake config probe");
        assert!(
            config_output.status.success(),
            "config probe failed: {}",
            String::from_utf8_lossy(&config_output.stderr)
        );
        assert!(
            !String::from_utf8_lossy(&config_output.stdout).contains(marker),
            "fake config/helper/proxy/SSH/trace values escaped the isolated config"
        );

        let repo = fixture.path().join("repo");
        fs::create_dir(&repo).expect("create attributes probe repository");
        let init = Command::new(&executable.path)
            .current_dir(&repo)
            .args(["init", "--initial-branch=main"])
            .output()
            .expect("initialize attributes probe repository");
        assert!(init.status.success());
        let mut attributes_probe = Command::new(&executable.path);
        attributes_probe
            .current_dir(&repo)
            .env("GIT_ATTRIBUTES_FILE", &fake_attributes)
            .env("GIT_CONFIG_GLOBAL", &fake_config)
            .env("GIT_CONFIG_SYSTEM", &fake_config);
        apply_git_policy(
            &mut attributes_probe,
            &GitExecutionPolicy::ReadOnly,
            &sandbox,
            &executable.path,
        );
        apply_git_options(
            &mut attributes_probe,
            &sandbox,
            &GitExecutionPolicy::ReadOnly,
        );
        attributes_probe.args(["check-attr", "--all", "--", "marker.txt"]);
        let attributes_output = attributes_probe
            .output()
            .expect("run fake attributes probe");
        assert!(attributes_output.status.success());
        assert!(
            !String::from_utf8_lossy(&attributes_output.stdout).contains(marker),
            "fake attributes escaped the isolated attributes file"
        );
    }

    #[test]
    fn remote_mutation_confirmation_requires_the_exact_authorized_endpoint() {
        let fixture = tempfile::tempdir().expect("create remote policy fixture");
        let root = fixture.path();
        let run_git = |arguments: &[&str]| {
            let output = Command::new("git")
                .current_dir(root)
                .args(arguments)
                .env("GIT_TERMINAL_PROMPT", "0")
                .output()
                .expect("run fixture Git");
            assert!(
                output.status.success(),
                "fixture Git failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        };
        run_git(&["init", "--initial-branch=main"]);
        run_git(&[
            "config",
            "remote.origin.url",
            "https://example.invalid/authorized/repo.git",
        ]);
        fs::write(root.join("tracked.txt"), "fixture\n").expect("write fixture file");

        let repository = GitRepository::with_limits(
            root,
            GitLimits {
                timeout: HARD_MAX_TIMEOUT,
                ..GitLimits::default()
            },
        )
        .expect("open fixture repository");
        let plan = repository
            .plan_push(Some("origin"), Some("main"))
            .expect("build push plan");
        let gate = GitCapabilityGate::new([GitCapability::Push]);
        assert!(
            gate.confirm(&plan, GitOperationPermit::test_mutation(&plan))
                .is_err(),
            "a transport endpoint must not be authorized by capability alone"
        );

        let mut exact_gate = GitCapabilityGate::new([GitCapability::Push]);
        exact_gate.authorize_remote(plan.remote_policy().clone());
        assert!(exact_gate
            .confirm(&plan, GitOperationPermit::test_mutation(&plan))
            .is_ok());

        let mut different_gate = GitCapabilityGate::new([GitCapability::Push]);
        different_gate.authorize_remote(
            RemotePolicy::https("https://example.invalid/authorized/other.git")
                .expect("build different endpoint policy"),
        );
        assert!(
            different_gate
                .confirm(&plan, GitOperationPermit::test_mutation(&plan))
                .is_err(),
            "authorization must bind the exact endpoint"
        );

        let ssh = RemotePolicy::ssh("ssh://git@example.invalid/authorized/repo.git")
            .expect("SSH usernames are not passwords");
        assert_eq!(ssh.transport(), RemoteTransport::Ssh);
        assert_eq!(
            ssh.endpoint(),
            "ssh://git@example.invalid/authorized/repo.git"
        );
        assert!(
            RemotePolicy::https("https://user:password@example.invalid/repo.git").is_err(),
            "HTTPS credentials must never enter a mutation policy"
        );
    }

    #[test]
    fn local_and_file_remote_policies_are_contained_and_identity_bound() {
        let fixture = tempfile::tempdir().expect("create local remote policy fixture");
        let root = fixture.path();
        let remote = root.join("remote.git");
        fs::create_dir(&remote).expect("create contained remote");
        let local = remote.to_string_lossy().into_owned();
        let local_policy = remote_policy_from_url(root, &local).expect("build local policy");
        assert_eq!(local_policy.transport(), RemoteTransport::Local);
        assert!(local_policy.identity().is_some());

        let file_url = url::Url::from_file_path(&remote)
            .expect("encode contained remote as file URL")
            .to_string();
        let file_policy = remote_policy_from_url(root, &file_url).expect("build file policy");
        assert_eq!(file_policy.transport(), RemoteTransport::File);
        assert_ne!(file_policy.endpoint(), local_policy.endpoint());
        assert_eq!(
            url::Url::parse(file_policy.endpoint())
                .expect("file policy endpoint is a URL")
                .to_file_path()
                .expect("file policy endpoint is a file URL"),
            remote
        );
        revalidate_remote_policy(root, &file_policy).expect("unchanged file policy revalidates");

        let replacement = root.join("replacement.git");
        #[cfg(not(windows))]
        {
            fs::rename(&remote, &replacement).expect("move original remote");
            fs::create_dir(&remote).expect("replace remote path");
            assert!(
                revalidate_remote_policy(root, &file_policy).is_err(),
                "a replacement at the exact remote path must fail identity validation"
            );
        }
        #[cfg(windows)]
        {
            assert!(
                fs::rename(&remote, &replacement).is_err(),
                "retained endpoint handles must prevent a path swap"
            );
            revalidate_remote_policy(root, &file_policy)
                .expect("the endpoint remains valid while its handle is retained");
        }
    }

    #[test]
    fn local_operation_permit_requires_the_exact_retained_endpoint_lease() {
        let fixture = tempfile::tempdir().expect("create endpoint lease fixture");
        let root = fixture.path();
        let remote = root.join("remote.git");
        fs::create_dir(&remote).expect("create contained remote");
        let remote_value = remote.to_string_lossy().into_owned();
        let issued_policy =
            remote_policy_from_url(root, &remote_value).expect("issue local endpoint policy");
        let forged_policy =
            remote_policy_from_url(root, &remote_value).expect("issue second endpoint policy");
        assert_eq!(issued_policy, forged_policy);

        let arguments = vec![OsString::from("push")];
        let permit =
            GitOperationPermit::test_service_mutation(&arguments, Some(issued_policy), None);
        let forged_execution = GitExecutionPolicy::ServiceMutation {
            remote: Some(forged_policy),
            remote_name: None,
        };
        assert!(
            !permit.operation_matches_policy(&forged_execution, &arguments),
            "a policy with a different retained endpoint lease must not authorize the operation"
        );
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn replaced_repository_root_is_rejected_before_spawn() {
        let _guard = process_test_guard();
        #[cfg(windows)]
        fs::create_dir_all(r"C:\Temp").expect("create bounded test root");
        #[cfg(windows)]
        let parent = tempfile::Builder::new()
            .prefix("devmanager-phase66-git-root-")
            .tempdir_in(Path::new(r"C:\Temp"))
            .expect("create root replacement fixture");
        #[cfg(unix)]
        let parent = tempfile::tempdir().expect("create root replacement fixture");
        let original = parent.path().join("repo");
        fs::create_dir(&original).expect("create original repository root");
        let repository = test_repository(
            &original,
            GitLimits {
                timeout: Duration::from_secs(2),
                ..GitLimits::default()
            },
        );
        let moved = parent.path().join("moved");
        #[cfg(windows)]
        {
            if fs::rename(&original, &moved).is_err() {
                // A delete-capable handle with delete sharing disabled blocks
                // the replacement itself; that is the strongest containment
                // proof available before CreateProcess.
                return;
            }
            fs::create_dir(&original).expect("replace repository root path");
        }
        #[cfg(unix)]
        {
            fs::rename(&original, &moved).expect("move original repository root");
            fs::create_dir(&original).expect("replace repository root path");
        }

        let arguments = if cfg!(windows) {
            vec![OsString::from("/c"), OsString::from("exit 0")]
        } else {
            vec![OsString::from("-c"), OsString::from("exit 0")]
        };
        let error = repository
            .run_test_process(
                Path::new(if cfg!(windows) { "cmd.exe" } else { "sh" }),
                &arguments,
                GitExecutionPolicy::ReadOnly,
            )
            .expect_err("a replaced repository root must fail closed");
        assert!(matches!(
            error,
            GitError::InvalidRepositoryRoot { .. }
                | GitError::CommandStart { .. }
                | GitError::CleanupFailed { .. }
        ));
    }

    #[test]
    fn debug_surfaces_do_not_include_command_or_output_payloads() {
        let error = GitError::CommandFailed {
            operation: "secret-argument".to_string(),
            code: Some(17),
            stderr: "https://user:password@example.invalid/private/path secret-output".to_string(),
        };
        let debug = format!("{error:?}");
        let display = error.to_string();
        for value in [
            "secret-argument",
            "password",
            "example.invalid",
            "private/path",
            "secret-output",
        ] {
            assert!(!debug.contains(value), "debug leaked {value}: {debug}");
            assert!(
                !display.contains(value),
                "display leaked {value}: {display}"
            );
        }

        let output = GitOutput {
            stdout: b"secret-stdout".to_vec(),
            stderr: b"secret-stderr".to_vec(),
            status: Command::new(if cfg!(windows) { "cmd.exe" } else { "sh" })
                .args(if cfg!(windows) {
                    ["/c", "exit 0"]
                } else {
                    ["-c", "exit 0"]
                })
                .status()
                .expect("create test exit status"),
        };
        let output_debug = format!("{output:?}");
        assert!(!output_debug.contains("secret-stdout"));
        assert!(!output_debug.contains("secret-stderr"));

        let invocation = GitInvocation {
            executable: OsString::from(r"C:\secret\git.exe"),
            cwd: PathBuf::from(r"C:\secret\repo"),
            arguments: vec![OsString::from("secret-argument")],
        };
        let invocation_debug = format!("{invocation:?}");
        assert!(!invocation_debug.contains("C:\\secret"));
        assert!(!invocation_debug.contains("secret-argument"));
    }

    #[test]
    fn reader_join_cancels_a_nonblocking_reader_at_the_deadline() {
        struct CancellableReader;

        impl Read for CancellableReader {
            fn read(&mut self, _buffer: &mut [u8]) -> io::Result<usize> {
                Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "fixture is waiting",
                ))
            }
        }

        let mut worker =
            ReaderWorker::spawn("stdout", CancellableReader, 64).expect("spawn reader");
        let started = Instant::now();
        let result = worker.join(OperationDeadline::from_now(Duration::ZERO));
        if let Err(error) = result {
            assert!(
                error.contains("reaper"),
                "an uncancellable reader must be retained visibly: {error}"
            );
        }

        assert!(
            started.elapsed() < Duration::from_millis(50),
            "reader join blocked after cancellation: {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn reader_drop_closes_a_blocking_reader_before_joining() {
        struct BlockingReader {
            released: Arc<AtomicBool>,
        }

        impl Read for BlockingReader {
            fn read(&mut self, _buffer: &mut [u8]) -> io::Result<usize> {
                while !self.released.load(Ordering::Acquire) {
                    thread::sleep(Duration::from_millis(1));
                }
                Ok(0)
            }
        }

        let released = Arc::new(AtomicBool::new(false));
        let release_for_cancel = Arc::clone(&released);
        let worker = ReaderWorker::spawn_with_cancel(
            "stdout",
            BlockingReader {
                released: Arc::clone(&released),
            },
            64,
            move || release_for_cancel.store(true, Ordering::Release),
        )
        .expect("spawn blocking reader");
        let started = Instant::now();
        drop(worker);
        assert!(
            started.elapsed() < Duration::from_millis(100),
            "reader Drop exceeded bounded cancellation: {:?}",
            started.elapsed()
        );
        assert!(released.load(Ordering::Acquire));
    }

    #[test]
    fn reader_join_does_not_wait_for_a_reader_that_ignores_cancellation() {
        struct SlowReader {
            finished: Arc<AtomicBool>,
        }

        impl Read for SlowReader {
            fn read(&mut self, _buffer: &mut [u8]) -> io::Result<usize> {
                thread::sleep(Duration::from_millis(120));
                self.finished.store(true, Ordering::Release);
                Ok(0)
            }
        }

        let finished = Arc::new(AtomicBool::new(false));
        let mut worker = ReaderWorker::spawn(
            "stdout",
            SlowReader {
                finished: Arc::clone(&finished),
            },
            64,
        )
        .expect("spawn slow reader");
        let started = Instant::now();
        let error = worker
            .join(OperationDeadline::from_now(Duration::from_millis(5)))
            .expect_err("an uncancellable reader must be retained by the bounded reaper");
        assert!(
            error.contains("reaper"),
            "unexpected reader result: {error}"
        );
        assert!(
            started.elapsed() < Duration::from_millis(60),
            "reader join exceeded its absolute deadline: {:?}",
            started.elapsed()
        );
        while !finished.load(Ordering::Acquire) {
            thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn operation_state_requires_an_owned_read_permit() {
        let fixture = tempfile::tempdir().expect("create operation-state fixture");
        let repository = test_repository(fixture.path(), GitLimits::default());
        repository
            .read_permit
            .lock()
            .expect("read permit lock")
            .take()
            .expect("fixture must own a read permit");

        assert!(matches!(
            repository.operation_state(),
            Err(GitError::AuthorityUnavailable)
        ));
    }

    #[test]
    fn read_permit_starts_a_fresh_deadline_at_each_operation_boundary() {
        let fixture = tempfile::tempdir().expect("create read-permit fixture");
        let repository = test_repository(
            fixture.path(),
            GitLimits {
                timeout: Duration::from_millis(500),
                max_stdout_bytes: 64 * 1024,
                max_stderr_bytes: 64 * 1024,
            },
        );
        let first = repository.take_read_permit().expect("initial read permit");
        let first_deadline = first.deadline.deadline;
        repository
            .restore_read_permit(first)
            .expect("restore initial read permit");
        thread::sleep(Duration::from_millis(20));

        let second = repository.take_read_permit().expect("fresh read permit");
        assert!(
            second.deadline.deadline > first_deadline,
            "read operations must not reuse an earlier operation deadline"
        );
    }

    #[test]
    fn mutation_execution_cannot_restart_an_expired_absolute_deadline() {
        let arguments = vec![OsString::from("add"), OsString::from("--")];
        let permit = GitOperationPermit::test_service_mutation_with_timeout(
            &arguments,
            None,
            None,
            Duration::from_millis(5),
        );
        thread::sleep(Duration::from_millis(20));

        assert!(matches!(
            permit.renewed_for_execution(),
            Err(GitError::AuthorityUnavailable)
        ));
    }

    #[test]
    fn service_path_validation_requires_an_owned_read_permit() {
        let fixture = tempfile::tempdir().expect("create service-path fixture");
        let repository = test_repository(fixture.path(), GitLimits::default());
        repository
            .read_permit
            .lock()
            .expect("read permit lock")
            .take()
            .expect("fixture must own a read permit");

        assert!(matches!(
            repository.validate_service_path(&RepoPath::from("safe.txt")),
            Err(GitError::AuthorityUnavailable)
        ));
    }

    #[test]
    fn local_remote_admission_rejects_an_expired_operation_deadline() {
        let fixture = tempfile::tempdir().expect("create remote deadline fixture");
        let remote = fixture.path().join("remote");
        fs::create_dir(&remote).expect("create local remote");
        let error = remote_policy_from_url_with_deadline(
            fixture.path(),
            "remote",
            Some(OperationDeadline::from_now(Duration::ZERO)),
        )
        .expect_err("expired remote admission must fail closed");
        assert!(matches!(error, GitError::TimedOut { .. }));
    }

    #[test]
    fn running_git_rejects_a_graph_swap_and_restore_during_the_effect() {
        let fixture = tempfile::tempdir().expect("create graph swap fixture");
        fs::create_dir_all(fixture.path().join(".git")).expect("create graph fixture Git dir");
        fs::write(
            fixture.path().join(".git").join("HEAD"),
            b"ref: refs/heads/main\n",
        )
        .expect("create graph fixture HEAD");
        let repository = test_repository(fixture.path(), GitLimits::default());
        let head = fixture.path().join(".git").join("HEAD");
        let moved = fixture.path().join(".git").join("HEAD.review-swap");
        let restore_head = head.clone();
        let restore_moved = moved.clone();
        let swap = thread::spawn(move || {
            thread::sleep(Duration::from_millis(40));
            fs::rename(&restore_head, &restore_moved).expect("move HEAD out of graph");
            thread::sleep(Duration::from_millis(40));
            fs::rename(&restore_moved, &restore_head).expect("restore HEAD");
        });

        let arguments = if cfg!(windows) {
            vec![
                OsString::from("/C"),
                OsString::from("ping 127.0.0.1 -n 4 > NUL"),
            ]
        } else {
            vec![OsString::from("-c"), OsString::from("sleep 1")]
        };
        let result = repository.run_test_process(
            Path::new(if cfg!(windows) { "cmd.exe" } else { "sh" }),
            &arguments,
            GitExecutionPolicy::ReadOnly,
        );
        swap.join().expect("swap thread");
        assert!(result.is_err(), "a transient graph swap must fail closed");
    }

    #[test]
    fn stage_transition_rejects_an_unrecognized_object_path() {
        let fixture = tempfile::tempdir().expect("create object transition fixture");
        let repository = test_repository(fixture.path(), GitLimits::default());
        let object_store = fixture.path().join(".git").join("objects");
        fs::create_dir(object_store.join("evil")).expect("create unexpected object fanout");
        fs::write(
            object_store.join("evil").join("payload"),
            b"not a Git object",
        )
        .expect("create unexpected object payload");

        let error = repository
            .root
            .graph
            .revalidate_after_transition(GraphTransition::Stage)
            .expect_err("stage must not adopt arbitrary object-store content");
        assert!(error.contains("outside the operation transition"));
    }

    #[test]
    fn stage_transition_rejects_deletion_of_a_bound_object() {
        let fixture = tempfile::tempdir().expect("create object deletion fixture");
        let object_store = fixture.path().join(".git").join("objects");
        let fanout = object_store.join("ab");
        fs::create_dir_all(&fanout).expect("create Git object fanout");
        let object = fanout.join("01234567890123456789012345678901234567");
        fs::write(&object, b"bound object").expect("create bound object");
        let repository = test_repository(fixture.path(), GitLimits::default());

        fs::remove_file(&object).expect("delete bound object");
        let error = repository
            .root
            .graph
            .revalidate_after_transition(GraphTransition::Stage)
            .expect_err("stage must not delete an existing object");
        assert!(error.contains("removed or replaced"));
    }

    #[test]
    fn in_flight_transition_does_not_advance_the_read_baseline() {
        let fixture = tempfile::tempdir().expect("create object baseline fixture");
        let repository = test_repository(fixture.path(), GitLimits::default());
        let object_store = fixture.path().join(".git").join("objects");
        let fanout = object_store.join("ab");
        fs::create_dir(&fanout).expect("create Git object fanout");
        fs::write(
            fanout.join("01234567890123456789012345678901234567"),
            b"new object",
        )
        .expect("create Git-shaped object");

        repository
            .root
            .graph
            .revalidate_during_transition_with_deadline(
                GraphTransition::Stage,
                Some(OperationDeadline::from_now(HARD_MAX_TIMEOUT)),
            )
            .expect("Git-shaped object may appear during an authorized stage");
        assert!(repository.root.graph.revalidate().is_err());
    }

    #[cfg(unix)]
    #[test]
    fn timed_out_command_terminates_and_reaps_its_owned_process_group() {
        let _guard = process_test_guard();
        let fixture = tempfile::tempdir().expect("create process-group fixture");
        let child_pid_file = fixture.path().join("child.pid");
        let repository = test_repository(
            fixture.path(),
            GitLimits {
                timeout: Duration::from_millis(250),
                max_stdout_bytes: 64 * 1024,
                max_stderr_bytes: 64 * 1024,
            },
        );
        let arguments = vec![
            OsString::from("-c"),
            OsString::from("sleep 30 & echo $! > \"$1\"; wait"),
            OsString::from("devmanager-git-process-test"),
            child_pid_file.clone().into_os_string(),
        ];

        let error = repository
            .run_test_process(Path::new("sh"), &arguments, GitExecutionPolicy::ReadOnly)
            .expect_err("the process group must exceed the operation deadline");
        assert!(matches!(
            error,
            GitError::TimedOut { .. } | GitError::CleanupFailed { .. }
        ));

        let child_pid = wait_for_pid_file(&child_pid_file);
        let started = Instant::now();
        while unix_pid_exists(child_pid) && started.elapsed() < Duration::from_secs(2) {
            thread::sleep(Duration::from_millis(10));
        }
        assert!(
            !unix_pid_exists(child_pid),
            "owned process-group descendant {child_pid} survived cleanup"
        );
    }

    #[cfg(windows)]
    #[test]
    fn unmanaged_suspended_child_cleanup_uses_its_owned_process_handle() {
        let _guard = process_test_guard();
        let mut child = Command::new("cmd.exe")
            .creation_flags(MANAGED_PROCESS_CREATION_FLAGS)
            .args(["/c", "exit 0"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn suspended setup child");
        let cleanup = cleanup_unmanaged_child(
            &mut child,
            OperationDeadline::from_now(Duration::from_secs(2)),
        );
        assert!(cleanup.is_none(), "setup child cleanup failed: {cleanup:?}");
        assert!(
            child.try_wait().expect("poll setup child").is_some(),
            "owned setup child must be reaped"
        );
    }

    #[cfg(windows)]
    #[test]
    fn timeout_cleanup_kills_job_descendants_before_the_single_deadline() {
        use crate::services::platform_service::is_pid_running;

        let _guard = process_test_guard();
        let fixture = ProcessFixture::new();
        let repository = test_repository(
            fixture.root.path(),
            GitLimits {
                timeout: Duration::from_secs(5),
                max_stdout_bytes: 64 * 1024,
                max_stderr_bytes: 64 * 1024,
            },
        );
        let started = Instant::now();

        let error = repository
            .run_test_process(
                Path::new("powershell.exe"),
                &fixture.timeout_arguments(),
                GitExecutionPolicy::ReadOnly,
            )
            .expect_err("the helper must exceed the operation deadline");

        assert!(matches!(
            error,
            GitError::TimedOut { .. } | GitError::CleanupFailed { .. }
        ));
        assert!(
            started.elapsed() < Duration::from_millis(7000),
            "cleanup exceeded the bounded deadline: {:?}",
            started.elapsed()
        );

        let child_pid = fixture.wait_for_child_pid();
        let check_started = Instant::now();
        while is_pid_running(child_pid) && check_started.elapsed() < Duration::from_secs(2) {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            !is_pid_running(child_pid),
            "Job descendant {child_pid} survived cleanup"
        );
    }

    #[cfg(windows)]
    #[test]
    fn timeout_cleanup_does_not_kill_an_unrelated_sentinel_process() {
        use crate::services::platform_service::is_pid_running;

        let _guard = process_test_guard();
        let mut sentinel = Command::new("ping.exe")
            .args(["-n", "30", "127.0.0.1"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn unrelated sentinel");
        let sentinel_pid = sentinel.id();
        let fixture = ProcessFixture::new();
        let repository = test_repository(
            fixture.root.path(),
            GitLimits {
                timeout: Duration::from_secs(5),
                max_stdout_bytes: 64 * 1024,
                max_stderr_bytes: 64 * 1024,
            },
        );

        let error = repository
            .run_test_process(
                Path::new("powershell.exe"),
                &fixture.timeout_arguments(),
                GitExecutionPolicy::ReadOnly,
            )
            .expect_err("the helper must exceed the operation deadline");
        assert!(matches!(
            error,
            GitError::TimedOut { .. } | GitError::CleanupFailed { .. }
        ));
        assert!(
            is_pid_running(sentinel_pid),
            "the runner must not terminate a process outside its managed Job"
        );
        sentinel.wait().expect("reap unrelated sentinel");
    }

    #[cfg(windows)]
    #[test]
    fn bounded_output_is_reported_without_leaving_the_managed_process_running() {
        let _guard = process_test_guard();
        let fixture = ProcessFixture::new();
        let repository = test_repository(
            fixture.root.path(),
            GitLimits {
                timeout: Duration::from_secs(5),
                max_stdout_bytes: 128,
                max_stderr_bytes: 128,
            },
        );
        let started = Instant::now();

        let error = repository
            .run_test_process(
                Path::new("powershell.exe"),
                &fixture.output_arguments(),
                GitExecutionPolicy::ReadOnly,
            )
            .expect_err("the helper must exceed the stdout bound");

        assert!(
            matches!(
                error,
                GitError::OutputLimitExceeded {
                    stream: "stdout",
                    ..
                }
            ),
            "unexpected bounded stdout error: {error:?}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "bounded output cleanup exceeded the operation deadline: {:?}",
            started.elapsed()
        );
    }

    #[cfg(windows)]
    #[test]
    fn bounded_stderr_is_reported_without_leaving_the_managed_process_running() {
        let _guard = process_test_guard();
        let fixture = ProcessFixture::new();
        let repository = test_repository(
            fixture.root.path(),
            GitLimits {
                timeout: Duration::from_secs(5),
                max_stdout_bytes: 128,
                max_stderr_bytes: 128,
            },
        );

        let error = repository
            .run_test_process(
                Path::new("powershell.exe"),
                &fixture.error_arguments(),
                GitExecutionPolicy::ReadOnly,
            )
            .expect_err("the helper must exceed the stderr bound");
        assert!(
            matches!(
                error,
                GitError::OutputLimitExceeded {
                    stream: "stderr",
                    ..
                }
            ),
            "unexpected bounded stderr error: {error:?}"
        );
    }

    #[cfg(windows)]
    #[test]
    fn simultaneous_stdout_and_stderr_overflow_has_deterministic_precedence() {
        let _guard = process_test_guard();
        let fixture = ProcessFixture::new();
        let repository = test_repository(
            fixture.root.path(),
            GitLimits {
                timeout: Duration::from_secs(5),
                max_stdout_bytes: 128,
                max_stderr_bytes: 128,
            },
        );

        let error = repository
            .run_test_process(
                Path::new("powershell.exe"),
                &fixture.both_arguments(),
                GitExecutionPolicy::ReadOnly,
            )
            .expect_err("both output streams must exceed their bounds");
        assert!(matches!(
            error,
            GitError::OutputLimitExceeded {
                stream: "stdout",
                ..
            }
        ));
    }

    #[cfg(windows)]
    #[test]
    fn grandchild_pipe_hold_is_cancelled_and_reader_workers_are_joined() {
        use crate::services::platform_service::is_pid_running;

        let _guard = process_test_guard();
        let fixture = ProcessFixture::new();
        let repository = test_repository(
            fixture.root.path(),
            GitLimits {
                timeout: Duration::from_secs(5),
                max_stdout_bytes: 64 * 1024,
                max_stderr_bytes: 64 * 1024,
            },
        );
        let started = Instant::now();
        let error = repository
            .run_test_process(
                Path::new("powershell.exe"),
                &fixture.pipe_hold_arguments(),
                GitExecutionPolicy::ReadOnly,
            )
            .expect_err("a grandchild-held pipe must not detach the readers");
        if let GitError::InvalidRepositoryRoot { reason, .. } = &error {
            panic!("pipe-hold failed graph validation: {reason}");
        }
        assert!(
            matches!(
                error,
                GitError::TimedOut { .. } | GitError::CleanupFailed { .. }
            ),
            "unexpected pipe-hold error: {error:?}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(8),
            "pipe-hold cleanup exceeded the single deadline: {:?}",
            started.elapsed()
        );
        let child_pid = fixture.wait_for_child_pid();
        let check_started = Instant::now();
        while is_pid_running(child_pid) && check_started.elapsed() < Duration::from_secs(2) {
            thread::sleep(Duration::from_millis(10));
        }
        assert!(
            !is_pid_running(child_pid),
            "grandchild holding the output pipe survived Job cleanup"
        );
    }

    #[cfg(unix)]
    #[test]
    fn cancellation_race_terminates_the_owned_process_group() {
        let _guard = process_test_guard();
        let fixture = tempfile::tempdir().expect("create cancellation fixture");
        let pid_file = fixture.path().join("shell.pid");
        let cancellation = GitCancellation::new();
        let repository = test_repository_with_cancellation(
            fixture.path(),
            GitLimits {
                timeout: Duration::from_secs(10),
                max_stdout_bytes: 64 * 1024,
                max_stderr_bytes: 64 * 1024,
            },
            cancellation.clone(),
        );
        let arguments = vec![
            OsString::from("-c"),
            OsString::from("echo $$ > \"$1\"; sleep 30"),
            OsString::from("devmanager-git-process-test"),
            pid_file.clone().into_os_string(),
        ];
        let handle = thread::spawn(move || {
            repository.run_test_process(Path::new("sh"), &arguments, GitExecutionPolicy::ReadOnly)
        });
        let shell_pid = wait_for_pid_file(&pid_file);
        cancellation.cancel();
        let error = handle
            .join()
            .expect("cancellation runner thread must not panic")
            .expect_err("cancellation must stop the process");
        assert!(matches!(
            error,
            GitError::Cancelled { .. } | GitError::CleanupFailed { .. }
        ));
        let started = Instant::now();
        while unix_pid_exists(shell_pid) && started.elapsed() < Duration::from_secs(2) {
            thread::sleep(Duration::from_millis(10));
        }
        assert!(
            !unix_pid_exists(shell_pid),
            "cancelled shell survived cleanup"
        );
    }

    #[cfg(windows)]
    #[test]
    fn cancellation_race_terminates_the_managed_job() {
        use crate::services::platform_service::is_pid_running;

        let _guard = process_test_guard();
        let fixture = ProcessFixture::new();
        let cancellation = GitCancellation::new();
        let repository = test_repository_with_cancellation(
            fixture.root.path(),
            GitLimits {
                timeout: Duration::from_secs(10),
                max_stdout_bytes: 64 * 1024,
                max_stderr_bytes: 64 * 1024,
            },
            cancellation.clone(),
        );
        let arguments = fixture.timeout_arguments();
        let handle = thread::spawn(move || {
            repository.run_test_process(
                Path::new("powershell.exe"),
                &arguments,
                GitExecutionPolicy::ReadOnly,
            )
        });
        let child_pid = fixture.wait_for_child_pid();
        cancellation.cancel();
        let error = handle
            .join()
            .expect("cancellation runner thread must not panic")
            .expect_err("cancellation must stop the process");
        assert!(matches!(
            error,
            GitError::Cancelled { .. } | GitError::CleanupFailed { .. }
        ));
        let started = Instant::now();
        while is_pid_running(child_pid) && started.elapsed() < Duration::from_secs(2) {
            thread::sleep(Duration::from_millis(10));
        }
        assert!(
            !is_pid_running(child_pid),
            "cancelled Job descendant survived cleanup"
        );
    }

    #[cfg(windows)]
    struct ProcessFixture {
        root: tempfile::TempDir,
        script: PathBuf,
        child_pid_file: PathBuf,
    }

    #[cfg(windows)]
    impl ProcessFixture {
        fn new() -> Self {
            let temp_root = Path::new(r"C:\Temp");
            fs::create_dir_all(temp_root).expect("create bounded test root");
            let root = tempfile::Builder::new()
                .prefix("devmanager-phase66-")
                .tempdir_in(temp_root)
                .expect("create bounded test fixture");
            let script = root.path().join("runner.ps1");
            let child_pid_file = root.path().join("child.pid");
            fs::write(
                &script,
                r#"param([string]$ChildPidFile, [switch]$OutputOnly, [switch]$ErrorOnly, [switch]$Both, [switch]$PipeHold)
if ($OutputOnly) {
    [Console]::Out.Write(('x' * 1024))
    exit 0
}
if ($ErrorOnly) {
    [Console]::Error.Write(('e' * 1024))
    exit 0
}
if ($Both) {
    [Console]::Out.Write(('x' * 1024))
    [Console]::Error.Write(('e' * 1024))
    exit 0
}
if ($PipeHold) {
    $startInfo = [Diagnostics.ProcessStartInfo]::new()
    $startInfo.FileName = 'cmd.exe'
    $startInfo.Arguments = '/c ping.exe -n 30 127.0.0.1'
    $startInfo.UseShellExecute = $false
    $startInfo.CreateNoWindow = $true
    $child = [Diagnostics.Process]::Start($startInfo)
    [IO.File]::WriteAllText($ChildPidFile, [string]$child.Id)
    exit 0
}
$child = Start-Process -WindowStyle Hidden -FilePath cmd.exe -ArgumentList @('/c','ping.exe -n 30 127.0.0.1 > nul') -PassThru
[IO.File]::WriteAllText($ChildPidFile, [string]$child.Id)
[Console]::Out.WriteLine('parent')
[Console]::Error.WriteLine('parent-error')
Start-Sleep -Seconds 10
"#,
            )
            .expect("write bounded test helper");
            Self {
                root,
                script,
                child_pid_file,
            }
        }

        fn timeout_arguments(&self) -> Vec<OsString> {
            vec![
                OsString::from("-NoProfile"),
                OsString::from("-NonInteractive"),
                OsString::from("-ExecutionPolicy"),
                OsString::from("Bypass"),
                OsString::from("-File"),
                self.script.clone().into_os_string(),
                OsString::from("-ChildPidFile"),
                self.child_pid_file.clone().into_os_string(),
            ]
        }

        fn output_arguments(&self) -> Vec<OsString> {
            vec![
                OsString::from("-NoProfile"),
                OsString::from("-NonInteractive"),
                OsString::from("-ExecutionPolicy"),
                OsString::from("Bypass"),
                OsString::from("-File"),
                self.script.clone().into_os_string(),
                OsString::from("-OutputOnly"),
            ]
        }

        fn error_arguments(&self) -> Vec<OsString> {
            vec![
                OsString::from("-NoProfile"),
                OsString::from("-NonInteractive"),
                OsString::from("-ExecutionPolicy"),
                OsString::from("Bypass"),
                OsString::from("-File"),
                self.script.clone().into_os_string(),
                OsString::from("-ErrorOnly"),
            ]
        }

        fn both_arguments(&self) -> Vec<OsString> {
            vec![
                OsString::from("-NoProfile"),
                OsString::from("-NonInteractive"),
                OsString::from("-ExecutionPolicy"),
                OsString::from("Bypass"),
                OsString::from("-File"),
                self.script.clone().into_os_string(),
                OsString::from("-Both"),
            ]
        }

        fn pipe_hold_arguments(&self) -> Vec<OsString> {
            vec![
                OsString::from("-NoProfile"),
                OsString::from("-NonInteractive"),
                OsString::from("-ExecutionPolicy"),
                OsString::from("Bypass"),
                OsString::from("-File"),
                self.script.clone().into_os_string(),
                OsString::from("-ChildPidFile"),
                self.child_pid_file.clone().into_os_string(),
                OsString::from("-PipeHold"),
            ]
        }

        fn wait_for_child_pid(&self) -> u32 {
            let started = Instant::now();
            while started.elapsed() < Duration::from_secs(5) {
                if let Ok(value) = fs::read_to_string(&self.child_pid_file) {
                    if let Ok(pid) = value.trim().parse() {
                        return pid;
                    }
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            panic!("managed helper did not record its child PID");
        }
    }

    #[cfg(any(unix, windows))]
    fn test_repository(root: &Path, limits: GitLimits) -> GitRepository {
        test_repository_with_cancellation(root, limits, GitCancellation::new())
    }

    #[cfg(any(unix, windows))]
    fn test_repository_with_cancellation(
        root: &Path,
        limits: GitLimits,
        cancellation: GitCancellation,
    ) -> GitRepository {
        // Process-lifecycle fixtures execute a shell/cmd helper rather than
        // Git itself, but the production repository holder now requires the
        // same minimal graph before it will retain a root handle.
        let git_dir = root.join(".git");
        fs::create_dir_all(git_dir.join("objects")).expect("create fixture Git objects");
        fs::write(git_dir.join("config"), b"").expect("create fixture Git config");
        let root = RepositoryRoot::open(root).expect("hold canonical fixture root");
        let limits = limits.bounded();
        GitRepository {
            workspace: WorkspaceIdentity::from_canonical_root(root.path.clone()),
            root,
            limits: limits.clone(),
            cancellation,
            authority: GitRepositoryAuthority::Test,
            read_permit: Arc::new(Mutex::new(Some(
                GitOperationPermit::test_read_with_timeout(limits.timeout),
            ))),
        }
    }

    #[test]
    fn host_binding_retains_live_authority_and_graph_handles() {
        let root = tempfile::tempdir().expect("create authority fixture");
        fs::create_dir_all(root.path().join(".git").join("objects")).expect("create Git objects");
        fs::write(
            root.path().join(".git").join("config"),
            b"[core]\n\trepositoryformatversion = 0\n",
        )
        .expect("create Git config");
        let binding = test_issue_git_host_binding(root.path(), Vec::new())
            .expect("issue test-only authority");

        assert!(binding.has_live_authority_for_test());
        assert!(binding.retained_handle_count_for_test() >= 2);
        let repository = GitRepository::from_host_binding(binding, GitCancellation::new())
            .expect("open bound repository");
        assert!(repository.has_live_authority_for_test());
    }

    #[test]
    fn host_binding_rejects_a_stale_action_generation() {
        let root = tempfile::tempdir().expect("create generation fixture");
        fs::create_dir_all(root.path().join(".git").join("objects")).expect("create Git objects");
        fs::write(
            root.path().join(".git").join("config"),
            b"[core]\n\trepositoryformatversion = 0\n",
        )
        .expect("create Git config");
        let binding = test_issue_git_host_binding(root.path(), Vec::new())
            .expect("issue test-only authority");
        binding
            .capability
            .action_generation
            .current
            .store(2, Ordering::Release);

        assert!(!binding.has_live_authority_for_test());
        assert!(matches!(
            GitRepository::from_host_binding(binding, GitCancellation::new()),
            Err(GitError::InvalidRepositoryRoot { .. })
        ));
    }

    #[test]
    fn host_authority_validation_honors_the_operation_deadline() {
        let root = tempfile::tempdir().expect("create deadline authority fixture");
        fs::create_dir_all(root.path().join(".git").join("objects")).expect("create Git objects");
        fs::write(root.path().join(".git").join("config"), b"").expect("create Git config");
        let binding = test_issue_git_host_binding(root.path(), Vec::new())
            .expect("issue test-only authority");
        let repository = GitRepository::from_host_binding(binding, GitCancellation::new())
            .expect("open bound repository");

        assert!(
            repository
                .validate_host_authority(false, OperationDeadline::from_now(Duration::ZERO))
                .is_err(),
            "authority graph identity checks must not run past an expired operation budget"
        );
    }

    #[test]
    fn missing_host_binding_stays_explicitly_unavailable() {
        let error = GitRepository::from_optional_host_binding(None, GitCancellation::new())
            .expect_err("production must not mint a path-only Git authority");
        assert!(matches!(error, GitError::AuthorityUnavailable));
    }

    #[cfg(windows)]
    #[test]
    fn path_containment_uses_exact_os_units_not_lossy_display() {
        let left = PathBuf::from(OsString::from_wide(&[
            0x0043, 0x003A, 0x005C, 0xD800, 0x0061,
        ]));
        let right = PathBuf::from(OsString::from_wide(&[
            0x0043, 0x003A, 0x005C, 0xD801, 0x0061,
        ]));
        assert_eq!(
            left.to_string_lossy(),
            right.to_string_lossy(),
            "fixture must collide under lossy display conversion"
        );
        assert!(
            !same_path(&left, &right),
            "distinct non-Unicode path units must not compare equal"
        );
        assert!(
            !is_within(&left, &right),
            "distinct non-Unicode path units must not satisfy containment"
        );
    }

    #[test]
    fn repository_graph_checks_deadline_before_canonicalization() {
        let root = tempfile::tempdir().expect("create deadline fixture");
        let missing = root.path().join("missing-repository");
        let error = match RepositoryGraph::open_with_deadline(
            &missing,
            &[],
            OperationDeadline::from_now(Duration::ZERO),
        ) {
            Ok(_) => panic!("an expired graph admission deadline must fail closed"),
            Err(error) => error,
        };
        assert!(error.contains("deadline"), "{error}");
    }

    #[test]
    fn approved_graph_roots_are_bounded_before_canonicalization() {
        let root = tempfile::tempdir().expect("create approved-root cap fixture");
        let approved = vec![PathBuf::new(); HARD_MAX_APPROVED_GRAPH_ROOTS + 1];
        let error = canonicalize_approved_graph_roots_with_deadline(
            root.path(),
            &approved,
            OperationDeadline::from_now(HARD_MAX_TIMEOUT),
        )
        .expect_err("approved graph roots must have a hard admission cap");
        assert!(error.contains("limit"), "{error}");
    }

    #[test]
    fn repository_graph_rejects_pack_fanout_above_the_hard_cap() {
        let root = tempfile::tempdir().expect("create graph fixture");
        let pack_dir = root.path().join(".git").join("objects").join("pack");
        fs::create_dir_all(&pack_dir).expect("create pack directory");
        fs::write(
            root.path().join(".git").join("config"),
            b"[core]\n\trepositoryformatversion = 0\n",
        )
        .expect("create Git config");
        for index in 0..=HARD_MAX_PACK_FILES {
            fs::write(
                pack_dir.join(format!("pack-{index:08x}.idx")),
                b"pack-index",
            )
            .expect("write pack fixture");
        }
        let error = match RepositoryRoot::open(root.path()) {
            Ok(_) => panic!("pack fanout must be rejected before any child"),
            Err(error) => error,
        };
        assert!(error.contains("limit"), "{error}");
    }

    #[cfg(unix)]
    fn wait_for_pid_file(path: &Path) -> u32 {
        let started = Instant::now();
        while started.elapsed() < Duration::from_secs(2) {
            if let Ok(value) = fs::read_to_string(path) {
                if let Ok(pid) = value.trim().parse() {
                    return pid;
                }
            }
            thread::sleep(Duration::from_millis(10));
        }
        panic!("process-group fixture did not record its child PID");
    }

    #[cfg(unix)]
    fn unix_pid_exists(pid: u32) -> bool {
        let pid = pid.to_string();
        std::process::Command::new("kill")
            .args(["-0", "--", pid.as_str()])
            .status()
            .is_ok_and(|status| status.success())
    }

    #[cfg(any(unix, windows))]
    fn process_test_guard() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}
