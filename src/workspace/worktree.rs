//! Host-owned Git worktree orchestration.
//!
//! This module deliberately contains the operation state machine, but not a
//! Git process launcher.  A process-owned executor and a durable journal are
//! injected through the sealed crate-local seams below.  Until the accepted
//! Task 3 executor is joined to this seam, [`WorktreeService::new`] fails
//! closed.

#[cfg(test)]
use std::collections::BTreeMap;
use std::fmt;
use std::fs::File;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
#[cfg(test)]
use std::sync::Barrier;
#[cfg(test)]
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use rusqlite::{OptionalExtension, TransactionBehavior};

use sha2::{Digest, Sha256};

#[cfg(test)]
use crate::domain::id::ResourceId;
#[cfg(test)]
use crate::domain::operation::ResourceFence;
use crate::domain::task::WorkspaceRef;
use crate::domain::{ClientId, CommandId, ProjectId, RequestId, TaskId};
use crate::process::identity::ProcessOwner;
#[cfg(test)]
use crate::process::identity::{ManagedProcessId, ManagedProcessIdentity};
use crate::process::registry::ManagedProcessFence as RegistryManagedProcessFence;

use super::model::{is_within, path_identity_key, WorkspaceBinding};
use super::service::{WorkspaceAuthorization, WorkspacePinnedPath, WorkspaceResourceLease};
use super::WorkspaceResource;

/// Maximum bytes accepted from one Git worktree porcelain response.
pub const MAX_PORCELAIN_BYTES: usize = 64 * 1024;

/// Maximum number of durable operation identities retained by one service.
pub const MAX_JOURNAL_OPERATIONS: usize = 256;

const MAX_PORCELAIN_RECORDS: usize = 256;
const MAX_LABEL_BYTES: usize = 96;
const MAX_BRANCH_BYTES: usize = 128;
const MAX_BASE_REVISION_BYTES: usize = 128;
const MAX_TARGET_PATH_BYTES: usize = 32 * 1024;
const MAX_COLLISION_ATTEMPTS: usize = 64;
const MAX_OPERATION_TIMEOUT: Duration = Duration::from_secs(30);

const ZERO_FINGERPRINT: [u8; 32] = [0; 32];

/// Cancellation owned by the command that admitted a worktree operation.
#[derive(Clone, Default)]
pub struct CancellationToken(Arc<AtomicBool>);

impl fmt::Debug for CancellationToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("CancellationToken(REDACTED)")
    }
}

impl CancellationToken {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        self.0.store(true, Ordering::Release);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

/// An exact, host-approved destination for one linked worktree.
///
/// The path is carried only inside the sealed workspace/Git state machine. A
/// production value is issued from a retained WorkspaceService pin; callers
/// cannot construct one from a display path. The durable journal stores the
/// exact path and the approved-root identity so recovery cannot substitute a
/// sibling or a reparse-point escape.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct WorktreeTarget {
    approved_root: PathBuf,
    path: PathBuf,
    approved_root_identity: [u8; 32],
}

impl fmt::Debug for WorktreeTarget {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("WorktreeTarget(REDACTED)")
    }
}

impl WorktreeTarget {
    /// Sealed host seam: only a retained WorkspaceService filesystem pin can
    /// issue a production destination. The target path may not exist yet,
    /// but it must be an exact lexical child of the approved root.
    pub(crate) fn from_retained_root(
        root: &RetainedWorktreeHandle,
        path: PathBuf,
    ) -> Result<Self, WorktreeError> {
        let target = Self {
            approved_root: root.path.clone(),
            path,
            approved_root_identity: root.identity,
        };
        if root.is_live()
            && root
                .handle
                .metadata()
                .is_ok_and(|metadata| metadata.is_dir())
            && target.validate()
        {
            Ok(target)
        } else {
            Err(WorktreeError::Hold(
                WorktreeHold::Task62AuthorityUnavailable,
            ))
        }
    }

    fn placeholder() -> Self {
        Self {
            approved_root: PathBuf::new(),
            path: PathBuf::new(),
            approved_root_identity: ZERO_FINGERPRINT,
        }
    }

    #[cfg(test)]
    pub(crate) fn for_test(root: impl Into<PathBuf>, path: impl Into<PathBuf>) -> Self {
        let approved_root = root.into();
        let path = path.into();
        let approved_root_identity = digest_with_tag(
            b"test-approved-worktree-root-v1",
            approved_root.to_string_lossy().as_bytes(),
        );
        Self {
            approved_root,
            path,
            approved_root_identity,
        }
    }

    fn validate(&self) -> bool {
        self.approved_root.is_absolute()
            && self.path.is_absolute()
            && self.approved_root_identity != ZERO_FINGERPRINT
            && self.approved_root.as_os_str().len() <= MAX_TARGET_PATH_BYTES
            && self.path.as_os_str().len() <= MAX_TARGET_PATH_BYTES
            && !self
                .approved_root
                .components()
                .chain(self.path.components())
                .any(|component| matches!(component, Component::ParentDir))
            && is_within(&self.approved_root, &self.path)
            && path_identity_key(&self.approved_root) != path_identity_key(&self.path)
    }

    fn same_as(&self, other: &Self) -> bool {
        self == other
    }
}

/// A bounded request to create one linked Git worktree.
#[derive(Clone)]
pub struct CreateWorktreeRequest {
    label: String,
    branch: Option<String>,
    idempotency_key: [u8; 16],
    target: Option<WorktreeTarget>,
    task_id: Option<TaskId>,
    workspace: Option<WorkspaceIdentity>,
    cancellation: CancellationToken,
    timeout: Duration,
}

impl fmt::Debug for CreateWorktreeRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("CreateWorktreeRequest(REDACTED)")
    }
}

impl CreateWorktreeRequest {
    pub fn new(label: impl Into<String>) -> Self {
        let idempotency_key = *uuid::Uuid::new_v4().as_bytes();
        Self {
            label: label.into(),
            branch: None,
            idempotency_key,
            target: None,
            task_id: None,
            workspace: None,
            cancellation: CancellationToken::new(),
            timeout: MAX_OPERATION_TIMEOUT,
        }
    }

    pub fn with_branch(mut self, branch: impl Into<String>) -> Self {
        self.branch = Some(branch.into());
        self
    }

    pub fn with_idempotency_key(mut self, key: [u8; 16]) -> Self {
        self.idempotency_key = key;
        self
    }

    /// Sealed integration seam used only after WorkspaceService has retained
    /// and revalidated the exact root pin and task/workspace identity.
    pub(crate) fn with_authorized_target(mut self, target: WorktreeTarget) -> Self {
        self.target = Some(target);
        self
    }

    /// Bind the request to the exact Task6.2 identity that admitted it.
    pub(crate) fn with_workspace_identity(
        mut self,
        task_id: TaskId,
        workspace: WorkspaceIdentity,
    ) -> Self {
        self.task_id = Some(task_id);
        self.workspace = Some(workspace);
        self
    }

    #[cfg(test)]
    fn with_test_target(mut self, target: WorktreeTarget) -> Self {
        self.target = Some(target);
        self
    }

    pub fn with_cancellation(mut self, cancellation: CancellationToken) -> Self {
        self.cancellation = cancellation;
        self
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout.min(MAX_OPERATION_TIMEOUT);
        self
    }
}

/// A typed confirmation for destructive cleanup.  It carries no path or
/// command string and cannot be manufactured from a display path.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum CleanupConfirmation {
    None,
    Confirmed,
    Force,
}

impl fmt::Debug for CleanupConfirmation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("CleanupConfirmation(REDACTED)")
    }
}

impl CleanupConfirmation {
    pub fn none() -> Self {
        Self::None
    }

    pub fn confirmed() -> Self {
        Self::Confirmed
    }

    pub fn force() -> Self {
        Self::Force
    }
}

/// An opaque identity captured by WorkspaceService and carried to the Git
/// executor.  The raw project/worktree paths are intentionally absent.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct WorkspaceIdentity(pub(crate) [u8; 32]);

impl fmt::Debug for WorkspaceIdentity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("WorkspaceIdentity(REDACTED)")
    }
}

/// Context that binds the operation to the current command, client, lease,
/// action epoch, and runtime generation.  The workspace reference is checked
/// against the live WorkspaceAuthorization; it is not path authority by
/// itself.
#[derive(Clone)]
pub struct WorkspaceActionContext {
    task_id: TaskId,
    project_id: ProjectId,
    client_id: ClientId,
    connection_id: uuid::Uuid,
    request_id: RequestId,
    command_id: CommandId,
    workspace: WorkspaceRef,
    action_epoch: u64,
    runtime_generation: u64,
}

impl fmt::Debug for WorkspaceActionContext {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("WorkspaceActionContext(REDACTED)")
    }
}

impl WorkspaceActionContext {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        task_id: TaskId,
        project_id: ProjectId,
        client_id: ClientId,
        connection_id: uuid::Uuid,
        request_id: RequestId,
        command_id: CommandId,
        workspace: WorkspaceRef,
        action_epoch: u64,
        runtime_generation: u64,
    ) -> Self {
        Self {
            task_id,
            project_id,
            client_id,
            connection_id,
            request_id,
            command_id,
            workspace,
            action_epoch,
            runtime_generation,
        }
    }
}

/// The only public success value returned by creation. The exact approved
/// target and base revision remain durable, sealed receipt state; subsequent
/// cleanup uses this receipt rather than re-resolving a display path.
#[derive(Clone, PartialEq, Eq)]
pub struct CreatedWorktree {
    pub(crate) operation_id: OperationKey,
    pub(crate) scope: WorkspaceScope,
    pub(crate) branch: String,
    pub(crate) base_commit: [u8; 32],
    pub(crate) base_revision: String,
    pub(crate) target: WorktreeTarget,
    pub(crate) identity: WorkspaceIdentity,
    pub(crate) linked: LinkedWorktreeIdentity,
}

impl fmt::Debug for CreatedWorktree {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("CreatedWorktree(REDACTED)")
    }
}

impl CreatedWorktree {
    pub fn branch(&self) -> &str {
        &self.branch
    }
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct OperationKey(pub(crate) [u8; 16]);

impl fmt::Debug for OperationKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("OperationKey(REDACTED)")
    }
}

/// Ephemeral ownership lease for one invocation of an operation.  It is
/// persisted with the intent and reservation so a duplicate caller cannot
/// settle, release, or recover the winner's in-flight work.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct OperationOwner(pub(crate) [u8; 16]);

impl fmt::Debug for OperationOwner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("OperationOwner(REDACTED)")
    }
}

impl OperationOwner {
    fn fresh() -> Self {
        let mut owner = [0; 16];
        // This token must remain unique across processes and restarts; a
        // process-local counter could collide with a persisted owner and let
        // an unrelated caller release another process's reservation.
        owner.copy_from_slice(uuid::Uuid::new_v4().as_bytes());
        Self(owner)
    }
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct LinkedWorktreeIdentity {
    pub(crate) gitdir: [u8; 32],
    pub(crate) commondir: [u8; 32],
    pub(crate) backreference: [u8; 32],
    pub(crate) repository: [u8; 32],
}

impl fmt::Debug for LinkedWorktreeIdentity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("LinkedWorktreeIdentity(REDACTED)")
    }
}

/// The immutable, pathless scope carried by every journal record and receipt.
///
/// This is deliberately derived only after WorkspaceService has revalidated
/// its private path pins.  It is not a path authority and cannot be
/// constructed by a production caller.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct WorkspaceScope {
    pub(crate) task_id: TaskId,
    pub(crate) project_id: ProjectId,
    pub(crate) workspace: WorkspaceIdentity,
    pub(crate) client_id: ClientId,
    pub(crate) connection_id: uuid::Uuid,
    pub(crate) request_id: RequestId,
    pub(crate) command_id: CommandId,
    pub(crate) action_epoch: u64,
    pub(crate) runtime_generation: u64,
    pub(crate) process_epoch: u64,
    pub(crate) root: [u8; 32],
    pub(crate) repository: [u8; 32],
    pub(crate) process_owner: [u8; 16],
    /// Identity of the retained, non-clone execution lease.  A zero value is
    /// only present before the host adapter binds the workspace to a lease;
    /// durable records must never be admitted with it.
    pub(crate) execution_lease: [u8; 16],
    pub(crate) linked: LinkedWorktreeIdentity,
}

/// A host-validated durable store.  Production code can only obtain this
/// value from the future Config/Workspace union; there is intentionally no
/// public path constructor.  Tests use the explicitly cfg(test) fixture
/// constructor below and still exercise the production SQLite implementation.
#[derive(Clone)]
pub(crate) struct WorktreeJournalStore {
    path: Arc<PathBuf>,
    handle: Arc<File>,
    identity: [u8; 32],
    handle_identity: [u8; 32],
}

impl fmt::Debug for WorktreeJournalStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("WorktreeJournalStore(REDACTED)")
    }
}

impl WorktreeJournalStore {
    /// Host-only seam: the caller must supply a WorkspaceService-retained
    /// file pin and the opaque durable-store identity. A raw path/handle
    /// tuple is never a production authority.
    pub(crate) fn from_workspace_pin(
        pin: &WorkspacePinnedPath,
        identity: [u8; 32],
    ) -> Result<Self, JournalError> {
        if pin.path().as_os_str().is_empty()
            || !pin.path().is_absolute()
            || pin.is_dir()
            || pin.identity().is_empty()
            || identity == [0; 32]
        {
            return Err(JournalError::InvalidStore);
        }
        let handle = pin.handle();
        let metadata = handle.metadata().map_err(|_| JournalError::InvalidStore)?;
        if !metadata.is_file() {
            return Err(JournalError::InvalidStore);
        }
        let handle_identity = retained_file_identity(&handle).ok_or(JournalError::InvalidStore)?;
        Ok(Self {
            path: Arc::new(pin.path().to_path_buf()),
            handle,
            identity,
            handle_identity,
        })
    }

    /// Test-only adversarial seam: exercise the swapped-path rejection while
    /// keeping the arbitrary tuple unavailable to production code.
    #[cfg(test)]
    pub(crate) fn from_validated(
        path: PathBuf,
        handle: Arc<File>,
        identity: [u8; 32],
    ) -> Result<Self, JournalError> {
        if !path.is_absolute() || path.as_os_str().is_empty() || identity == [0; 32] {
            return Err(JournalError::InvalidStore);
        }
        let metadata = handle.metadata().map_err(|_| JournalError::InvalidStore)?;
        if !metadata.is_file() {
            return Err(JournalError::InvalidStore);
        }
        let handle_identity = retained_file_identity(&handle).ok_or(JournalError::InvalidStore)?;
        Ok(Self {
            path: Arc::new(path),
            handle,
            identity,
            handle_identity,
        })
    }

    #[cfg(test)]
    pub(crate) fn for_test(path: &Path) -> Result<Self, JournalError> {
        let path = path
            .canonicalize()
            .ok()
            .or_else(|| {
                path.parent()
                    .and_then(|parent| parent.canonicalize().ok())
                    .map(|parent| parent.join(path.file_name().unwrap_or_default()))
            })
            .ok_or(JournalError::InvalidStore)?;
        let identity = digest_with_tag(
            b"test-worktree-journal-store-v1",
            path.to_string_lossy().as_bytes(),
        );
        let handle = Arc::new(
            std::fs::OpenOptions::new()
                .create(true)
                .read(true)
                .write(true)
                .open(&path)
                .map_err(|_| JournalError::InvalidStore)?,
        );
        Self::from_validated(path, handle, identity)
    }
}

/// Derive the identity from the retained descriptor, not from displayable
/// path text.  Windows uses the volume/file-index pair and Unix uses the
/// device/inode pair; both survive a path replacement and let the SQLite
/// adapter reject a reopened path that no longer names the admitted file.
pub(crate) fn retained_file_identity(file: &File) -> Option<[u8; 32]> {
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawHandle;
        use windows::Win32::Foundation::HANDLE;
        use windows::Win32::Storage::FileSystem::{
            GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION,
        };

        let mut information = BY_HANDLE_FILE_INFORMATION::default();
        unsafe { GetFileInformationByHandle(HANDLE(file.as_raw_handle()), &mut information) }
            .ok()?;
        let file_index =
            (u64::from(information.nFileIndexHigh) << 32) | u64::from(information.nFileIndexLow);
        let mut hasher = Sha256::new();
        hasher.update(b"devmanager-retained-file-v1\0");
        hasher.update(information.dwVolumeSerialNumber.to_le_bytes());
        hasher.update(file_index.to_le_bytes());
        return Some(hasher.finalize().into());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let metadata = file.metadata().ok()?;
        let mut hasher = Sha256::new();
        hasher.update(b"devmanager-retained-file-v1\0");
        hasher.update(metadata.dev().to_le_bytes());
        hasher.update(metadata.ino().to_le_bytes());
        return Some(hasher.finalize().into());
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = file;
        None
    }
}

pub(crate) fn retained_path_identity(path: &Path) -> Option<[u8; 32]> {
    let metadata = std::fs::symlink_metadata(path).ok()?;
    if metadata.file_type().is_symlink() {
        return None;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        use windows::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;
        if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0 {
            return None;
        }
    }
    let handle = open_retained_path(path)?;
    retained_file_identity(&handle)
}

fn open_retained_path(path: &Path) -> Option<File> {
    #[cfg(windows)]
    {
        use std::fs::OpenOptions;
        use std::os::windows::fs::OpenOptionsExt;
        use windows::Win32::Storage::FileSystem::{
            FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_DELETE,
            FILE_SHARE_READ, FILE_SHARE_WRITE,
        };
        return OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ.0 | FILE_SHARE_WRITE.0 | FILE_SHARE_DELETE.0)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS.0 | FILE_FLAG_OPEN_REPARSE_POINT.0)
            .open(path)
            .ok();
    }
    #[cfg(unix)]
    {
        use std::fs::OpenOptions;
        use std::os::unix::fs::OpenOptionsExt;
        #[cfg(any(target_os = "linux", target_os = "android"))]
        const NOFOLLOW: i32 = 0x20000;
        #[cfg(any(
            target_os = "macos",
            target_os = "ios",
            target_os = "freebsd",
            target_os = "openbsd",
            target_os = "netbsd"
        ))]
        const NOFOLLOW: i32 = 0x100;
        #[cfg(not(any(
            target_os = "linux",
            target_os = "android",
            target_os = "macos",
            target_os = "ios",
            target_os = "freebsd",
            target_os = "openbsd",
            target_os = "netbsd"
        )))]
        const NOFOLLOW: i32 = 0;
        return OpenOptions::new()
            .read(true)
            .custom_flags(NOFOLLOW)
            .open(path)
            .ok();
    }
    #[cfg(not(any(unix, windows)))]
    {
        File::open(path).ok()
    }
}

/// A retained filesystem identity.  The handle is held for the full child
/// lifetime; path text is an implementation detail and is never formatted or
/// transported as authority.
pub(crate) struct RetainedWorktreeHandle {
    path: PathBuf,
    handle: Arc<File>,
    identity: [u8; 32],
}

impl fmt::Debug for RetainedWorktreeHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("RetainedWorktreeHandle(REDACTED)")
    }
}

impl RetainedWorktreeHandle {
    /// The only production constructor takes a WorkspaceService-issued pin
    /// token and computes the identity from its retained handle itself. A raw
    /// `(path, handle, identity)` tuple is intentionally not accepted.
    pub(crate) fn from_workspace_pin(pin: &WorkspacePinnedPath) -> Result<Self, WorktreeError> {
        let path = pin.path().to_path_buf();
        let handle = pin.handle();
        let identity = retained_file_identity(&handle).ok_or(WorktreeError::Hold(
            WorktreeHold::Task62AuthorityUnavailable,
        ))?;
        if !path.is_absolute() || pin.identity().is_empty() {
            return Err(WorktreeError::Hold(
                WorktreeHold::Task62AuthorityUnavailable,
            ));
        }
        Ok(Self {
            path,
            handle,
            identity,
        })
    }

    #[cfg(test)]
    fn for_test(label: &[u8]) -> Self {
        let path = std::env::current_exe().expect("test executable path");
        let handle = Arc::new(File::open(&path).expect("test executable handle"));
        let identity = retained_file_identity(&handle).expect("test executable identity");
        let _ = label;
        Self {
            path,
            handle,
            identity,
        }
    }

    fn is_live(&self) -> bool {
        self.path.is_absolute()
            && self.identity != [0; 32]
            && self.handle.metadata().is_ok()
            && retained_path_identity(&self.path) == Some(self.identity)
    }
}

/// Opaque live endpoint lease retained by the process-owned Git child.
pub(crate) struct WorktreeEndpointLease {
    handle: Arc<File>,
    identity: [u8; 32],
}

impl fmt::Debug for WorktreeEndpointLease {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("WorktreeEndpointLease(REDACTED)")
    }
}

impl WorktreeEndpointLease {
    pub(crate) fn from_workspace_pin(pin: &WorkspacePinnedPath) -> Result<Self, WorktreeError> {
        let handle = pin.handle();
        let identity = retained_file_identity(&handle)
            .ok_or(WorktreeError::Hold(WorktreeHold::Task3AuthorityUnavailable))?;
        Ok(Self { handle, identity })
    }

    fn is_live(&self) -> bool {
        self.handle.metadata().is_ok() && self.identity != [0; 32]
    }
}

/// Crate-private controller/connection tokens.  Their constructors are
/// private so a scalar or path-only value cannot forge a production lease.
pub(crate) struct WorktreeControllerHandle {
    live: Arc<AtomicBool>,
    identity: [u8; 16],
}

impl WorktreeControllerHandle {
    pub(crate) fn from_host(live: Arc<AtomicBool>, identity: [u8; 16]) -> Self {
        Self { live, identity }
    }
}

pub(crate) struct WorktreeConnectionHandle {
    live: Arc<AtomicBool>,
    identity: [u8; 16],
}

impl WorktreeConnectionHandle {
    pub(crate) fn from_host(live: Arc<AtomicBool>, identity: [u8; 16]) -> Self {
        Self { live, identity }
    }
}

pub(crate) struct WorktreeExecutionLimits {
    pub(crate) max_nodes: usize,
    pub(crate) max_bytes: usize,
    pub(crate) max_output_bytes: usize,
}

impl Default for WorktreeExecutionLimits {
    fn default() -> Self {
        Self {
            max_nodes: 256,
            max_bytes: 256 * 1024,
            max_output_bytes: 256 * 1024,
        }
    }
}

enum WorktreeAuthority {
    Host {
        workspace: Arc<WorkspaceAuthorization>,
        resource: Arc<WorkspaceResourceLease>,
        controller: WorktreeControllerHandle,
        connection: WorktreeConnectionHandle,
    },
    #[cfg(test)]
    Test { live: Arc<AtomicBool> },
}

/// Non-Clone capability required by every process-owned worktree executor.
/// It retains the authority, resource lease, generation fence, approved
/// handles, endpoint, bounded budget, and future process owner instead of
/// copying scalar bindings into child requests.
pub(crate) struct WorktreeExecutionLease {
    authority: WorktreeAuthority,
    controller_generation: u64,
    runtime_generation: u64,
    root: RetainedWorktreeHandle,
    repository: RetainedWorktreeHandle,
    git_dir: RetainedWorktreeHandle,
    common_dir: Option<RetainedWorktreeHandle>,
    linked_worktree: RetainedWorktreeHandle,
    target: RetainedWorktreeHandle,
    endpoint: Option<WorktreeEndpointLease>,
    deadline: Instant,
    limits: WorktreeExecutionLimits,
    process_owner: Option<RegistryManagedProcessFence>,
    journal_store: Arc<WorktreeJournalStore>,
    identity: [u8; 16],
}

impl fmt::Debug for WorktreeExecutionLease {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("WorktreeExecutionLease(REDACTED)")
    }
}

impl WorktreeExecutionLease {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn from_host(
        workspace: Arc<WorkspaceAuthorization>,
        resource: Arc<WorkspaceResourceLease>,
        controller: WorktreeControllerHandle,
        connection: WorktreeConnectionHandle,
        root: RetainedWorktreeHandle,
        repository: RetainedWorktreeHandle,
        git_dir: RetainedWorktreeHandle,
        common_dir: Option<RetainedWorktreeHandle>,
        linked_worktree: RetainedWorktreeHandle,
        target: RetainedWorktreeHandle,
        endpoint: Option<WorktreeEndpointLease>,
        deadline: Instant,
        limits: WorktreeExecutionLimits,
        process_owner: Option<RegistryManagedProcessFence>,
        journal_store: Arc<WorktreeJournalStore>,
        identity: [u8; 16],
    ) -> Result<Self, WorktreeError> {
        if process_owner.is_none() {
            return Err(WorktreeError::Hold(WorktreeHold::ProcessOwnerUnavailable));
        }
        Ok(Self {
            controller_generation: workspace.action_epoch(),
            runtime_generation: workspace.runtime_generation(),
            authority: WorktreeAuthority::Host {
                workspace,
                resource,
                controller,
                connection,
            },
            root,
            repository,
            git_dir,
            common_dir,
            linked_worktree,
            target,
            endpoint,
            deadline,
            limits,
            process_owner,
            journal_store,
            identity,
        })
    }

    fn is_live(&self) -> bool {
        if self.identity == [0; 16] || Instant::now() >= self.deadline {
            return false;
        }
        match &self.authority {
            WorktreeAuthority::Host {
                workspace,
                resource,
                controller,
                connection,
                ..
            } => {
                self.process_owner.is_some()
                    && resource.ensure_active().is_ok()
                    && workspace.action_epoch() == self.controller_generation
                    && workspace.runtime_generation() == self.runtime_generation
                    && controller.live.load(Ordering::Acquire)
                    && controller.identity != [0; 16]
                    && connection.live.load(Ordering::Acquire)
                    && connection.identity != [0; 16]
                    && self.root.is_live()
                    && self.repository.is_live()
                    && self.git_dir.is_live()
                    && self
                        .common_dir
                        .as_ref()
                        .is_none_or(RetainedWorktreeHandle::is_live)
                    && self.linked_worktree.is_live()
                    && self.target.is_live()
                    && self
                        .endpoint
                        .as_ref()
                        .is_none_or(WorktreeEndpointLease::is_live)
            }
            #[cfg(test)]
            WorktreeAuthority::Test { live } => {
                live.load(Ordering::Acquire)
                    && self.root.is_live()
                    && self.repository.is_live()
                    && self.git_dir.is_live()
                    && self
                        .common_dir
                        .as_ref()
                        .is_none_or(RetainedWorktreeHandle::is_live)
                    && self.linked_worktree.is_live()
                    && self.target.is_live()
                    && self
                        .endpoint
                        .as_ref()
                        .is_none_or(WorktreeEndpointLease::is_live)
            }
        }
    }

    fn bound_budget(&self, budget: ExecutionBudget) -> ExecutionBudget {
        ExecutionBudget {
            deadline: budget.deadline.min(self.deadline),
            max_nodes: budget.max_nodes.min(self.limits.max_nodes),
            max_bytes: budget.max_bytes.min(self.limits.max_bytes),
            max_output_bytes: budget.max_output_bytes.min(self.limits.max_output_bytes),
        }
    }

    fn identity(&self) -> [u8; 16] {
        self.identity
    }

    fn target_is_bound(&self, target: &WorktreeTarget) -> bool {
        match &self.authority {
            WorktreeAuthority::Host { .. } => {
                target.approved_root_identity == self.root.identity
                    && self.root.is_live()
                    && target.validate()
            }
            #[cfg(test)]
            WorktreeAuthority::Test { .. } => target.validate(),
        }
    }

    #[cfg(test)]
    fn for_test() -> Self {
        let live = Arc::new(AtomicBool::new(true));
        let journal_path = std::env::current_exe().expect("test executable path");
        let journal_handle = Arc::new(File::open(&journal_path).expect("test journal handle"));
        let journal_store = WorktreeJournalStore::from_validated(
            journal_path,
            journal_handle,
            digest_with_tag(b"test-worktree-journal-store-v1", b"fixture"),
        )
        .expect("test journal store");
        Self {
            authority: WorktreeAuthority::Test { live },
            controller_generation: 0,
            runtime_generation: 0,
            root: RetainedWorktreeHandle::for_test(b"root"),
            repository: RetainedWorktreeHandle::for_test(b"repository"),
            git_dir: RetainedWorktreeHandle::for_test(b"gitdir"),
            common_dir: Some(RetainedWorktreeHandle::for_test(b"commondir")),
            linked_worktree: RetainedWorktreeHandle::for_test(b"linked"),
            target: RetainedWorktreeHandle::for_test(b"target"),
            endpoint: Some(WorktreeEndpointLease {
                handle: Arc::new(
                    File::open(std::env::current_exe().expect("test executable path"))
                        .expect("test endpoint handle"),
                ),
                identity: digest_with_tag(b"test-worktree-endpoint-v1", b"endpoint"),
            }),
            deadline: Instant::now() + MAX_OPERATION_TIMEOUT,
            limits: WorktreeExecutionLimits::default(),
            process_owner: None,
            journal_store: Arc::new(journal_store),
            // Test service instances represent one reacquired fake lease so
            // durable recovery can verify the stable lease identity across a
            // drop/reopen boundary.
            identity: {
                let digest = digest_with_tag(b"test-worktree-execution-lease-v1", b"fixture");
                let mut identity = [0; 16];
                identity.copy_from_slice(&digest[..16]);
                identity
            },
        }
    }
}

impl sealed::Authority for WorktreeExecutionLease {}

pub(crate) trait WorktreeAuthorityCapability: sealed::Authority + Send + Sync {
    fn lease_identity(&self) -> [u8; 16];
    fn is_live(&self) -> bool;
}

impl WorktreeAuthorityCapability for WorktreeExecutionLease {
    fn lease_identity(&self) -> [u8; 16] {
        self.identity()
    }

    fn is_live(&self) -> bool {
        WorktreeExecutionLease::is_live(self)
    }
}

impl WorkspaceScope {
    /// Recovery authority is the task/workspace/repository and the immutable
    /// root/link/process generation fence.  Client/connection/request IDs are
    /// invocation correlation only and must not let a restart tombstone an
    /// operation issued by another live connection.
    fn stable_eq(&self, other: &Self) -> bool {
        self.task_id == other.task_id
            && self.project_id == other.project_id
            && self.workspace == other.workspace
            && self.action_epoch == other.action_epoch
            && self.runtime_generation == other.runtime_generation
            && self.process_epoch == other.process_epoch
            && self.root == other.root
            && self.repository == other.repository
            && self.process_owner == other.process_owner
            && self.execution_lease == other.execution_lease
            && self.linked == other.linked
    }
}

impl fmt::Debug for WorkspaceScope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("WorkspaceScope(REDACTED)")
    }
}

#[derive(Clone)]
pub(crate) struct ResolvedWorkspace {
    pub(crate) identity: WorkspaceIdentity,
    pub(crate) linked: LinkedWorktreeIdentity,
    pub(crate) scope: WorkspaceScope,
    pub(crate) process_fence: Option<RegistryManagedProcessFence>,
    pub(crate) execution_lease: Option<Arc<WorktreeExecutionLease>>,
}

impl fmt::Debug for ResolvedWorkspace {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ResolvedWorkspace(REDACTED)")
    }
}

#[derive(Clone, Copy)]
pub(crate) struct ExecutionBudget {
    deadline: Instant,
    max_nodes: usize,
    max_bytes: usize,
    max_output_bytes: usize,
}

impl ExecutionBudget {
    fn from_timeout(timeout: Duration) -> Self {
        Self {
            deadline: Instant::now() + timeout.min(MAX_OPERATION_TIMEOUT),
            max_nodes: WorktreeExecutionLimits::default().max_nodes,
            max_bytes: WorktreeExecutionLimits::default().max_bytes,
            max_output_bytes: WorktreeExecutionLimits::default().max_output_bytes,
        }
    }

    pub(crate) fn expired(self) -> bool {
        Instant::now() >= self.deadline
    }

    pub(crate) fn remaining(self) -> Duration {
        self.deadline.saturating_duration_since(Instant::now())
    }

    pub(crate) fn max_nodes(self) -> usize {
        self.max_nodes
    }

    pub(crate) fn max_bytes(self) -> usize {
        self.max_bytes
    }

    pub(crate) fn max_output_bytes(self) -> usize {
        self.max_output_bytes
    }
}

#[derive(Clone, PartialEq, Eq)]
pub(crate) struct WorktreePlan {
    pub(crate) branch: String,
    pub(crate) attempt: usize,
    pub(crate) scope: WorkspaceScope,
    pub(crate) identity: WorkspaceIdentity,
    pub(crate) base_commit: [u8; 32],
    pub(crate) base_revision: String,
    pub(crate) target: WorktreeTarget,
    pub(crate) linked: LinkedWorktreeIdentity,
    pub(crate) repository: [u8; 32],
}

#[derive(Clone)]
pub(crate) struct ProcessZeroProof {
    pub(crate) identity: WorkspaceIdentity,
    pub(crate) fence: RegistryManagedProcessFence,
    /// This receipt is issued by the process-owned adapter together with its
    /// fresh ACTIVE_PROCESS_ZERO observation.  The fence is carried alongside
    /// it so an old zero epoch can never authorize a different managed root.
    pub(crate) zero_observation: u64,
}

impl fmt::Debug for ProcessZeroProof {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ProcessZeroProof(REDACTED)")
    }
}

#[derive(Clone, PartialEq, Eq)]
pub(crate) enum ProbeResult {
    Available {
        base_commit: [u8; 32],
        base_revision: String,
        target: WorktreeTarget,
    },
    Collision,
}

#[derive(Clone)]
pub(crate) enum AddResult {
    Applied(CreatedWorktree),
    InterruptedAfterSideEffect,
}

#[derive(Clone)]
pub(crate) enum RecoveryLookup {
    Absent,
    Applied(CreatedWorktree),
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum CleanupState {
    Clean,
    Dirty,
}

#[derive(Clone, Copy)]
pub(crate) struct CleanupSnapshot {
    pub(crate) tracked: CleanupState,
    pub(crate) untracked: CleanupState,
    pub(crate) unpushed: CleanupState,
    pub(crate) nested: CleanupState,
    pub(crate) linked: CleanupState,
    pub(crate) foreign: CleanupState,
    pub(crate) main_checkout: CleanupState,
}

impl CleanupSnapshot {
    fn user_dirty(&self) -> bool {
        [self.tracked, self.untracked, self.unpushed]
            .iter()
            .any(|state| *state == CleanupState::Dirty)
    }

    fn ownership_conflict(&self) -> bool {
        [self.nested, self.linked, self.foreign, self.main_checkout]
            .iter()
            .any(|state| *state == CleanupState::Dirty)
    }
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum JournalState {
    Intent,
    SideEffectApplied,
    Settled,
    Compensated,
    Aborted,
    Refused,
    Recoverable,
}

fn valid_state_transition(current: JournalState, next: JournalState) -> bool {
    matches!(
        (current, next),
        (JournalState::Intent, JournalState::SideEffectApplied)
            | (JournalState::Intent, JournalState::Aborted)
            | (JournalState::Intent, JournalState::Recoverable)
            | (JournalState::Intent, JournalState::Refused)
            | (JournalState::Intent, JournalState::Settled)
            | (JournalState::SideEffectApplied, JournalState::Settled)
            | (JournalState::SideEffectApplied, JournalState::Compensated)
            | (JournalState::SideEffectApplied, JournalState::Recoverable)
            | (JournalState::SideEffectApplied, JournalState::Refused)
            | (JournalState::Refused, JournalState::Intent)
    )
}

impl fmt::Debug for JournalState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Intent => "Intent",
            Self::SideEffectApplied => "SideEffectApplied",
            Self::Settled => "Settled",
            Self::Compensated => "Compensated",
            Self::Aborted => "Aborted",
            Self::Refused => "Refused",
            Self::Recoverable => "Recoverable",
        })
    }
}

#[derive(Clone)]
pub(crate) struct JournalRecord {
    pub(crate) operation: JournalOperation,
    pub(crate) owner: OperationOwner,
    pub(crate) state: JournalState,
    pub(crate) version: u64,
    pub(crate) scope: WorkspaceScope,
    pub(crate) plan: WorktreePlan,
    pub(crate) receipt: Option<CreatedWorktree>,
}

impl JournalRecord {
    pub(crate) fn state(&self) -> JournalState {
        self.state
    }

    pub(crate) fn owner(&self) -> OperationOwner {
        self.owner
    }
}

/// A redacted read-only view used by focused tests and diagnostics.
pub struct JournalRecordView {
    state: JournalState,
}

impl fmt::Debug for JournalRecordView {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("JournalRecordView")
            .field("state", &self.state)
            .finish()
    }
}

impl JournalRecordView {
    pub fn state(&self) -> JournalState {
        self.state
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum JournalKind {
    Add,
    Remove,
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct JournalOperation {
    pub(crate) kind: JournalKind,
    pub(crate) key: OperationKey,
}

impl fmt::Debug for JournalOperation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("JournalOperation(REDACTED)")
    }
}

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct ReservationKey {
    workspace: WorkspaceIdentity,
    repository: [u8; 32],
    branch: String,
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct ReservationLease {
    operation: JournalOperation,
    owner: OperationOwner,
}

impl fmt::Debug for ReservationKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ReservationKey(REDACTED)")
    }
}

pub(crate) mod sealed {
    pub trait Executor {}
    pub trait Journal {}
    #[allow(dead_code)]
    pub trait Authority {}
}

/// Sealed, process-owned executor contract.  External callers receive the
/// concrete test executor only; production implementors must be joined inside
/// the crate after the accepted Git owner supplies the Job/ProcessOwner seam.
pub(crate) trait GitWorktreeExecutor: sealed::Executor + Send + Sync {
    fn probe(
        &self,
        _workspace: &ResolvedWorkspace,
        plan: &WorktreePlan,
        cancellation: &CancellationToken,
        budget: ExecutionBudget,
    ) -> Result<ProbeResult, ExecutorError>;

    fn add(
        &self,
        _workspace: &ResolvedWorkspace,
        operation: JournalOperation,
        plan: &WorktreePlan,
        cancellation: &CancellationToken,
        budget: ExecutionBudget,
    ) -> Result<AddResult, ExecutorError>;

    fn inspect(
        &self,
        _workspace: &ResolvedWorkspace,
        operation: JournalOperation,
        plan: &WorktreePlan,
        expected_receipt: Option<&CreatedWorktree>,
        cancellation: &CancellationToken,
        budget: ExecutionBudget,
    ) -> Result<RecoveryLookup, ExecutorError>;

    fn compensate(
        &self,
        workspace: &ResolvedWorkspace,
        operation: JournalOperation,
        receipt: &CreatedWorktree,
        cancellation: &CancellationToken,
        budget: ExecutionBudget,
    ) -> Result<(), ExecutorError>;

    fn preview(
        &self,
        workspace: &ResolvedWorkspace,
        receipt: &CreatedWorktree,
        cancellation: &CancellationToken,
        budget: ExecutionBudget,
    ) -> Result<CleanupSnapshot, ExecutorError>;

    fn prove_process_zero(
        &self,
        workspace: &ResolvedWorkspace,
        receipt: &CreatedWorktree,
        cancellation: &CancellationToken,
        budget: ExecutionBudget,
    ) -> Result<ProcessZeroProof, ExecutorError>;

    fn remove(
        &self,
        workspace: &ResolvedWorkspace,
        operation: JournalOperation,
        receipt: &CreatedWorktree,
        proof: ProcessZeroProof,
        cancellation: &CancellationToken,
        budget: ExecutionBudget,
    ) -> Result<(), ExecutorError>;
}

pub(crate) trait DurableOperationJournal: sealed::Journal + Send + Sync {
    fn insert_intent(
        &self,
        record: JournalRecord,
        context: JournalContext<'_>,
    ) -> Result<(), JournalError>;
    /// CAS variant bound to the current operation owner.  Durable adapters
    /// must validate owner and version in the same transaction as the state
    /// transition.  There is deliberately no sequential fallback: a future
    /// production adapter must prove this invariant before it can be wired.
    fn update_owned_cas(
        &self,
        operation: JournalOperation,
        owner: OperationOwner,
        expected_version: u64,
        expected_state: JournalState,
        next_state: JournalState,
        receipt: Option<CreatedWorktree>,
        context: JournalContext<'_>,
    ) -> Result<JournalRecord, JournalError>;
    fn get(
        &self,
        operation: JournalOperation,
        context: JournalContext<'_>,
    ) -> Result<Option<JournalRecord>, JournalError>;
    fn records(
        &self,
        limit: usize,
        context: JournalContext<'_>,
    ) -> Result<Vec<JournalRecord>, JournalError>;
    /// Remove only reservations whose durable operation record is absent from
    /// the exact stable scope.  This closes the crash window between a
    /// workspace reservation and intent insertion during startup recovery.
    fn reconcile_reservations(
        &self,
        scope: &WorkspaceScope,
        context: JournalContext<'_>,
    ) -> Result<(), JournalError>;
    fn reserve(
        &self,
        scope: &WorkspaceScope,
        branch: &str,
        operation: JournalOperation,
        owner: OperationOwner,
        context: JournalContext<'_>,
    ) -> Result<(), JournalError>;
    fn release(
        &self,
        scope: &WorkspaceScope,
        branch: &str,
        operation: JournalOperation,
        owner: OperationOwner,
        context: JournalContext<'_>,
    ) -> Result<(), JournalError>;
    /// Atomically settle a terminal state and release the exact reservation.
    /// Implementors must perform both changes in one durable transaction; a
    /// default sequential fallback would make a crash leave a settled record
    /// reserved (or a side effect unreserved).
    fn settle_and_release(
        &self,
        operation: JournalOperation,
        expected_version: u64,
        expected_state: JournalState,
        next_state: JournalState,
        receipt: Option<CreatedWorktree>,
        scope: &WorkspaceScope,
        branch: &str,
        owner: OperationOwner,
        context: JournalContext<'_>,
    ) -> Result<JournalRecord, JournalError>;
    /// Claim an interrupted operation and its reservation in one CAS
    /// transaction.  A default implementation would permit two restart
    /// workers to inspect/settle the same side effect, so adapters must supply
    /// the durable transaction explicitly.
    fn claim_recovery(
        &self,
        operation: JournalOperation,
        expected_version: u64,
        expected_state: JournalState,
        expected_owner: OperationOwner,
        new_owner: OperationOwner,
        context: JournalContext<'_>,
    ) -> Result<JournalRecord, JournalError>;
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum ExecutorError {
    Cancelled,
    Deadline,
    Collision,
    IdentityMismatch,
    ProcessNotZero,
    InterruptedAfterSideEffect,
    CompensationFailed,
    NotFound,
    MalformedOutput,
    OversizeOutput,
}

impl fmt::Debug for ExecutorError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ExecutorError(REDACTED)")
    }
}

impl fmt::Display for ExecutorError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Cancelled => "Git operation was cancelled",
            Self::Deadline => "Git operation exceeded its deadline",
            Self::Collision => "Git worktree identity collided",
            Self::IdentityMismatch => "Git worktree identity changed",
            Self::ProcessNotZero => "managed process proof was not zero",
            Self::InterruptedAfterSideEffect => {
                "Git operation was interrupted after its side effect"
            }
            Self::CompensationFailed => "Git compensation failed",
            Self::NotFound => "Git worktree identity was not found",
            Self::MalformedOutput => "Git worktree output is malformed",
            Self::OversizeOutput => "Git worktree output exceeds the limit",
        })
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum JournalError {
    InvalidStore,
    Full,
    Duplicate,
    ReservationMissing,
    CasMismatch,
    ReservationBusy,
    OperationInFlight,
    OwnerMismatch,
    AtomicSettlementUnavailable,
    Busy,
    Cancelled,
    Deadline,
    Corrupt,
}

impl fmt::Debug for JournalError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("JournalError(REDACTED)")
    }
}

impl fmt::Display for JournalError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::InvalidStore => "worktree journal store is not host validated",
            Self::Full => "worktree operation journal is full",
            Self::Duplicate => "worktree operation already exists",
            Self::ReservationMissing => "worktree operation reservation is missing",
            Self::CasMismatch => "worktree operation state changed",
            Self::ReservationBusy => "worktree target is reserved",
            Self::OperationInFlight => "worktree operation is already in flight",
            Self::OwnerMismatch => "worktree operation ownership changed",
            Self::AtomicSettlementUnavailable => {
                "worktree operation settlement transaction is unavailable"
            }
            Self::Busy => "worktree operation journal is busy",
            Self::Cancelled => "worktree operation journal request was cancelled",
            Self::Deadline => "worktree operation journal request exceeded its deadline",
            Self::Corrupt => "worktree operation journal is corrupt",
        })
    }
}

#[derive(Clone, Copy)]
pub(crate) struct JournalContext<'a> {
    cancellation: &'a CancellationToken,
    budget: ExecutionBudget,
}

impl<'a> JournalContext<'a> {
    fn new(cancellation: &'a CancellationToken, budget: ExecutionBudget) -> Self {
        Self {
            cancellation,
            budget,
        }
    }

    pub(crate) fn check(self) -> Result<(), JournalError> {
        if self.cancellation.is_cancelled() {
            Err(JournalError::Cancelled)
        } else if self.budget.expired() {
            Err(JournalError::Deadline)
        } else {
            Ok(())
        }
    }

    pub(crate) fn remaining(self) -> Duration {
        self.budget.remaining()
    }
}

/// A truthful integration hold. The state machine must not report a pending
/// worktree as created while Task6.2 pins or the Task3 process owner are not
/// actually joined to the production seam.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum WorktreeHold {
    Task62AuthorityUnavailable,
    Task3AuthorityUnavailable,
    ProcessOwnerUnavailable,
}

impl fmt::Debug for WorktreeHold {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("WorktreeHold(REDACTED)")
    }
}

impl fmt::Display for WorktreeHold {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Task62AuthorityUnavailable => "workspace authority is unavailable",
            Self::Task3AuthorityUnavailable => "Git worktree authority is unavailable",
            Self::ProcessOwnerUnavailable => "managed process authority is unavailable",
        })
    }
}

/// Typed, redacted errors for worktree orchestration.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum WorktreeError {
    Hold(WorktreeHold),
    AuthorityUnavailable,
    ExecutorUnavailable,
    JournalFull,
    InvalidLabel,
    InvalidBranch,
    BranchCollision,
    TargetCollision,
    OperationInFlight,
    CollisionBudgetExceeded,
    StaleAuthority,
    WorkspaceChanged,
    Cancelled,
    Deadline,
    Interrupted,
    RecoverableOperation,
    CleanupRefused,
    ProcessNotZero,
    AlreadyRemoved,
    MalformedPorcelain,
    OversizePorcelain,
    ExecutorFailure,
    JournalFailure,
}

impl fmt::Debug for WorktreeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("WorktreeError(REDACTED)")
    }
}

impl fmt::Display for WorktreeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::Hold(hold) => return hold.fmt(f),
            Self::AuthorityUnavailable => "Git worktree authority is unavailable",
            Self::ExecutorUnavailable => "Git worktree executor is unavailable",
            Self::JournalFull => "worktree operation journal is full",
            Self::InvalidLabel => "worktree label is invalid",
            Self::InvalidBranch => "worktree branch is invalid",
            Self::BranchCollision => "requested worktree branch already exists",
            Self::TargetCollision => "requested worktree target already exists",
            Self::OperationInFlight => "worktree operation is already in flight",
            Self::CollisionBudgetExceeded => "worktree collision attempts exceeded the limit",
            Self::StaleAuthority => "workspace authority is stale",
            Self::WorkspaceChanged => "workspace identity changed",
            Self::Cancelled => "worktree request was cancelled",
            Self::Deadline => "worktree request exceeded its deadline",
            Self::Interrupted => "worktree request was interrupted after its side effect",
            Self::RecoverableOperation => "worktree operation requires recovery",
            Self::CleanupRefused => "worktree cleanup was refused",
            Self::ProcessNotZero => "managed process proof was not zero",
            Self::AlreadyRemoved => "worktree has already been removed",
            Self::MalformedPorcelain => "git worktree output is malformed",
            Self::OversizePorcelain => "git worktree output exceeds the limit",
            Self::ExecutorFailure => "Git worktree executor failed",
            Self::JournalFailure => "worktree operation journal failed",
        };
        f.write_str(message)
    }
}

impl std::error::Error for WorktreeError {}

/// A bounded redacted record from `git worktree list --porcelain -z`.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct PorcelainRecord {
    identity: WorkspaceIdentity,
    branch: Option<[u8; MAX_BRANCH_BYTES]>,
}

impl fmt::Debug for PorcelainRecord {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("PorcelainRecord(REDACTED)")
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum PorcelainError {
    Malformed,
    Oversize,
    TooManyRecords,
}

impl fmt::Debug for PorcelainError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("PorcelainError(REDACTED)")
    }
}

impl fmt::Display for PorcelainError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Malformed => "git worktree output is malformed",
            Self::Oversize => "git worktree output exceeds the limit",
            Self::TooManyRecords => "git worktree output contains too many records",
        })
    }
}

impl std::error::Error for PorcelainError {}

/// Parse only the bounded identity/branch subset of Git's NUL-delimited
/// porcelain.  Raw paths are hashed and never returned to callers.
pub fn parse_worktree_porcelain(input: &[u8]) -> Result<Vec<PorcelainRecord>, PorcelainError> {
    if input.len() > MAX_PORCELAIN_BYTES {
        return Err(PorcelainError::Oversize);
    }
    if input.is_empty() {
        return Ok(Vec::new());
    }
    let mut records = Vec::new();
    let mut current: Option<(WorkspaceIdentity, Option<[u8; MAX_BRANCH_BYTES]>)> = None;
    for field in input.split(|byte| *byte == 0) {
        if field.is_empty() {
            if let Some((identity, branch)) = current.take() {
                records.push(PorcelainRecord { identity, branch });
                if records.len() > MAX_PORCELAIN_RECORDS {
                    return Err(PorcelainError::TooManyRecords);
                }
            }
            continue;
        }
        if let Some(value) = field.strip_prefix(b"worktree ") {
            if current.is_some() || value.is_empty() {
                return Err(PorcelainError::Malformed);
            }
            current = Some((opaque_identity(value), None));
        } else if let Some(value) = field.strip_prefix(b"branch ") {
            let Some((_, branch)) = current.as_mut() else {
                return Err(PorcelainError::Malformed);
            };
            if value.is_empty() || value.len() > MAX_BRANCH_BYTES {
                return Err(PorcelainError::Malformed);
            }
            let mut result = [0; MAX_BRANCH_BYTES];
            result[..value.len()].copy_from_slice(value);
            *branch = Some(result);
        } else if let Some(value) = field.strip_prefix(b"HEAD ") {
            if current.is_none()
                || value.is_empty()
                || value.len() > 128
                || !value
                    .iter()
                    .all(|byte| byte.is_ascii_hexdigit() || *byte == b'-')
            {
                return Err(PorcelainError::Malformed);
            }
        } else if matches!(field, b"detached" | b"locked" | b"prunable") {
            if current.is_none() {
                return Err(PorcelainError::Malformed);
            }
        } else {
            return Err(PorcelainError::Malformed);
        }
    }
    if let Some((identity, branch)) = current.take() {
        records.push(PorcelainRecord { identity, branch });
    }
    if records.len() > MAX_PORCELAIN_RECORDS {
        return Err(PorcelainError::TooManyRecords);
    }
    if records.is_empty() {
        return Err(PorcelainError::Malformed);
    }
    Ok(records)
}

fn opaque_identity(value: &[u8]) -> WorkspaceIdentity {
    let mut hasher = Sha256::new();
    hasher.update(b"devmanager-worktree-identity-v1\0");
    hasher.update(value);
    WorkspaceIdentity(hasher.finalize().into())
}

trait Admission {
    fn revalidate(&self) -> Result<ResolvedWorkspace, WorktreeError>;
}

struct LiveAdmission<'a> {
    authorization: &'a WorkspaceAuthorization,
    lease: &'a WorkspaceResourceLease,
    context: &'a WorkspaceActionContext,
}

impl Admission for LiveAdmission<'_> {
    fn revalidate(&self) -> Result<ResolvedWorkspace, WorktreeError> {
        if self.lease.resource() != WorkspaceResource::Git {
            return Err(WorktreeError::StaleAuthority);
        }
        self.lease
            .ensure_active()
            .map_err(|_| WorktreeError::StaleAuthority)?;
        let binding = self
            .authorization
            .validated_binding(
                self.context.task_id,
                self.context.project_id,
                self.context.client_id,
                self.context.connection_id,
                self.context.request_id,
                self.context.command_id,
                &self.context.workspace,
                self.context.action_epoch,
                self.context.runtime_generation,
            )
            .ok_or(WorktreeError::StaleAuthority)?;
        Ok(resolved_workspace(
            binding,
            self.context.task_id,
            self.context.project_id,
            self.context.client_id,
            self.context.connection_id,
            self.context.request_id,
            self.context.command_id,
            self.context.action_epoch,
            self.context.runtime_generation,
        ))
    }
}

/// Host-owned Task Cockpit seam: re-check the exact Task/workspace fence
/// immediately before a mutation. Path strings never authorize. A File
/// mutation must present a File lease; Git/worktree mutations present Git.
pub(crate) fn revalidate_cockpit_workspace_action(
    authorization: &WorkspaceAuthorization,
    lease: &WorkspaceResourceLease,
    context: &WorkspaceActionContext,
    resource: WorkspaceResource,
) -> Result<(), WorktreeError> {
    if lease.resource() != resource {
        return Err(WorktreeError::StaleAuthority);
    }
    lease
        .ensure_active()
        .map_err(|_| WorktreeError::StaleAuthority)?;
    authorization
        .validated_binding(
            context.task_id,
            context.project_id,
            context.client_id,
            context.connection_id,
            context.request_id,
            context.command_id,
            &context.workspace,
            context.action_epoch,
            context.runtime_generation,
        )
        .ok_or(WorktreeError::StaleAuthority)?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn resolved_workspace(
    binding: &WorkspaceBinding,
    task_id: TaskId,
    project_id: ProjectId,
    client_id: ClientId,
    connection_id: uuid::Uuid,
    request_id: RequestId,
    command_id: CommandId,
    action_epoch: u64,
    runtime_generation: u64,
) -> ResolvedWorkspace {
    let binding_fact = binding.durable_ref().host_binding();
    let mut hasher = Sha256::new();
    hasher.update(b"devmanager-worktree-scope-v1\0");
    // A live host-bound fact supplies the opaque identity allocated by
    // WorkspaceService.  The display path is deliberately never used as
    // authority (the fallback is only for non-admitted test/model values).
    if let Some(fact) = binding_fact {
        hasher.update(fact.binding_fingerprint().as_str().as_bytes());
    } else {
        hasher.update(binding.identity_key().as_bytes());
    }
    if let Some(repository) = binding.repository() {
        hasher.update(repository.fingerprint().as_str().as_bytes());
    }
    if let Some(branch) = binding.branch() {
        hasher.update(branch.as_bytes());
    }
    let identity = WorkspaceIdentity(hasher.finalize().into());
    let fact_identity = |tag: &[u8], fact: Option<&crate::domain::task::WorkspacePathFact>| {
        fact.map(|value| digest_with_tag(tag, value.opaque_id().as_bytes()))
            .unwrap_or_else(|| digest_with_tag(tag, &identity.0))
    };
    let linked = LinkedWorktreeIdentity {
        gitdir: fact_identity(b"gitdir", binding_fact.and_then(|fact| fact.gitdir())),
        commondir: fact_identity(b"commondir", binding_fact.and_then(|fact| fact.commondir())),
        backreference: fact_identity(
            b"backreference",
            binding_fact.and_then(|fact| fact.admin_dir()),
        ),
        repository: fact_identity(
            b"repository",
            binding_fact.and_then(|fact| fact.repository_root()),
        ),
    };
    let root = fact_identity(
        b"workspace-root",
        binding_fact.map(|fact| fact.workspace_root()),
    );
    let repository = binding
        .repository()
        .map(|repository| {
            digest_with_tag(
                b"repository-fingerprint",
                repository.fingerprint().as_str().as_bytes(),
            )
        })
        .unwrap_or(ZERO_FINGERPRINT);
    let scope = WorkspaceScope {
        task_id,
        project_id,
        workspace: identity,
        client_id,
        connection_id,
        request_id,
        command_id,
        action_epoch,
        runtime_generation,
        process_epoch: runtime_generation,
        root,
        repository,
        // No process owner is authoritative until the accepted Task3
        // adapter supplies a ManagedProcessFence.  Do not synthesize one
        // from a task ID and accidentally treat it as a process lease.
        process_owner: [0; 16],
        execution_lease: [0; 16],
        linked,
    };
    ResolvedWorkspace {
        identity,
        linked,
        scope,
        process_fence: None,
        execution_lease: None,
    }
}

fn digest_with_tag(tag: &[u8], value: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(tag);
    hasher.update(value);
    hasher.finalize().into()
}

fn process_owner_identity(fence: &RegistryManagedProcessFence) -> [u8; 16] {
    let mut hasher = Sha256::new();
    hasher.update(b"devmanager-worktree-process-owner-v1\0");
    match fence.owner() {
        ProcessOwner::Task(task_id) => hasher.update(task_id.as_bytes()),
        ProcessOwner::Host => hasher.update(b"host"),
    }
    hasher.update(fence.resource().resource_id.as_bytes());
    hasher.update(fence.resource().runtime_generation.to_le_bytes());
    let digest = hasher.finalize();
    let mut identity = [0; 16];
    identity.copy_from_slice(&digest[..16]);
    identity
}

/// Restore the exact current Host execution-lease process fence onto a freshly
/// resolved workspace. `resolved_workspace` intentionally leaves process
/// ownership unset; production admission and every post-admission revalidation
/// must reattach the live lease fence so comparisons reject changed/stale
/// fences without synthesizing one from task/path scalars. Test admissions that
/// already supply a fence under `WorktreeAuthority::Test` are left intact.
fn apply_live_process_owner_fence(
    workspace: &mut ResolvedWorkspace,
    lease: &WorktreeExecutionLease,
) -> Result<(), WorktreeError> {
    match &lease.authority {
        WorktreeAuthority::Host { .. } => {
            let Some(process_fence) = lease.process_owner.clone() else {
                return Err(WorktreeError::Hold(WorktreeHold::ProcessOwnerUnavailable));
            };
            workspace.process_fence = Some(process_fence.clone());
            workspace.scope.process_owner = process_owner_identity(&process_fence);
            Ok(())
        }
        #[cfg(test)]
        WorktreeAuthority::Test { .. } => Ok(()),
    }
}

/// Reports restart recovery without exposing operation IDs or paths.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RecoveryReport {
    recovered: usize,
    tombstones: usize,
}

impl RecoveryReport {
    pub fn recovered(&self) -> usize {
        self.recovered
    }

    pub fn tombstones(&self) -> usize {
        self.tombstones
    }
}

/// Worktree orchestration service.  Target mutations are coordinated by the
/// durable journal's workspace-scoped reservations, and every executor/journal
/// implementation is injected through crate-private sealed contracts.
pub struct WorktreeService {
    executor: Option<Arc<dyn GitWorktreeExecutor>>,
    journal: Option<Arc<dyn DurableOperationJournal>>,
    execution_lease: Option<Arc<WorktreeExecutionLease>>,
}

impl fmt::Debug for WorktreeService {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("WorktreeService(REDACTED)")
    }
}

impl WorktreeService {
    /// Production construction remains fail-closed until the accepted Task 3
    /// process-owned Git executor and durable journal are joined here.
    pub fn new() -> Result<Self, WorktreeError> {
        Err(WorktreeError::Hold(WorktreeHold::Task3AuthorityUnavailable))
    }

    /// Crate-only wiring seam for the future accepted Git/process owner.
    #[cfg(not(test))]
    pub(crate) fn from_process_owned(
        lease: WorktreeExecutionLease,
        executor: Arc<dyn GitWorktreeExecutor>,
    ) -> Result<Self, WorktreeError> {
        let journal = SqliteWorktreeJournal::from_store(Arc::clone(&lease.journal_store))
            .map_err(map_journal_error)?;
        Ok(Self {
            executor: Some(executor),
            journal: Some(Arc::new(journal)),
            execution_lease: Some(Arc::new(lease)),
        })
    }

    /// Test-only wiring uses the same sealed executor/journal path, but its
    /// authority capability is an explicit fake rather than a production
    /// scalar/path constructor.
    #[cfg(test)]
    pub(crate) fn from_process_owned(
        executor: Arc<dyn GitWorktreeExecutor>,
        journal: Arc<dyn DurableOperationJournal>,
    ) -> Self {
        Self {
            executor: Some(executor),
            journal: Some(journal),
            execution_lease: Some(Arc::new(WorktreeExecutionLease::for_test())),
        }
    }

    /// Construct the pure state machine with a deterministic fake executor and
    /// bounded in-memory journal for focused tests.
    #[cfg(test)]
    pub(crate) fn for_test(
        executor: TestGitWorktreeExecutor,
        journal: TestOperationJournal,
    ) -> Self {
        Self::from_process_owned(Arc::new(executor), Arc::new(journal))
    }

    fn admitted_workspace<A: Admission>(
        &self,
        admission: &A,
    ) -> Result<ResolvedWorkspace, WorktreeError> {
        let lease = self
            .execution_lease
            .as_ref()
            .ok_or(WorktreeError::Hold(WorktreeHold::Task3AuthorityUnavailable))?;
        if !WorktreeAuthorityCapability::is_live(&**lease) {
            return Err(WorktreeError::StaleAuthority);
        }
        let mut workspace = admission.revalidate()?;
        if workspace.scope.action_epoch != lease.controller_generation
            || workspace.scope.runtime_generation != lease.runtime_generation
        {
            return Err(WorktreeError::StaleAuthority);
        }
        apply_live_process_owner_fence(&mut workspace, lease)?;
        workspace.scope.execution_lease = lease.identity();
        workspace.execution_lease = Some(Arc::clone(lease));
        Ok(workspace)
    }

    /// Production-facing operation entry point.  The caller must present the
    /// opaque authorization, live Git lease, and exact current-generation
    /// context issued by WorkspaceService.
    pub fn create(
        &self,
        authorization: &WorkspaceAuthorization,
        lease: &WorkspaceResourceLease,
        context: &WorkspaceActionContext,
        request: CreateWorktreeRequest,
    ) -> Result<CreatedWorktree, WorktreeError> {
        let admission = LiveAdmission {
            authorization,
            lease,
            context,
        };
        self.create_inner(&admission, request)
    }

    #[cfg(test)]
    pub(crate) fn create_for_test(
        &self,
        authorization: &TestWorkspaceAuthorization,
        request: CreateWorktreeRequest,
    ) -> Result<CreatedWorktree, WorktreeError> {
        self.create_inner(authorization, request)
    }

    /// Production-facing cleanup entry point.  The same live authorization
    /// and Git lease fence both preview and removal.
    pub fn remove(
        &self,
        authorization: &WorkspaceAuthorization,
        lease: &WorkspaceResourceLease,
        context: &WorkspaceActionContext,
        receipt: &CreatedWorktree,
        confirmation: CleanupConfirmation,
    ) -> Result<(), WorktreeError> {
        let admission = LiveAdmission {
            authorization,
            lease,
            context,
        };
        self.remove_inner(&admission, receipt, confirmation)
    }

    #[cfg(test)]
    pub(crate) fn remove_for_test(
        &self,
        authorization: &TestWorkspaceAuthorization,
        receipt: &CreatedWorktree,
        confirmation: CleanupConfirmation,
    ) -> Result<(), WorktreeError> {
        self.remove_inner(authorization, receipt, confirmation)
    }

    #[cfg(test)]
    pub(crate) fn recover_for_test(
        &self,
        authorization: &TestWorkspaceAuthorization,
    ) -> Result<RecoveryReport, WorktreeError> {
        let cancellation = CancellationToken::new();
        let budget = ExecutionBudget::from_timeout(MAX_OPERATION_TIMEOUT);
        self.recover_inner(authorization, &cancellation, budget)
    }

    /// Reconcile durable intents after a restart using a still-live
    /// WorkspaceService authorization.  Unknown effects become tombstones.
    pub fn recover(
        &self,
        authorization: &WorkspaceAuthorization,
        lease: &WorkspaceResourceLease,
        context: &WorkspaceActionContext,
    ) -> Result<RecoveryReport, WorktreeError> {
        self.recover_with_cancellation(
            authorization,
            lease,
            context,
            CancellationToken::new(),
            MAX_OPERATION_TIMEOUT,
        )
    }

    /// Reconcile using one caller-owned cancellation token and one absolute
    /// deadline.  Restart reconciliation is bounded independently from any
    /// one Git command, while callers can still stop a long journal scan.
    pub fn recover_with_cancellation(
        &self,
        authorization: &WorkspaceAuthorization,
        lease: &WorkspaceResourceLease,
        context: &WorkspaceActionContext,
        cancellation: CancellationToken,
        timeout: Duration,
    ) -> Result<RecoveryReport, WorktreeError> {
        let admission = LiveAdmission {
            authorization,
            lease,
            context,
        };
        let budget = ExecutionBudget::from_timeout(timeout);
        self.recover_inner(&admission, &cancellation, budget)
    }

    fn create_inner<A: Admission>(
        &self,
        admission: &A,
        request: CreateWorktreeRequest,
    ) -> Result<CreatedWorktree, WorktreeError> {
        let executor = self
            .executor
            .as_ref()
            .ok_or(WorktreeError::ExecutorUnavailable)?;
        let journal = self
            .journal
            .as_ref()
            .ok_or(WorktreeError::ExecutorUnavailable)?;
        let mut request = request;
        validate_create_request(&request)?;
        let requested_budget = ExecutionBudget::from_timeout(request.timeout);
        check_budget(&request.cancellation, requested_budget)?;
        let workspace = self.admitted_workspace(admission)?;
        if request
            .task_id
            .is_some_and(|task_id| task_id != workspace.scope.task_id)
            || request
                .workspace
                .is_some_and(|identity| identity != workspace.identity)
        {
            return Err(WorktreeError::WorkspaceChanged);
        }
        #[cfg(test)]
        {
            request.task_id.get_or_insert(workspace.scope.task_id);
            request.workspace.get_or_insert(workspace.identity);
        }
        #[cfg(not(test))]
        if request.target.is_none() || request.task_id.is_none() || request.workspace.is_none() {
            return Err(WorktreeError::Hold(
                WorktreeHold::Task62AuthorityUnavailable,
            ));
        }
        let budget = workspace
            .execution_lease
            .as_ref()
            .expect("admitted workspace carries execution lease")
            .bound_budget(requested_budget);
        let journal_context = JournalContext::new(&request.cancellation, budget);
        check_budget(&request.cancellation, budget)?;
        let operation = JournalOperation {
            kind: JournalKind::Add,
            key: OperationKey(request.idempotency_key),
        };
        let owner = OperationOwner::fresh();

        let existing = journal
            .get(operation, journal_context)
            .map_err(map_journal_error)?;
        check_budget(&request.cancellation, budget)?;
        if let Some(existing) = existing {
            return self.replay_or_recover_create(
                admission, &workspace, &request, operation, existing, budget,
            );
        }

        let plan = choose_plan(
            admission,
            executor.as_ref(),
            journal.as_ref(),
            &workspace,
            &request,
            operation,
            owner,
            budget,
        )?;
        request.target = Some(plan.target.clone());
        check_budget(&request.cancellation, budget).or_else(|error| {
            journal
                .release(
                    &workspace.scope,
                    &plan.branch,
                    operation,
                    owner,
                    journal_context,
                )
                .map_err(map_journal_error)?;
            Err(error)
        })?;
        if let Err(error) = revalidate_same(admission, &workspace) {
            journal
                .release(
                    &workspace.scope,
                    &plan.branch,
                    operation,
                    owner,
                    journal_context,
                )
                .map_err(map_journal_error)?;
            return Err(error);
        }
        let record = JournalRecord {
            operation,
            owner,
            state: JournalState::Intent,
            version: 0,
            scope: workspace.scope,
            plan: plan.clone(),
            receipt: None,
        };
        if let Err(error) = journal.insert_intent(record, journal_context) {
            journal
                .release(
                    &workspace.scope,
                    &plan.branch,
                    operation,
                    owner,
                    journal_context,
                )
                .map_err(map_journal_error)?;
            if matches!(error, JournalError::Duplicate) {
                let existing = journal
                    .get(operation, journal_context)
                    .map_err(map_journal_error)?
                    .ok_or(WorktreeError::JournalFailure)?;
                return self.replay_or_recover_create(
                    admission, &workspace, &request, operation, existing, budget,
                );
            }
            return Err(map_journal_error(error));
        }

        if let Err(error) = revalidate_same(admission, &workspace) {
            transition_and_release(
                journal.as_ref(),
                operation,
                owner,
                0,
                JournalState::Intent,
                JournalState::Recoverable,
                &workspace.scope,
                &plan.branch,
                None,
                journal_context,
            )?;
            return Err(error);
        }
        let outcome =
            match executor.add(&workspace, operation, &plan, &request.cancellation, budget) {
                Ok(outcome) => outcome,
                Err(error) => {
                    if matches!(error, ExecutorError::InterruptedAfterSideEffect) {
                        // The side effect may already exist.  Keep Intent and
                        // its owner reservation for restart recovery instead
                        // of releasing an effect we have not inspected.
                        return Err(WorktreeError::Interrupted);
                    }
                    transition_and_release(
                        journal.as_ref(),
                        operation,
                        owner,
                        0,
                        JournalState::Intent,
                        JournalState::Aborted,
                        &workspace.scope,
                        &plan.branch,
                        None,
                        journal_context,
                    )?;
                    return Err(map_executor_error(error));
                }
            };
        let receipt = match outcome {
            AddResult::InterruptedAfterSideEffect => {
                // Keep the owned reservation with the durable Intent.  A
                // restart reconciler must be able to claim it atomically;
                // releasing here would make the side effect orphanable.
                return Err(WorktreeError::Interrupted);
            }
            AddResult::Applied(receipt) => {
                if !receipt_matches_plan(&receipt, operation, &plan) {
                    transition_and_release(
                        journal.as_ref(),
                        operation,
                        owner,
                        0,
                        JournalState::Intent,
                        JournalState::Recoverable,
                        &workspace.scope,
                        &plan.branch,
                        Some(receipt),
                        journal_context,
                    )?;
                    return Err(WorktreeError::WorkspaceChanged);
                }
                receipt
            }
        };
        let post_effect_cancellation = CancellationToken::new();
        let post_effect_context = JournalContext::new(&post_effect_cancellation, budget);
        let applied = match journal.update_owned_cas(
            operation,
            owner,
            0,
            JournalState::Intent,
            JournalState::SideEffectApplied,
            Some(receipt.clone()),
            post_effect_context,
        ) {
            Ok(applied) => applied,
            Err(_) => return Err(WorktreeError::RecoverableOperation),
        };

        if request.cancellation.is_cancelled() {
            return self.compensate_or_tombstone(
                admission,
                &workspace,
                executor.as_ref(),
                journal.as_ref(),
                operation,
                &receipt,
                &plan,
                owner,
                applied.version,
            );
        }
        if revalidate_same(admission, &workspace).is_err() || budget.expired() {
            transition_and_release(
                journal.as_ref(),
                operation,
                owner,
                applied.version,
                JournalState::SideEffectApplied,
                JournalState::Recoverable,
                &workspace.scope,
                &plan.branch,
                Some(receipt.clone()),
                post_effect_context,
            )?;
            return Err(WorktreeError::RecoverableOperation);
        }
        let settled = match journal.settle_and_release(
            operation,
            applied.version,
            JournalState::SideEffectApplied,
            JournalState::Settled,
            Some(receipt.clone()),
            &workspace.scope,
            &plan.branch,
            owner,
            post_effect_context,
        ) {
            Ok(settled) => settled,
            Err(error) => return Err(map_journal_error(error)),
        };
        // `settle_and_release` is the terminal ownership boundary.  The
        // reservation is gone by the time this returns, so a later authority
        // change cannot reopen the record or manufacture a new reservation.
        let _ = settled;
        Ok(receipt)
    }

    fn replay_or_recover_create<A: Admission>(
        &self,
        admission: &A,
        workspace: &ResolvedWorkspace,
        request: &CreateWorktreeRequest,
        operation: JournalOperation,
        existing: JournalRecord,
        budget: ExecutionBudget,
    ) -> Result<CreatedWorktree, WorktreeError> {
        check_budget(&request.cancellation, budget)?;
        revalidate_same(admission, workspace)?;
        if existing.operation != operation
            || existing.scope != workspace.scope
            || existing.plan.scope != workspace.scope
            || !create_plan_matches_scope(&existing.plan, &workspace.scope)
            || !request_matches_plan(request, &existing.plan)
        {
            return Err(WorktreeError::WorkspaceChanged);
        }
        match existing.state {
            JournalState::Settled => {
                let receipt = existing
                    .receipt
                    .ok_or(WorktreeError::RecoverableOperation)?;
                if !receipt_matches_plan(&receipt, operation, &existing.plan) {
                    return Err(WorktreeError::WorkspaceChanged);
                }
                Ok(receipt)
            }
            JournalState::Recoverable => Err(WorktreeError::RecoverableOperation),
            JournalState::Compensated | JournalState::Aborted | JournalState::Refused => {
                Err(WorktreeError::RecoverableOperation)
            }
            // A second live caller is never a recovery authority.  Recovery
            // is reserved for the restart reconciler, otherwise it could
            // inspect an in-flight winner and release its reservation.
            JournalState::Intent | JournalState::SideEffectApplied => {
                Err(WorktreeError::OperationInFlight)
            }
        }
    }

    fn compensate_or_tombstone<A: Admission>(
        &self,
        admission: &A,
        workspace: &ResolvedWorkspace,
        executor: &dyn GitWorktreeExecutor,
        journal: &dyn DurableOperationJournal,
        operation: JournalOperation,
        receipt: &CreatedWorktree,
        plan: &WorktreePlan,
        owner: OperationOwner,
        version: u64,
    ) -> Result<CreatedWorktree, WorktreeError> {
        let cleanup_cancellation = CancellationToken::new();
        let cleanup_budget = ExecutionBudget::from_timeout(Duration::from_secs(5));
        let cleanup_context = JournalContext::new(&cleanup_cancellation, cleanup_budget);
        if revalidate_same(admission, workspace).is_err() {
            transition_and_release(
                journal,
                operation,
                owner,
                version,
                JournalState::SideEffectApplied,
                JournalState::Recoverable,
                &workspace.scope,
                &receipt.branch,
                Some(receipt.clone()),
                cleanup_context,
            )?;
            return Err(WorktreeError::RecoverableOperation);
        }
        // Request cancellation must not cancel cleanup of an already-created
        // worktree.  Use a fresh, independently bounded token for the
        // compensation attempt, then revalidate the authority/effect.
        match executor.compensate(
            workspace,
            operation,
            receipt,
            &cleanup_cancellation,
            cleanup_budget,
        ) {
            Ok(()) => {
                // A successful compensation call is not proof that the
                // linked worktree is gone.  Re-inspect with the independent
                // cleanup budget before making the terminal transition.
                match executor.inspect(
                    workspace,
                    operation,
                    plan,
                    Some(receipt),
                    &cleanup_cancellation,
                    cleanup_budget,
                ) {
                    Ok(RecoveryLookup::Absent) => {}
                    Ok(RecoveryLookup::Applied(_)) | Err(_) => {
                        transition_and_release(
                            journal,
                            operation,
                            owner,
                            version,
                            JournalState::SideEffectApplied,
                            JournalState::Recoverable,
                            &workspace.scope,
                            &receipt.branch,
                            Some(receipt.clone()),
                            cleanup_context,
                        )?;
                        return Err(WorktreeError::RecoverableOperation);
                    }
                }
                if revalidate_same(admission, workspace).is_err() {
                    transition_and_release(
                        journal,
                        operation,
                        owner,
                        version,
                        JournalState::SideEffectApplied,
                        JournalState::Recoverable,
                        &workspace.scope,
                        &receipt.branch,
                        Some(receipt.clone()),
                        cleanup_context,
                    )?;
                    return Err(WorktreeError::RecoverableOperation);
                }
                let compensated = match journal.settle_and_release(
                    operation,
                    version,
                    JournalState::SideEffectApplied,
                    JournalState::Compensated,
                    Some(receipt.clone()),
                    &workspace.scope,
                    &receipt.branch,
                    owner,
                    cleanup_context,
                ) {
                    Ok(record) => record,
                    Err(_) => return Err(WorktreeError::RecoverableOperation),
                };
                let _ = compensated;
                Err(WorktreeError::Cancelled)
            }
            Err(_) => {
                transition_and_release(
                    journal,
                    operation,
                    owner,
                    version,
                    JournalState::SideEffectApplied,
                    JournalState::Recoverable,
                    &workspace.scope,
                    &receipt.branch,
                    Some(receipt.clone()),
                    cleanup_context,
                )?;
                Err(WorktreeError::RecoverableOperation)
            }
        }
    }

    fn remove_inner<A: Admission>(
        &self,
        admission: &A,
        receipt: &CreatedWorktree,
        confirmation: CleanupConfirmation,
    ) -> Result<(), WorktreeError> {
        if matches!(confirmation, CleanupConfirmation::None) {
            return Err(WorktreeError::CleanupRefused);
        }
        let executor = self
            .executor
            .as_ref()
            .ok_or(WorktreeError::ExecutorUnavailable)?;
        let journal = self
            .journal
            .as_ref()
            .ok_or(WorktreeError::ExecutorUnavailable)?;
        let requested_budget = ExecutionBudget::from_timeout(MAX_OPERATION_TIMEOUT);
        let cancellation = CancellationToken::new();
        check_budget(&cancellation, requested_budget)?;
        let workspace = self.admitted_workspace(admission)?;
        let budget = workspace
            .execution_lease
            .as_ref()
            .expect("admitted workspace carries execution lease")
            .bound_budget(requested_budget);
        check_budget(&cancellation, budget)?;
        if receipt.scope != workspace.scope || receipt.linked != workspace.linked {
            return Err(WorktreeError::WorkspaceChanged);
        }
        let journal_context = JournalContext::new(&cancellation, budget);
        check_budget(&cancellation, budget)?;
        let operation = JournalOperation {
            kind: JournalKind::Remove,
            key: receipt.operation_id,
        };
        let mut owner = OperationOwner::fresh();
        let plan = WorktreePlan {
            branch: receipt.branch.clone(),
            attempt: 0,
            scope: receipt.scope,
            identity: receipt.identity,
            base_commit: receipt.base_commit,
            base_revision: receipt.base_revision.clone(),
            target: receipt.target.clone(),
            linked: receipt.linked,
            repository: receipt.scope.repository,
        };
        let add_operation = JournalOperation {
            kind: JournalKind::Add,
            key: receipt.operation_id,
        };
        check_budget(&cancellation, budget)?;
        let add_record = journal
            .get(add_operation, journal_context)
            .map_err(map_journal_error)?
            .ok_or(WorktreeError::RecoverableOperation)?;
        check_budget(&cancellation, budget)?;
        if add_record.state != JournalState::Settled
            || add_record.scope != workspace.scope
            || add_record.receipt.as_ref() != Some(receipt)
            || !receipt_matches_plan(receipt, add_operation, &add_record.plan)
        {
            return Err(WorktreeError::WorkspaceChanged);
        }
        let mut intent_version = 0u64;
        let mut reused_refused = false;
        check_budget(&cancellation, budget)?;
        if let Some(existing) = journal
            .get(operation, journal_context)
            .map_err(map_journal_error)?
        {
            check_budget(&cancellation, budget)?;
            if existing.scope != workspace.scope
                || existing.receipt.as_ref() != Some(receipt)
                || existing.plan != plan
            {
                return Err(WorktreeError::WorkspaceChanged);
            }
            if existing.state == JournalState::Settled {
                return Ok(());
            }
            if existing.state == JournalState::Refused {
                intent_version = existing.version;
                owner = existing.owner;
                reused_refused = true;
            } else {
                return Err(WorktreeError::RecoverableOperation);
            }
        }
        check_budget(&cancellation, budget)?;
        journal
            .reserve(
                &workspace.scope,
                &plan.branch,
                operation,
                owner,
                journal_context,
            )
            .map_err(|error| match error {
                JournalError::ReservationBusy => WorktreeError::TargetCollision,
                other => map_journal_error(other),
            })?;
        check_budget(&cancellation, budget).or_else(|error| {
            journal
                .release(
                    &workspace.scope,
                    &plan.branch,
                    operation,
                    owner,
                    journal_context,
                )
                .map_err(map_journal_error)?;
            Err(error)
        })?;
        if !reused_refused {
            if let Err(error) = journal.insert_intent(
                JournalRecord {
                    operation,
                    owner,
                    state: JournalState::Intent,
                    version: 0,
                    scope: workspace.scope,
                    plan: plan.clone(),
                    receipt: Some(receipt.clone()),
                },
                journal_context,
            ) {
                journal
                    .release(
                        &workspace.scope,
                        &plan.branch,
                        operation,
                        owner,
                        journal_context,
                    )
                    .map_err(map_journal_error)?;
                if matches!(error, JournalError::Duplicate) {
                    return Err(WorktreeError::RecoverableOperation);
                }
                return Err(map_journal_error(error));
            }
        }
        if reused_refused {
            intent_version = match journal.update_owned_cas(
                operation,
                owner,
                intent_version,
                JournalState::Refused,
                JournalState::Intent,
                Some(receipt.clone()),
                journal_context,
            ) {
                Ok(record) => record.version,
                Err(_) => return Err(WorktreeError::RecoverableOperation),
            };
        }
        if let Err(error) = revalidate_same(admission, &workspace) {
            transition_and_release(
                journal.as_ref(),
                operation,
                owner,
                intent_version,
                JournalState::Intent,
                JournalState::Recoverable,
                &workspace.scope,
                &plan.branch,
                Some(receipt.clone()),
                journal_context,
            )?;
            return Err(error);
        }
        if let Err(error) = check_budget(&cancellation, budget) {
            transition_and_release(
                journal.as_ref(),
                operation,
                owner,
                intent_version,
                JournalState::Intent,
                JournalState::Aborted,
                &workspace.scope,
                &plan.branch,
                Some(receipt.clone()),
                journal_context,
            )?;
            return Err(error);
        }
        let snapshot = match executor.preview(&workspace, receipt, &cancellation, budget) {
            Ok(snapshot) => snapshot,
            Err(error) => {
                if matches!(error, ExecutorError::NotFound) {
                    // `preview` is only an observation.  An already-absent
                    // target must still be inspected and the live authority
                    // revalidated before its remove intent becomes terminal;
                    // otherwise a swap or lease change in this window could
                    // be recorded as a successful cleanup.
                    let post_preview_cancellation = CancellationToken::new();
                    let post_preview_context =
                        JournalContext::new(&post_preview_cancellation, budget);
                    match executor.inspect(
                        &workspace,
                        operation,
                        &plan,
                        Some(receipt),
                        &post_preview_cancellation,
                        budget,
                    ) {
                        Ok(RecoveryLookup::Absent) => {}
                        Ok(RecoveryLookup::Applied(_)) | Err(_) => {
                            transition_and_release(
                                journal.as_ref(),
                                operation,
                                owner,
                                intent_version,
                                JournalState::Intent,
                                JournalState::Recoverable,
                                &workspace.scope,
                                &plan.branch,
                                Some(receipt.clone()),
                                post_preview_context,
                            )?;
                            return Err(WorktreeError::RecoverableOperation);
                        }
                    }
                    if let Err(error) = revalidate_same(admission, &workspace) {
                        transition_and_release(
                            journal.as_ref(),
                            operation,
                            owner,
                            intent_version,
                            JournalState::Intent,
                            JournalState::Recoverable,
                            &workspace.scope,
                            &plan.branch,
                            Some(receipt.clone()),
                            post_preview_context,
                        )?;
                        return Err(error);
                    }
                    journal
                        .settle_and_release(
                            operation,
                            intent_version,
                            JournalState::Intent,
                            JournalState::Settled,
                            Some(receipt.clone()),
                            &workspace.scope,
                            &plan.branch,
                            owner,
                            post_preview_context,
                        )
                        .map_err(map_journal_error)?;
                    return Ok(());
                }
                transition_and_release(
                    journal.as_ref(),
                    operation,
                    owner,
                    intent_version,
                    JournalState::Intent,
                    JournalState::Recoverable,
                    &workspace.scope,
                    &plan.branch,
                    Some(receipt.clone()),
                    journal_context,
                )?;
                return Err(map_executor_error(error));
            }
        };
        if let Err(error) = check_budget(&cancellation, budget) {
            transition_and_release(
                journal.as_ref(),
                operation,
                owner,
                intent_version,
                JournalState::Intent,
                JournalState::Aborted,
                &workspace.scope,
                &plan.branch,
                Some(receipt.clone()),
                journal_context,
            )?;
            return Err(error);
        }
        if let Err(error) = revalidate_same(admission, &workspace) {
            transition_and_release(
                journal.as_ref(),
                operation,
                owner,
                intent_version,
                JournalState::Intent,
                JournalState::Recoverable,
                &workspace.scope,
                &plan.branch,
                Some(receipt.clone()),
                journal_context,
            )?;
            return Err(error);
        }
        // Ordinary confirmation refuses user dirt. Force is explicit consent
        // for the exact service-owned target and may remove tracked,
        // untracked, or unpushed user state. It never crosses an ownership
        // boundary such as a nested repository, foreign worktree, linked
        // target, or the main checkout.
        let force = matches!(confirmation, CleanupConfirmation::Force);
        if snapshot.ownership_conflict() || (snapshot.user_dirty() && !force) {
            journal
                .settle_and_release(
                    operation,
                    intent_version,
                    JournalState::Intent,
                    JournalState::Refused,
                    Some(receipt.clone()),
                    &workspace.scope,
                    &plan.branch,
                    owner,
                    journal_context,
                )
                .map_err(map_journal_error)?;
            return Err(WorktreeError::CleanupRefused);
        }
        let planned = match journal.update_owned_cas(
            operation,
            owner,
            intent_version,
            JournalState::Intent,
            JournalState::SideEffectApplied,
            Some(receipt.clone()),
            journal_context,
        ) {
            Ok(planned) => planned,
            Err(_) => return Err(WorktreeError::RecoverableOperation),
        };
        if let Err(_) = check_budget(&cancellation, budget) {
            transition_and_release(
                journal.as_ref(),
                operation,
                owner,
                planned.version,
                JournalState::SideEffectApplied,
                JournalState::Recoverable,
                &workspace.scope,
                &plan.branch,
                Some(receipt.clone()),
                journal_context,
            )?;
            return Err(WorktreeError::RecoverableOperation);
        }
        if let Err(_) = revalidate_same(admission, &workspace) {
            transition_and_release(
                journal.as_ref(),
                operation,
                owner,
                planned.version,
                JournalState::SideEffectApplied,
                JournalState::Recoverable,
                &workspace.scope,
                &plan.branch,
                Some(receipt.clone()),
                journal_context,
            )?;
            return Err(WorktreeError::RecoverableOperation);
        }
        let proof = match executor.prove_process_zero(&workspace, receipt, &cancellation, budget) {
            Ok(proof) => proof,
            Err(error) => {
                let next_state = if matches!(error, ExecutorError::ProcessNotZero) {
                    JournalState::Refused
                } else {
                    JournalState::Recoverable
                };
                transition_and_release(
                    journal.as_ref(),
                    operation,
                    owner,
                    planned.version,
                    JournalState::SideEffectApplied,
                    next_state,
                    &workspace.scope,
                    &plan.branch,
                    Some(receipt.clone()),
                    journal_context,
                )?;
                return Err(map_executor_error(error));
            }
        };
        if check_budget(&cancellation, budget).is_err() {
            transition_and_release(
                journal.as_ref(),
                operation,
                owner,
                planned.version,
                JournalState::SideEffectApplied,
                JournalState::Recoverable,
                &workspace.scope,
                &plan.branch,
                Some(receipt.clone()),
                journal_context,
            )?;
            return Err(WorktreeError::RecoverableOperation);
        }
        if proof.identity != workspace.identity
            || proof.zero_observation == 0
            || workspace.process_fence.as_ref() != Some(&proof.fence)
        {
            transition_and_release(
                journal.as_ref(),
                operation,
                owner,
                planned.version,
                JournalState::SideEffectApplied,
                JournalState::Refused,
                &workspace.scope,
                &plan.branch,
                Some(receipt.clone()),
                journal_context,
            )?;
            return Err(WorktreeError::ProcessNotZero);
        }
        if let Err(_) = revalidate_same(admission, &workspace) {
            transition_and_release(
                journal.as_ref(),
                operation,
                owner,
                planned.version,
                JournalState::SideEffectApplied,
                JournalState::Recoverable,
                &workspace.scope,
                &plan.branch,
                Some(receipt.clone()),
                journal_context,
            )?;
            return Err(WorktreeError::RecoverableOperation);
        }
        let post_effect_cancellation = CancellationToken::new();
        let post_effect_context = JournalContext::new(&post_effect_cancellation, budget);
        if let Err(error) =
            executor.remove(&workspace, operation, receipt, proof, &cancellation, budget)
        {
            if !matches!(error, ExecutorError::NotFound) {
                transition_and_release(
                    journal.as_ref(),
                    operation,
                    owner,
                    planned.version,
                    JournalState::SideEffectApplied,
                    JournalState::Recoverable,
                    &workspace.scope,
                    &plan.branch,
                    Some(receipt.clone()),
                    post_effect_context,
                )?;
                return Err(map_executor_error(error));
            }
            // A NotFound result is only a claim that the effect is absent.  It
            // must still pass the same exact identity inspection and authority
            // revalidation as a successful remove before settlement.
        }
        // Removal is not settled until the process-owned executor confirms
        // the exact linked-worktree identity is absent.  A successful Git
        // exit alone is insufficient when the filesystem or admin metadata
        // was swapped concurrently.
        match executor.inspect(
            &workspace,
            operation,
            &plan,
            Some(receipt),
            &post_effect_cancellation,
            budget,
        ) {
            Ok(RecoveryLookup::Absent) => {}
            Ok(RecoveryLookup::Applied(_)) | Err(_) => {
                transition_and_release(
                    journal.as_ref(),
                    operation,
                    owner,
                    planned.version,
                    JournalState::SideEffectApplied,
                    JournalState::Recoverable,
                    &workspace.scope,
                    &plan.branch,
                    Some(receipt.clone()),
                    post_effect_context,
                )?;
                return Err(WorktreeError::RecoverableOperation);
            }
        }
        if check_budget(&cancellation, budget).is_err() {
            transition_and_release(
                journal.as_ref(),
                operation,
                owner,
                planned.version,
                JournalState::SideEffectApplied,
                JournalState::Recoverable,
                &workspace.scope,
                &plan.branch,
                Some(receipt.clone()),
                post_effect_context,
            )?;
            return Err(WorktreeError::RecoverableOperation);
        }
        if revalidate_same(admission, &workspace).is_err() {
            transition_and_release(
                journal.as_ref(),
                operation,
                owner,
                planned.version,
                JournalState::SideEffectApplied,
                JournalState::Recoverable,
                &workspace.scope,
                &plan.branch,
                Some(receipt.clone()),
                post_effect_context,
            )?;
            return Err(WorktreeError::RecoverableOperation);
        }
        let settled = journal
            .settle_and_release(
                operation,
                planned.version,
                JournalState::SideEffectApplied,
                JournalState::Settled,
                Some(receipt.clone()),
                &workspace.scope,
                &plan.branch,
                owner,
                post_effect_context,
            )
            .map_err(map_journal_error)?;
        let _ = settled;
        Ok(())
    }

    fn recover_inner<A: Admission>(
        &self,
        admission: &A,
        cancellation: &CancellationToken,
        budget: ExecutionBudget,
    ) -> Result<RecoveryReport, WorktreeError> {
        let executor = self
            .executor
            .as_ref()
            .ok_or(WorktreeError::ExecutorUnavailable)?;
        let journal = self
            .journal
            .as_ref()
            .ok_or(WorktreeError::ExecutorUnavailable)?;
        check_budget(cancellation, budget)?;
        let workspace = self.admitted_workspace(admission)?;
        let budget = workspace
            .execution_lease
            .as_ref()
            .expect("admitted workspace carries execution lease")
            .bound_budget(budget);
        let journal_context = JournalContext::new(cancellation, budget);
        check_budget(cancellation, budget)?;
        journal
            .reconcile_reservations(&workspace.scope, journal_context)
            .map_err(map_journal_error)?;
        check_budget(cancellation, budget)?;
        let mut report = RecoveryReport {
            recovered: 0,
            tombstones: 0,
        };
        let records = journal
            .records(MAX_JOURNAL_OPERATIONS, journal_context)
            .map_err(map_journal_error)?;
        check_budget(cancellation, budget)?;
        for record in records {
            check_budget(&cancellation, budget)?;
            if !record.scope.stable_eq(&workspace.scope)
                || !record.plan.scope.stable_eq(&workspace.scope)
            {
                // A recovery pass is scoped to the current stable authority.
                // Leave another task/repository untouched; it belongs to a
                // different reconciler and must never be tombstoned here.
                continue;
            }
            if !matches!(
                record.state,
                JournalState::Intent | JournalState::SideEffectApplied
            ) {
                // A crash can occur after a durable terminal transition but
                // before a legacy adapter releases its reservation.  Reconcile
                // only this exact stable scope; never touch another scope's
                // lease or tombstone.
                check_budget(cancellation, budget)?;
                journal
                    .release(
                        &record.scope,
                        &record.plan.branch,
                        record.operation,
                        record.owner,
                        journal_context,
                    )
                    .map_err(map_journal_error)?;
                continue;
            }
            check_budget(cancellation, budget)?;
            let recovery_owner = OperationOwner::fresh();
            let record = match journal.claim_recovery(
                record.operation,
                record.version,
                record.state,
                record.owner,
                recovery_owner,
                journal_context,
            ) {
                Ok(record) => record,
                Err(JournalError::OwnerMismatch | JournalError::CasMismatch) => continue,
                Err(error) => return Err(map_journal_error(error)),
            };
            check_budget(cancellation, budget)?;
            if record.operation.kind == JournalKind::Remove {
                transition_and_release(
                    journal.as_ref(),
                    record.operation,
                    record.owner,
                    record.version,
                    record.state,
                    JournalState::Recoverable,
                    &record.scope,
                    &record.plan.branch,
                    record.receipt.clone(),
                    journal_context,
                )?;
                report.tombstones += 1;
                continue;
            }
            let lookup = match executor.inspect(
                &workspace,
                record.operation,
                &record.plan,
                record.receipt.as_ref(),
                cancellation,
                budget,
            ) {
                Ok(lookup) => lookup,
                Err(_error) => {
                    transition_and_release(
                        journal.as_ref(),
                        record.operation,
                        record.owner,
                        record.version,
                        record.state,
                        JournalState::Recoverable,
                        &record.scope,
                        &record.plan.branch,
                        record.receipt.clone(),
                        journal_context,
                    )?;
                    report.tombstones += 1;
                    continue;
                }
            };
            if check_budget(&cancellation, budget).is_err() {
                transition_and_release(
                    journal.as_ref(),
                    record.operation,
                    record.owner,
                    record.version,
                    record.state,
                    JournalState::Recoverable,
                    &record.scope,
                    &record.plan.branch,
                    record.receipt.clone(),
                    journal_context,
                )?;
                report.tombstones += 1;
                continue;
            }
            match lookup {
                RecoveryLookup::Applied(receipt)
                    if receipt_matches_recovery_plan(&receipt, record.operation, &record.plan) =>
                {
                    let mut version = record.version;
                    if record.state == JournalState::Intent {
                        version = match journal.update_owned_cas(
                            record.operation,
                            record.owner,
                            version,
                            JournalState::Intent,
                            JournalState::SideEffectApplied,
                            Some(receipt.clone()),
                            journal_context,
                        ) {
                            Ok(record) => record.version,
                            Err(_) => return Err(WorktreeError::RecoverableOperation),
                        };
                    }
                    if check_budget(&cancellation, budget).is_err() {
                        transition_and_release(
                            journal.as_ref(),
                            record.operation,
                            record.owner,
                            version,
                            JournalState::SideEffectApplied,
                            JournalState::Recoverable,
                            &record.scope,
                            &record.plan.branch,
                            Some(receipt),
                            journal_context,
                        )?;
                        report.tombstones += 1;
                        continue;
                    }
                    if revalidate_stable(admission, &workspace).is_err() {
                        transition_and_release(
                            journal.as_ref(),
                            record.operation,
                            record.owner,
                            version,
                            JournalState::SideEffectApplied,
                            JournalState::Recoverable,
                            &record.scope,
                            &record.plan.branch,
                            Some(receipt),
                            journal_context,
                        )?;
                        report.tombstones += 1;
                    } else {
                        match journal.settle_and_release(
                            record.operation,
                            version,
                            JournalState::SideEffectApplied,
                            JournalState::Settled,
                            Some(receipt.clone()),
                            &record.scope,
                            &record.plan.branch,
                            record.owner,
                            journal_context,
                        ) {
                            Ok(_) => {}
                            Err(error) => return Err(map_journal_error(error)),
                        }
                        // Settlement atomically releases the reservation.  Do
                        // not perform a post-release validation and attempt a
                        // Settled -> Recoverable transition: that state would
                        // require a new reservation and could orphan a newer
                        // recovery attempt.  All checks that authorize this
                        // transition occur above the atomic boundary.
                        report.recovered += 1;
                    }
                }
                RecoveryLookup::Absent if record.state == JournalState::Intent => {
                    journal
                        .settle_and_release(
                            record.operation,
                            record.version,
                            JournalState::Intent,
                            JournalState::Aborted,
                            None,
                            &record.scope,
                            &record.plan.branch,
                            record.owner,
                            journal_context,
                        )
                        .map_err(map_journal_error)?;
                }
                _ => {
                    transition_and_release(
                        journal.as_ref(),
                        record.operation,
                        record.owner,
                        record.version,
                        record.state,
                        JournalState::Recoverable,
                        &record.scope,
                        &record.plan.branch,
                        record.receipt,
                        journal_context,
                    )?;
                    report.tombstones += 1;
                }
            }
        }
        Ok(report)
    }
}

fn choose_plan<A: Admission>(
    admission: &A,
    executor: &dyn GitWorktreeExecutor,
    journal: &dyn DurableOperationJournal,
    workspace: &ResolvedWorkspace,
    request: &CreateWorktreeRequest,
    operation: JournalOperation,
    owner: OperationOwner,
    budget: ExecutionBudget,
) -> Result<WorktreePlan, WorktreeError> {
    let slug = slugify(&request.label)?;
    let explicit_branch = request.branch.is_some();
    let journal_context = JournalContext::new(&request.cancellation, budget);
    for attempt in 1..=MAX_COLLISION_ATTEMPTS {
        let branch = request.branch.clone().unwrap_or_else(|| {
            if attempt == 1 {
                format!("codex/{slug}")
            } else {
                format!("codex/{slug}-{attempt}")
            }
        });
        validate_branch(&branch)?;
        check_budget(&request.cancellation, budget)?;
        revalidate_same(admission, workspace)?;
        let candidate = WorktreePlan {
            branch: branch.clone(),
            attempt,
            scope: workspace.scope,
            identity: target_identity(&workspace.scope, &branch),
            base_commit: ZERO_FINGERPRINT,
            base_revision: String::new(),
            target: request
                .target
                .clone()
                .unwrap_or_else(WorktreeTarget::placeholder),
            linked: workspace.linked,
            repository: workspace.scope.repository,
        };
        match journal.reserve(&workspace.scope, &branch, operation, owner, journal_context) {
            Ok(()) => {}
            Err(JournalError::ReservationBusy) if explicit_branch => {
                return Err(WorktreeError::BranchCollision);
            }
            Err(JournalError::ReservationBusy | JournalError::Busy) => continue,
            Err(error) => return Err(map_journal_error(error)),
        }
        let probe = match executor.probe(workspace, &candidate, &request.cancellation, budget) {
            Ok(probe) => probe,
            Err(error) => {
                journal
                    .release(&workspace.scope, &branch, operation, owner, journal_context)
                    .map_err(map_journal_error)?;
                return Err(map_executor_error(error));
            }
        };
        if let Err(error) = check_budget(&request.cancellation, budget) {
            journal
                .release(&workspace.scope, &branch, operation, owner, journal_context)
                .map_err(map_journal_error)?;
            return Err(error);
        }
        match probe {
            ProbeResult::Available {
                base_commit,
                base_revision,
                target,
            } => {
                let target_is_bound = workspace
                    .execution_lease
                    .as_ref()
                    .is_some_and(|lease| lease.target_is_bound(&target));
                if !target_is_bound
                    || request
                        .target
                        .as_ref()
                        .is_some_and(|requested| !requested.same_as(&target))
                    || !validate_base_revision(&base_revision)
                {
                    journal
                        .release(&workspace.scope, &branch, operation, owner, journal_context)
                        .map_err(map_journal_error)?;
                    return Err(WorktreeError::WorkspaceChanged);
                }
                return Ok(WorktreePlan {
                    base_commit,
                    base_revision,
                    target,
                    ..candidate
                });
            }
            ProbeResult::Collision if explicit_branch => {
                journal
                    .release(&workspace.scope, &branch, operation, owner, journal_context)
                    .map_err(map_journal_error)?;
                return Err(WorktreeError::BranchCollision);
            }
            ProbeResult::Collision => {
                journal
                    .release(&workspace.scope, &branch, operation, owner, journal_context)
                    .map_err(map_journal_error)?;
            }
        }
    }
    Err(WorktreeError::CollisionBudgetExceeded)
}

fn map_journal_error(error: JournalError) -> WorktreeError {
    match error {
        JournalError::InvalidStore => WorktreeError::JournalFailure,
        JournalError::Full => WorktreeError::JournalFull,
        JournalError::Duplicate
        | JournalError::ReservationMissing
        | JournalError::CasMismatch
        | JournalError::ReservationBusy
        | JournalError::OwnerMismatch
        | JournalError::AtomicSettlementUnavailable
        | JournalError::Busy
        | JournalError::Corrupt => WorktreeError::JournalFailure,
        JournalError::Cancelled => WorktreeError::Cancelled,
        JournalError::Deadline => WorktreeError::Deadline,
        JournalError::OperationInFlight => WorktreeError::OperationInFlight,
    }
}

fn transition_and_release(
    journal: &dyn DurableOperationJournal,
    operation: JournalOperation,
    owner: OperationOwner,
    version: u64,
    expected_state: JournalState,
    next_state: JournalState,
    scope: &WorkspaceScope,
    branch: &str,
    receipt: Option<CreatedWorktree>,
    context: JournalContext<'_>,
) -> Result<(), WorktreeError> {
    let record = journal
        .get(operation, context)
        .map_err(map_journal_error)?
        .ok_or(WorktreeError::JournalFailure)?;
    if record.owner != owner || record.scope != *scope || record.plan.branch != branch {
        return Err(WorktreeError::JournalFailure);
    }
    journal
        .settle_and_release(
            operation,
            version,
            expected_state,
            next_state,
            receipt,
            scope,
            branch,
            owner,
            context,
        )
        .map(|_| ())
        .map_err(map_journal_error)
}

fn target_identity(scope: &WorkspaceScope, branch: &str) -> WorkspaceIdentity {
    let mut hasher = Sha256::new();
    hasher.update(b"devmanager-worktree-target-v2\0");
    hasher.update(scope.workspace.0);
    hasher.update(scope.root);
    hasher.update(scope.repository);
    hasher.update(branch.as_bytes());
    WorkspaceIdentity(hasher.finalize().into())
}

fn revalidate_same<A: Admission>(
    admission: &A,
    expected: &ResolvedWorkspace,
) -> Result<ResolvedWorkspace, WorktreeError> {
    if expected
        .execution_lease
        .as_ref()
        .is_some_and(|lease| !lease.is_live())
    {
        return Err(WorktreeError::StaleAuthority);
    }
    let mut current = admission.revalidate()?;
    current.scope.execution_lease = expected.scope.execution_lease;
    current.execution_lease = expected.execution_lease.clone();
    if let Some(lease) = expected.execution_lease.as_ref() {
        apply_live_process_owner_fence(&mut current, lease)?;
    }
    if current.scope == expected.scope
        && current.identity == expected.identity
        && current.linked == expected.linked
        && current.process_fence == expected.process_fence
    {
        Ok(current)
    } else {
        Err(WorktreeError::WorkspaceChanged)
    }
}

fn revalidate_stable<A: Admission>(
    admission: &A,
    expected: &ResolvedWorkspace,
) -> Result<ResolvedWorkspace, WorktreeError> {
    if expected
        .execution_lease
        .as_ref()
        .is_some_and(|lease| !lease.is_live())
    {
        return Err(WorktreeError::StaleAuthority);
    }
    let mut current = admission.revalidate()?;
    current.scope.execution_lease = expected.scope.execution_lease;
    current.execution_lease = expected.execution_lease.clone();
    if let Some(lease) = expected.execution_lease.as_ref() {
        apply_live_process_owner_fence(&mut current, lease)?;
    }
    if current.scope.stable_eq(&expected.scope)
        && current.identity == expected.identity
        && current.linked == expected.linked
        && current.process_fence == expected.process_fence
    {
        Ok(current)
    } else {
        Err(WorktreeError::WorkspaceChanged)
    }
}

fn request_matches_plan(request: &CreateWorktreeRequest, plan: &WorktreePlan) -> bool {
    if request
        .target
        .as_ref()
        .is_some_and(|target| target != &plan.target)
        || request
            .task_id
            .is_some_and(|task_id| task_id != plan.scope.task_id)
        || request
            .workspace
            .is_some_and(|workspace| workspace != plan.scope.workspace)
    {
        return false;
    }
    if let Some(branch) = &request.branch {
        return branch == &plan.branch && plan.attempt == 1;
    }
    let Ok(slug) = slugify(&request.label) else {
        return false;
    };
    let expected = if plan.attempt == 1 {
        format!("codex/{slug}")
    } else {
        format!("codex/{slug}-{}", plan.attempt)
    };
    expected == plan.branch
}

fn create_plan_matches_scope(plan: &WorktreePlan, scope: &WorkspaceScope) -> bool {
    plan.scope == *scope
        && plan.attempt > 0
        && plan.attempt <= MAX_COLLISION_ATTEMPTS
        && plan.linked == scope.linked
        && plan.repository == scope.repository
        && plan.target.validate()
        && validate_base_revision(&plan.base_revision)
        && plan.identity == target_identity(scope, &plan.branch)
        && validate_branch(&plan.branch).is_ok()
}

fn receipt_matches_plan(
    receipt: &CreatedWorktree,
    operation: JournalOperation,
    plan: &WorktreePlan,
) -> bool {
    receipt.operation_id == operation.key
        && receipt.scope == plan.scope
        && receipt.branch == plan.branch
        && receipt.base_commit == plan.base_commit
        && receipt.base_revision == plan.base_revision
        && receipt.target == plan.target
        && receipt.identity == plan.identity
        && receipt.linked == plan.linked
        && receipt.scope.repository == plan.repository
}

fn receipt_matches_recovery_plan(
    receipt: &CreatedWorktree,
    operation: JournalOperation,
    plan: &WorktreePlan,
) -> bool {
    receipt.operation_id == operation.key
        && receipt.scope.stable_eq(&plan.scope)
        && receipt.branch == plan.branch
        && receipt.base_commit == plan.base_commit
        && receipt.base_revision == plan.base_revision
        && receipt.target == plan.target
        && receipt.identity == plan.identity
        && receipt.linked == plan.linked
        && receipt.scope.repository == plan.repository
}

fn validate_create_request(request: &CreateWorktreeRequest) -> Result<(), WorktreeError> {
    if request.label.trim().is_empty() || request.label.len() > MAX_LABEL_BYTES {
        return Err(WorktreeError::InvalidLabel);
    }
    if request
        .label
        .chars()
        .any(|character| character.is_control())
    {
        return Err(WorktreeError::InvalidLabel);
    }
    if let Some(branch) = &request.branch {
        validate_branch(branch)?;
    }
    Ok(())
}

fn validate_base_revision(revision: &str) -> bool {
    !revision.is_empty()
        && revision.len() <= MAX_BASE_REVISION_BYTES
        && revision.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn validate_branch(branch: &str) -> Result<(), WorktreeError> {
    if branch.is_empty()
        || branch.len() > MAX_BRANCH_BYTES
        || branch.starts_with('.')
        || branch.ends_with('.')
        || branch.starts_with('/')
        || branch.ends_with('/')
        || branch.ends_with(".lock")
        || branch.contains("..")
        || branch.contains("//")
        || branch.contains("@{")
        || branch.contains('\\')
        || branch.chars().any(|character| {
            character.is_control()
                || character.is_whitespace()
                || matches!(character, '~' | '^' | ':' | '?' | '*' | '[')
        })
    {
        return Err(WorktreeError::InvalidBranch);
    }
    Ok(())
}

fn slugify(label: &str) -> Result<String, WorktreeError> {
    let mut slug = String::new();
    for character in label.chars() {
        if character.is_ascii_alphanumeric() {
            slug.push(character.to_ascii_lowercase());
        } else if !slug.ends_with('-') {
            slug.push('-');
        }
    }
    let slug = slug.trim_matches('-');
    if slug.is_empty() || slug.len() > MAX_LABEL_BYTES.saturating_sub(7) {
        return Err(WorktreeError::InvalidLabel);
    }
    Ok(slug.to_string())
}

fn check_budget(
    cancellation: &CancellationToken,
    budget: ExecutionBudget,
) -> Result<(), WorktreeError> {
    if cancellation.is_cancelled() {
        Err(WorktreeError::Cancelled)
    } else if budget.expired() {
        Err(WorktreeError::Deadline)
    } else {
        Ok(())
    }
}

fn map_executor_error(error: ExecutorError) -> WorktreeError {
    match error {
        ExecutorError::Cancelled => WorktreeError::Cancelled,
        ExecutorError::Deadline => WorktreeError::Deadline,
        ExecutorError::Collision => WorktreeError::BranchCollision,
        ExecutorError::IdentityMismatch => WorktreeError::WorkspaceChanged,
        ExecutorError::ProcessNotZero => WorktreeError::ProcessNotZero,
        ExecutorError::InterruptedAfterSideEffect => WorktreeError::Interrupted,
        ExecutorError::CompensationFailed => WorktreeError::RecoverableOperation,
        ExecutorError::NotFound => WorktreeError::AlreadyRemoved,
        ExecutorError::MalformedOutput => WorktreeError::MalformedPorcelain,
        ExecutorError::OversizeOutput => WorktreeError::OversizePorcelain,
    }
}

// ── Focused pure-core fake seams ───────────────────────────────────────────

#[cfg(test)]
#[derive(Clone)]
pub(crate) struct TestWorkspaceAuthorization {
    state: Arc<Mutex<TestAuthorityState>>,
}

#[cfg(test)]
#[derive(Clone)]
pub(crate) struct TestWorkspaceControl {
    state: Arc<Mutex<TestAuthorityState>>,
}

#[cfg(test)]
#[derive(Clone, Copy)]
struct TestAuthorityState {
    active: bool,
    action_epoch: u64,
    runtime_generation: u64,
    identity_revision: u64,
    process_resource_id: ResourceId,
    task_id: TaskId,
    project_id: ProjectId,
    client_id: ClientId,
    connection_id: uuid::Uuid,
    request_id: RequestId,
    command_id: CommandId,
    transient_revision: u64,
}

#[cfg(test)]
fn test_process_fence(state: &TestAuthorityState) -> RegistryManagedProcessFence {
    let id = ManagedProcessId::new(std::process::id(), 1)
        .expect("test process identity has a non-zero creation marker");
    let executable = std::env::current_exe().expect("test executable path");
    let root =
        ManagedProcessIdentity::new(id, executable).expect("test executable canonicalization");
    RegistryManagedProcessFence::new(
        ResourceFence::new(state.process_resource_id, state.runtime_generation),
        ProcessOwner::Task(state.task_id),
        root,
    )
}

#[cfg(test)]
#[test]
fn apply_live_process_owner_fence_leaves_test_admission_fence_intact() {
    let (authorization, _control) = TestWorkspaceAuthorization::new();
    let mut workspace = authorization.revalidate().expect("test admission");
    let expected_fence = workspace.process_fence.clone();
    let lease = WorktreeExecutionLease::for_test();
    apply_live_process_owner_fence(&mut workspace, &lease).expect("test authority");
    assert_eq!(
        workspace.process_fence, expected_fence,
        "Test authority must keep the admission-supplied process fence"
    );
}

#[cfg(test)]
impl fmt::Debug for TestWorkspaceAuthorization {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("TestWorkspaceAuthorization(REDACTED)")
    }
}

#[cfg(test)]
impl fmt::Debug for TestWorkspaceControl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("TestWorkspaceControl(REDACTED)")
    }
}

#[cfg(test)]
impl TestWorkspaceAuthorization {
    pub(crate) fn new() -> (Self, TestWorkspaceControl) {
        let state = Arc::new(Mutex::new(TestAuthorityState {
            active: true,
            action_epoch: 0,
            runtime_generation: 0,
            identity_revision: 0,
            process_resource_id: ResourceId::new(),
            task_id: TaskId::new(),
            project_id: ProjectId::new(),
            client_id: ClientId::new(),
            connection_id: uuid::Uuid::now_v7(),
            request_id: RequestId::new(),
            command_id: CommandId::new(),
            transient_revision: 0,
        }));
        (
            Self {
                state: Arc::clone(&state),
            },
            TestWorkspaceControl { state },
        )
    }

    pub(crate) fn control(&self) -> TestWorkspaceControl {
        TestWorkspaceControl {
            state: Arc::clone(&self.state),
        }
    }
}

#[cfg(test)]
impl Admission for TestWorkspaceAuthorization {
    fn revalidate(&self) -> Result<ResolvedWorkspace, WorktreeError> {
        let state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if !state.active || state.action_epoch != 0 || state.runtime_generation != 0 {
            return Err(WorktreeError::StaleAuthority);
        }
        if state.identity_revision != 0 {
            return Err(WorktreeError::WorkspaceChanged);
        }
        let identity = WorkspaceIdentity(digest_with_tag(
            b"test-workspace",
            &state.runtime_generation.to_le_bytes(),
        ));
        let linked = LinkedWorktreeIdentity {
            gitdir: digest_with_tag(b"gitdir", &identity.0),
            commondir: digest_with_tag(b"commondir", &identity.0),
            backreference: digest_with_tag(b"backreference", &identity.0),
            repository: digest_with_tag(b"repository", &identity.0),
        };
        let scope = WorkspaceScope {
            task_id: state.task_id,
            project_id: state.project_id,
            workspace: identity,
            client_id: state.client_id,
            connection_id: state.connection_id,
            request_id: state.request_id,
            command_id: state.command_id,
            action_epoch: state.action_epoch,
            runtime_generation: state.runtime_generation,
            process_epoch: state.runtime_generation,
            root: digest_with_tag(b"test-root", &identity.0),
            repository: linked.repository,
            process_owner: *state.task_id.as_bytes(),
            execution_lease: [0; 16],
            linked,
        };
        Ok(ResolvedWorkspace {
            identity,
            linked,
            scope,
            process_fence: Some(test_process_fence(&state)),
            execution_lease: None,
        })
    }
}

#[cfg(test)]
impl TestWorkspaceControl {
    fn mutate(&self, f: impl FnOnce(&mut TestAuthorityState)) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        f(&mut state);
    }

    pub(crate) fn bump_action_epoch(&self) {
        self.mutate(|state| state.action_epoch = 1);
    }

    pub(crate) fn restore_action_epoch(&self) {
        self.mutate(|state| state.action_epoch = 0);
    }

    pub(crate) fn bump_runtime_generation(&self) {
        self.mutate(|state| state.runtime_generation = 1);
    }

    pub(crate) fn restore_runtime_generation(&self) {
        self.mutate(|state| state.runtime_generation = 0);
    }

    pub(crate) fn revoke_lease(&self) {
        self.mutate(|state| state.active = false);
    }

    pub(crate) fn bump_transient_scope(&self) {
        self.mutate(|state| {
            state.transient_revision = state.transient_revision.saturating_add(1);
            state.request_id = RequestId::new();
            state.command_id = CommandId::new();
        });
    }

    pub(crate) fn replace_root_identity(&self) {
        self.mutate(|state| state.identity_revision = 1);
    }

    pub(crate) fn replace_ancestor_identity(&self) {
        self.mutate(|state| state.identity_revision = 2);
    }

    pub(crate) fn replace_reparse_identity(&self) {
        self.mutate(|state| state.identity_revision = 3);
    }

    pub(crate) fn replace_acl_identity(&self) {
        self.mutate(|state| state.identity_revision = 4);
    }
}

#[cfg(test)]
#[derive(Clone)]
pub(crate) struct TestOperationJournal {
    records: Arc<Mutex<BTreeMap<JournalOperation, JournalRecord>>>,
    reservations: Arc<Mutex<BTreeMap<ReservationKey, ReservationLease>>>,
    post_settlement_control: Arc<Mutex<Option<TestWorkspaceControl>>>,
}

#[cfg(test)]
impl fmt::Debug for TestOperationJournal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("TestOperationJournal(REDACTED)")
    }
}

#[cfg(test)]
impl TestOperationJournal {
    pub(crate) fn new() -> Self {
        Self {
            records: Arc::new(Mutex::new(BTreeMap::new())),
            reservations: Arc::new(Mutex::new(BTreeMap::new())),
            post_settlement_control: Arc::new(Mutex::new(None)),
        }
    }

    pub(crate) fn invalidate_after_settlement(&self, control: TestWorkspaceControl) {
        *self
            .post_settlement_control
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(control);
    }

    pub(crate) fn records(&self) -> Vec<JournalRecordView> {
        self.records
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .values()
            .map(|record| JournalRecordView {
                state: record.state,
            })
            .collect()
    }

    pub(crate) fn reservation_count(&self) -> usize {
        self.reservations
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .len()
    }
}

#[cfg(test)]
impl Default for TestOperationJournal {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
impl sealed::Journal for TestOperationJournal {}

#[cfg(test)]
impl DurableOperationJournal for TestOperationJournal {
    fn insert_intent(
        &self,
        record: JournalRecord,
        context: JournalContext<'_>,
    ) -> Result<(), JournalError> {
        context.check()?;
        if record.version != 0
            || record.state != JournalState::Intent
            || record.scope.execution_lease == [0; 16]
            || record.scope.process_owner == [0; 16]
            || record.plan.scope != record.scope
            || record.plan.branch.is_empty()
            || record.plan.branch.len() > MAX_BRANCH_BYTES
            || !validate_base_revision(&record.plan.base_revision)
            || !record.plan.target.validate()
            || record.receipt.as_ref().is_some_and(|receipt| {
                !receipt_matches_plan(receipt, record.operation, &record.plan)
            })
        {
            return Err(JournalError::Corrupt);
        }
        let mut records = self
            .records
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if !records.contains_key(&record.operation) && records.len() >= MAX_JOURNAL_OPERATIONS {
            return Err(JournalError::Full);
        }
        if records.contains_key(&record.operation) {
            return Err(JournalError::Duplicate);
        }
        let key = ReservationKey {
            workspace: record.scope.workspace,
            repository: record.scope.repository,
            branch: record.plan.branch.clone(),
        };
        let reservations = self
            .reservations
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if reservations.get(&key).copied()
            != Some(ReservationLease {
                operation: record.operation,
                owner: record.owner,
            })
        {
            return Err(JournalError::ReservationMissing);
        }
        records.insert(record.operation, record);
        Ok(())
    }

    fn update_owned_cas(
        &self,
        operation: JournalOperation,
        owner: OperationOwner,
        expected_version: u64,
        expected_state: JournalState,
        next_state: JournalState,
        receipt: Option<CreatedWorktree>,
        context: JournalContext<'_>,
    ) -> Result<JournalRecord, JournalError> {
        context.check()?;
        let mut records = self
            .records
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let record = records
            .get_mut(&operation)
            .ok_or(JournalError::CasMismatch)?;
        if record.owner != owner {
            return Err(JournalError::OwnerMismatch);
        }
        if record.version != expected_version || record.state != expected_state {
            return Err(JournalError::CasMismatch);
        }
        if !valid_state_transition(record.state, next_state) {
            return Err(JournalError::CasMismatch);
        }
        if receipt
            .as_ref()
            .is_some_and(|receipt| !receipt_matches_plan(receipt, operation, &record.plan))
        {
            return Err(JournalError::Corrupt);
        }
        record.state = next_state;
        record.version = record.version.checked_add(1).ok_or(JournalError::Corrupt)?;
        record.receipt = receipt;
        Ok(record.clone())
    }

    fn get(
        &self,
        operation: JournalOperation,
        context: JournalContext<'_>,
    ) -> Result<Option<JournalRecord>, JournalError> {
        context.check()?;
        Ok(self
            .records
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&operation)
            .cloned())
    }

    fn records(
        &self,
        limit: usize,
        context: JournalContext<'_>,
    ) -> Result<Vec<JournalRecord>, JournalError> {
        context.check()?;
        let records = self
            .records
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut result = Vec::with_capacity(limit.min(MAX_JOURNAL_OPERATIONS));
        for record in records.values().take(limit.min(MAX_JOURNAL_OPERATIONS)) {
            context.check()?;
            result.push(record.clone());
        }
        Ok(result)
    }

    fn reconcile_reservations(
        &self,
        scope: &WorkspaceScope,
        context: JournalContext<'_>,
    ) -> Result<(), JournalError> {
        context.check()?;
        let records = self
            .records
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut reservations = self
            .reservations
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let keys = reservations.keys().cloned().collect::<Vec<_>>();
        for key in keys {
            context.check()?;
            if key.workspace == scope.workspace
                && key.repository == scope.repository
                && reservations.get(&key).is_some_and(|lease| {
                    !records.values().any(|record| {
                        record.operation == lease.operation
                            && record.scope.stable_eq(scope)
                            && record.plan.branch == key.branch
                    })
                })
            {
                reservations.remove(&key);
            }
        }
        Ok(())
    }

    fn reserve(
        &self,
        scope: &WorkspaceScope,
        branch: &str,
        operation: JournalOperation,
        owner: OperationOwner,
        context: JournalContext<'_>,
    ) -> Result<(), JournalError> {
        context.check()?;
        let key = ReservationKey {
            workspace: scope.workspace,
            repository: scope.repository,
            branch: branch.to_string(),
        };
        let mut reservations = self
            .reservations
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match reservations.get(&key) {
            Some(lease) if lease.operation == operation && lease.owner != owner => {
                Err(JournalError::OperationInFlight)
            }
            Some(lease) if lease.operation != operation => Err(JournalError::ReservationBusy),
            _ => {
                if !reservations.contains_key(&key) && reservations.len() >= MAX_JOURNAL_OPERATIONS
                {
                    return Err(JournalError::Full);
                }
                reservations.insert(key, ReservationLease { operation, owner });
                Ok(())
            }
        }
    }

    fn release(
        &self,
        scope: &WorkspaceScope,
        branch: &str,
        operation: JournalOperation,
        owner: OperationOwner,
        context: JournalContext<'_>,
    ) -> Result<(), JournalError> {
        context.check()?;
        let key = ReservationKey {
            workspace: scope.workspace,
            repository: scope.repository,
            branch: branch.to_string(),
        };
        let mut reservations = self
            .reservations
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match reservations.get(&key).copied() {
            None => Ok(()),
            Some(ReservationLease {
                operation: current_operation,
                owner: current_owner,
            }) if current_operation == operation && current_owner == owner => {
                reservations.remove(&key);
                Ok(())
            }
            Some(_) => Err(JournalError::OwnerMismatch),
        }
    }

    fn settle_and_release(
        &self,
        operation: JournalOperation,
        expected_version: u64,
        expected_state: JournalState,
        next_state: JournalState,
        receipt: Option<CreatedWorktree>,
        scope: &WorkspaceScope,
        branch: &str,
        owner: OperationOwner,
        context: JournalContext<'_>,
    ) -> Result<JournalRecord, JournalError> {
        context.check()?;
        let key = ReservationKey {
            workspace: scope.workspace,
            repository: scope.repository,
            branch: branch.to_string(),
        };
        let mut records = self
            .records
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let current = records
            .get(&operation)
            .cloned()
            .ok_or(JournalError::CasMismatch)?;
        if current.version != expected_version || current.state != expected_state {
            return Err(JournalError::CasMismatch);
        }
        if current.scope != *scope || current.plan.branch != branch || current.owner != owner {
            return Err(JournalError::CasMismatch);
        }
        if !valid_state_transition(current.state, next_state) {
            return Err(JournalError::CasMismatch);
        }
        if receipt
            .as_ref()
            .is_some_and(|receipt| !receipt_matches_plan(receipt, operation, &current.plan))
        {
            return Err(JournalError::Corrupt);
        }
        let mut reservations = self
            .reservations
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if reservations.get(&key).copied() != Some(ReservationLease { operation, owner }) {
            return Err(JournalError::OwnerMismatch);
        }
        let mut settled = current;
        settled.state = next_state;
        settled.version = settled
            .version
            .checked_add(1)
            .ok_or(JournalError::Corrupt)?;
        settled.receipt = receipt;
        records.insert(operation, settled.clone());
        reservations.remove(&key);
        drop(reservations);
        drop(records);
        if let Some(control) = self
            .post_settlement_control
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
        {
            control.bump_action_epoch();
        }
        Ok(settled)
    }

    fn claim_recovery(
        &self,
        operation: JournalOperation,
        expected_version: u64,
        expected_state: JournalState,
        expected_owner: OperationOwner,
        new_owner: OperationOwner,
        context: JournalContext<'_>,
    ) -> Result<JournalRecord, JournalError> {
        context.check()?;
        let mut records = self
            .records
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let current = records
            .get_mut(&operation)
            .ok_or(JournalError::CasMismatch)?
            .clone();
        if current.version != expected_version
            || current.state != expected_state
            || current.owner != expected_owner
        {
            return Err(JournalError::OwnerMismatch);
        }
        let key = ReservationKey {
            workspace: current.scope.workspace,
            repository: current.scope.repository,
            branch: current.plan.branch.clone(),
        };
        let mut reservations = self
            .reservations
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match reservations.get(&key).copied() {
            Some(ReservationLease {
                operation: current_operation,
                owner: current_owner,
            }) if current_operation == operation && current_owner == expected_owner => {}
            None => {}
            Some(_) => return Err(JournalError::OwnerMismatch),
        }
        if !reservations.contains_key(&key) && reservations.len() >= MAX_JOURNAL_OPERATIONS {
            return Err(JournalError::Full);
        }
        let record = records
            .get_mut(&operation)
            .ok_or(JournalError::CasMismatch)?;
        record.owner = new_owner;
        record.version = record.version.checked_add(1).ok_or(JournalError::Corrupt)?;
        if let Some(lease) = reservations.get_mut(&key) {
            lease.owner = new_owner;
        } else {
            reservations.insert(
                key,
                ReservationLease {
                    operation,
                    owner: new_owner,
                },
            );
        }
        Ok(record.clone())
    }
}

const MAX_JOURNAL_RECORD_BYTES: usize = 256 * 1024;

#[derive(Serialize, Deserialize)]
struct DiskLinkedWorktreeIdentity {
    gitdir: [u8; 32],
    commondir: [u8; 32],
    backreference: [u8; 32],
    repository: [u8; 32],
}

#[derive(Serialize, Deserialize)]
struct DiskWorkspaceScope {
    task_id: TaskId,
    project_id: ProjectId,
    workspace: [u8; 32],
    client_id: ClientId,
    connection_id: uuid::Uuid,
    request_id: RequestId,
    command_id: CommandId,
    action_epoch: u64,
    runtime_generation: u64,
    process_epoch: u64,
    root: [u8; 32],
    repository: [u8; 32],
    process_owner: [u8; 16],
    #[serde(default)]
    execution_lease: [u8; 16],
    linked: DiskLinkedWorktreeIdentity,
}

#[derive(Serialize, Deserialize)]
struct DiskWorktreePlan {
    branch: String,
    attempt: usize,
    scope: DiskWorkspaceScope,
    identity: [u8; 32],
    base_commit: [u8; 32],
    #[serde(default)]
    base_revision: String,
    #[serde(default)]
    target: Option<DiskWorktreeTarget>,
    linked: DiskLinkedWorktreeIdentity,
    repository: [u8; 32],
}

#[derive(Serialize, Deserialize)]
struct DiskWorktreeTarget {
    approved_root: String,
    path: String,
    approved_root_identity: [u8; 32],
}

#[derive(Serialize, Deserialize)]
struct DiskCreatedWorktree {
    operation_id: [u8; 16],
    scope: DiskWorkspaceScope,
    branch: String,
    base_commit: [u8; 32],
    #[serde(default)]
    base_revision: String,
    #[serde(default)]
    target: Option<DiskWorktreeTarget>,
    identity: [u8; 32],
    linked: DiskLinkedWorktreeIdentity,
}

#[derive(Serialize, Deserialize)]
struct DiskJournalRecord {
    operation_kind: u8,
    operation_key: [u8; 16],
    owner: [u8; 16],
    state: u8,
    version: u64,
    scope: DiskWorkspaceScope,
    plan: DiskWorktreePlan,
    receipt: Option<DiskCreatedWorktree>,
}

fn disk_linked(linked: LinkedWorktreeIdentity) -> DiskLinkedWorktreeIdentity {
    DiskLinkedWorktreeIdentity {
        gitdir: linked.gitdir,
        commondir: linked.commondir,
        backreference: linked.backreference,
        repository: linked.repository,
    }
}

fn linked_from_disk(linked: DiskLinkedWorktreeIdentity) -> LinkedWorktreeIdentity {
    LinkedWorktreeIdentity {
        gitdir: linked.gitdir,
        commondir: linked.commondir,
        backreference: linked.backreference,
        repository: linked.repository,
    }
}

fn disk_target(target: &WorktreeTarget) -> DiskWorktreeTarget {
    DiskWorktreeTarget {
        approved_root: target.approved_root.to_string_lossy().into_owned(),
        path: target.path.to_string_lossy().into_owned(),
        approved_root_identity: target.approved_root_identity,
    }
}

fn target_from_disk(target: Option<DiskWorktreeTarget>) -> Result<WorktreeTarget, JournalError> {
    let Some(DiskWorktreeTarget {
        approved_root,
        path,
        approved_root_identity,
    }) = target
    else {
        return Err(JournalError::Corrupt);
    };
    if approved_root.is_empty()
        || path.is_empty()
        || approved_root.len() > MAX_TARGET_PATH_BYTES
        || path.len() > MAX_TARGET_PATH_BYTES
    {
        return Err(JournalError::Corrupt);
    }
    let target = WorktreeTarget {
        approved_root: PathBuf::from(approved_root),
        path: PathBuf::from(path),
        approved_root_identity,
    };
    target
        .validate()
        .then_some(target)
        .ok_or(JournalError::Corrupt)
}

fn disk_scope(scope: WorkspaceScope) -> DiskWorkspaceScope {
    DiskWorkspaceScope {
        task_id: scope.task_id,
        project_id: scope.project_id,
        workspace: scope.workspace.0,
        client_id: scope.client_id,
        connection_id: scope.connection_id,
        request_id: scope.request_id,
        command_id: scope.command_id,
        action_epoch: scope.action_epoch,
        runtime_generation: scope.runtime_generation,
        process_epoch: scope.process_epoch,
        root: scope.root,
        repository: scope.repository,
        process_owner: scope.process_owner,
        execution_lease: scope.execution_lease,
        linked: disk_linked(scope.linked),
    }
}

fn scope_from_disk(scope: DiskWorkspaceScope) -> WorkspaceScope {
    WorkspaceScope {
        task_id: scope.task_id,
        project_id: scope.project_id,
        workspace: WorkspaceIdentity(scope.workspace),
        client_id: scope.client_id,
        connection_id: scope.connection_id,
        request_id: scope.request_id,
        command_id: scope.command_id,
        action_epoch: scope.action_epoch,
        runtime_generation: scope.runtime_generation,
        process_epoch: scope.process_epoch,
        root: scope.root,
        repository: scope.repository,
        process_owner: scope.process_owner,
        execution_lease: scope.execution_lease,
        linked: linked_from_disk(scope.linked),
    }
}

fn disk_plan(plan: &WorktreePlan) -> DiskWorktreePlan {
    DiskWorktreePlan {
        branch: plan.branch.clone(),
        attempt: plan.attempt,
        scope: disk_scope(plan.scope),
        identity: plan.identity.0,
        base_commit: plan.base_commit,
        base_revision: plan.base_revision.clone(),
        target: Some(disk_target(&plan.target)),
        linked: disk_linked(plan.linked),
        repository: plan.repository,
    }
}

fn plan_from_disk(plan: DiskWorktreePlan) -> Result<WorktreePlan, JournalError> {
    if plan.branch.is_empty()
        || plan.branch.len() > MAX_BRANCH_BYTES
        || !validate_base_revision(&plan.base_revision)
    {
        return Err(JournalError::Corrupt);
    }
    let scope = scope_from_disk(plan.scope);
    let linked = linked_from_disk(plan.linked);
    let target = target_from_disk(plan.target)?;
    if plan.repository != scope.repository || linked != scope.linked {
        return Err(JournalError::Corrupt);
    }
    Ok(WorktreePlan {
        branch: plan.branch,
        attempt: plan.attempt,
        scope,
        identity: WorkspaceIdentity(plan.identity),
        base_commit: plan.base_commit,
        base_revision: plan.base_revision,
        target,
        linked,
        repository: plan.repository,
    })
}

fn disk_receipt(receipt: &CreatedWorktree) -> DiskCreatedWorktree {
    DiskCreatedWorktree {
        operation_id: receipt.operation_id.0,
        scope: disk_scope(receipt.scope),
        branch: receipt.branch.clone(),
        base_commit: receipt.base_commit,
        base_revision: receipt.base_revision.clone(),
        target: Some(disk_target(&receipt.target)),
        identity: receipt.identity.0,
        linked: disk_linked(receipt.linked),
    }
}

fn receipt_from_disk(receipt: DiskCreatedWorktree) -> Result<CreatedWorktree, JournalError> {
    if receipt.branch.is_empty()
        || receipt.branch.len() > MAX_BRANCH_BYTES
        || !validate_base_revision(&receipt.base_revision)
    {
        return Err(JournalError::Corrupt);
    }
    let target = target_from_disk(receipt.target)?;
    Ok(CreatedWorktree {
        operation_id: OperationKey(receipt.operation_id),
        scope: scope_from_disk(receipt.scope),
        branch: receipt.branch,
        base_commit: receipt.base_commit,
        base_revision: receipt.base_revision,
        target,
        identity: WorkspaceIdentity(receipt.identity),
        linked: linked_from_disk(receipt.linked),
    })
}

fn disk_record(record: &JournalRecord) -> DiskJournalRecord {
    DiskJournalRecord {
        operation_kind: match record.operation.kind {
            JournalKind::Add => 0,
            JournalKind::Remove => 1,
        },
        operation_key: record.operation.key.0,
        owner: record.owner.0,
        state: match record.state {
            JournalState::Intent => 0,
            JournalState::SideEffectApplied => 1,
            JournalState::Settled => 2,
            JournalState::Compensated => 3,
            JournalState::Aborted => 4,
            JournalState::Refused => 5,
            JournalState::Recoverable => 6,
        },
        version: record.version,
        scope: disk_scope(record.scope),
        plan: disk_plan(&record.plan),
        receipt: record.receipt.as_ref().map(disk_receipt),
    }
}

fn record_from_disk(record: DiskJournalRecord) -> Result<JournalRecord, JournalError> {
    let kind = match record.operation_kind {
        0 => JournalKind::Add,
        1 => JournalKind::Remove,
        _ => return Err(JournalError::Corrupt),
    };
    let state = match record.state {
        0 => JournalState::Intent,
        1 => JournalState::SideEffectApplied,
        2 => JournalState::Settled,
        3 => JournalState::Compensated,
        4 => JournalState::Aborted,
        5 => JournalState::Refused,
        6 => JournalState::Recoverable,
        _ => return Err(JournalError::Corrupt),
    };
    let scope = scope_from_disk(record.scope);
    if scope.execution_lease == [0; 16] || scope.process_owner == [0; 16] {
        return Err(JournalError::Corrupt);
    }
    let plan = plan_from_disk(record.plan)?;
    if plan.scope != scope {
        return Err(JournalError::Corrupt);
    }
    let receipt = record.receipt.map(receipt_from_disk).transpose()?;
    let operation = JournalOperation {
        kind,
        key: OperationKey(record.operation_key),
    };
    if kind == JournalKind::Add && !create_plan_matches_scope(&plan, &scope) {
        return Err(JournalError::Corrupt);
    }
    if kind == JournalKind::Remove {
        let Some(remove_receipt) = receipt.as_ref() else {
            return Err(JournalError::Corrupt);
        };
        let expected_plan = WorktreePlan {
            branch: remove_receipt.branch.clone(),
            attempt: 0,
            scope: remove_receipt.scope,
            identity: remove_receipt.identity,
            base_commit: remove_receipt.base_commit,
            base_revision: remove_receipt.base_revision.clone(),
            target: remove_receipt.target.clone(),
            linked: remove_receipt.linked,
            repository: remove_receipt.scope.repository,
        };
        if plan != expected_plan || !receipt_matches_plan(remove_receipt, operation, &plan) {
            return Err(JournalError::Corrupt);
        }
    }
    if let Some(receipt) = &receipt {
        if receipt.operation_id != operation.key
            || receipt.scope != scope
            || receipt.branch != plan.branch
            || receipt.base_commit != plan.base_commit
            || receipt.base_revision != plan.base_revision
            || receipt.target != plan.target
            || receipt.identity != plan.identity
            || receipt.linked != plan.linked
        {
            return Err(JournalError::Corrupt);
        }
    }
    Ok(JournalRecord {
        operation,
        owner: OperationOwner(record.owner),
        state,
        version: record.version,
        scope,
        plan,
        receipt,
    })
}

fn encode_record(record: &JournalRecord) -> Result<Vec<u8>, JournalError> {
    let encoded = serde_json::to_vec(&disk_record(record)).map_err(|_| JournalError::Corrupt)?;
    if encoded.len() > MAX_JOURNAL_RECORD_BYTES {
        Err(JournalError::Corrupt)
    } else {
        Ok(encoded)
    }
}

fn decode_record(encoded: &[u8]) -> Result<JournalRecord, JournalError> {
    if encoded.len() > MAX_JOURNAL_RECORD_BYTES {
        return Err(JournalError::Corrupt);
    }
    let record: DiskJournalRecord =
        serde_json::from_slice(encoded).map_err(|_| JournalError::Corrupt)?;
    record_from_disk(record)
}

fn sqlite_operation_kind(kind: JournalKind) -> i64 {
    match kind {
        JournalKind::Add => 0,
        JournalKind::Remove => 1,
    }
}

fn sqlite_error(error: rusqlite::Error) -> JournalError {
    if matches!(
        error,
        rusqlite::Error::SqliteFailure(ref code, _)
            if matches!(
                code.code,
                rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked
            )
    ) {
        JournalError::Busy
    } else {
        JournalError::Corrupt
    }
}

#[derive(Clone)]
pub(crate) struct SqliteWorktreeJournal {
    store: Arc<WorktreeJournalStore>,
}

impl fmt::Debug for SqliteWorktreeJournal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SqliteWorktreeJournal(REDACTED)")
    }
}

impl SqliteWorktreeJournal {
    fn from_store(store: Arc<WorktreeJournalStore>) -> Result<Self, JournalError> {
        let journal = Self { store };
        journal.initialize()?;
        Ok(journal)
    }

    fn validate_store_path(&self) -> Result<(), JournalError> {
        self.store
            .handle
            .metadata()
            .map_err(|_| JournalError::InvalidStore)?;
        let metadata =
            std::fs::symlink_metadata(&*self.store.path).map_err(|_| JournalError::InvalidStore)?;
        if !metadata.is_file() || metadata.file_type().is_symlink() {
            return Err(JournalError::InvalidStore);
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt;
            use windows::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;
            if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0 {
                return Err(JournalError::InvalidStore);
            }
        }
        let path_handle = File::open(&*self.store.path).map_err(|_| JournalError::InvalidStore)?;
        let path_identity =
            retained_file_identity(&path_handle).ok_or(JournalError::InvalidStore)?;
        if path_identity != self.store.handle_identity {
            return Err(JournalError::InvalidStore);
        }
        Ok(())
    }

    fn initialize(&self) -> Result<(), JournalError> {
        self.validate_store_path()?;
        let path = &*self.store.path;
        let connection = rusqlite::Connection::open(path).map_err(sqlite_error)?;
        self.validate_store_path()?;
        connection
            .busy_timeout(Duration::from_secs(2))
            .map_err(sqlite_error)?;
        self.validate_store_path()?;
        connection
            .execute_batch(
                "PRAGMA journal_mode = WAL;
                 PRAGMA synchronous = FULL;
                 CREATE TABLE IF NOT EXISTS worktree_journal (
                     operation_kind INTEGER NOT NULL,
                     operation_key BLOB NOT NULL,
                     version INTEGER NOT NULL,
                     state INTEGER NOT NULL,
                     payload BLOB NOT NULL,
                     PRIMARY KEY (operation_kind, operation_key)
                 );
                 CREATE TABLE IF NOT EXISTS worktree_reservation (
                     workspace BLOB NOT NULL,
                     repository BLOB NOT NULL,
                     branch TEXT NOT NULL,
                     operation_kind INTEGER NOT NULL,
                     operation_key BLOB NOT NULL,
                     owner BLOB NOT NULL,
                     PRIMARY KEY (workspace, repository, branch)
                 );
                 CREATE TABLE IF NOT EXISTS worktree_journal_meta (
                     id INTEGER PRIMARY KEY CHECK (id = 1),
                     store_identity BLOB NOT NULL
                 );",
            )
            .map_err(sqlite_error)?;
        connection
            .execute(
                "INSERT OR IGNORE INTO worktree_journal_meta (id, store_identity)
                 VALUES (1, ?1)",
                rusqlite::params![self.store.identity.to_vec()],
            )
            .map_err(sqlite_error)?;
        let persisted: Vec<u8> = connection
            .query_row(
                "SELECT store_identity FROM worktree_journal_meta WHERE id = 1",
                [],
                |row| row.get(0),
            )
            .map_err(sqlite_error)?;
        if persisted.as_slice() != self.store.identity.as_slice() {
            return Err(JournalError::InvalidStore);
        }
        Ok(())
    }

    /// Every journal instance opens its own bounded connection.  Correctness
    /// therefore comes from SQLite transactions/CAS rather than an
    /// in-process mutex, including after reconnect or across service
    /// instances.
    fn connection(
        &self,
        context: JournalContext<'_>,
    ) -> Result<rusqlite::Connection, JournalError> {
        context.check()?;
        self.validate_store_path()?;
        let connection = rusqlite::Connection::open(&*self.store.path).map_err(sqlite_error)?;
        // The path may have been replaced between the pre-open check and
        // SQLite's resolution of the filename. Validate the retained file
        // identity before any connection PRAGMA can create or switch WAL
        // state on an unapproved replacement.
        self.validate_store_path()?;
        connection
            .busy_timeout(context.remaining().min(Duration::from_secs(2)))
            .map_err(sqlite_error)?;
        self.validate_store_path()?;
        connection
            .execute_batch(
                "PRAGMA journal_mode = WAL;
                 PRAGMA synchronous = FULL;",
            )
            .map_err(sqlite_error)?;
        // Recheck after opening: a replacement between the pre-open check and
        // SQLite's path resolution must never turn a retained lease into a
        // path-authority lease.
        self.validate_store_path()?;
        Ok(connection)
    }

    fn row_to_record(
        operation_kind: i64,
        operation_key: Vec<u8>,
        version: i64,
        state: i64,
        payload: Vec<u8>,
    ) -> Result<JournalRecord, JournalError> {
        if operation_kind != 0 && operation_kind != 1
            || operation_key.len() != 16
            || version < 0
            || state < 0
        {
            return Err(JournalError::Corrupt);
        }
        let record = decode_record(&payload)?;
        let expected_kind = match operation_kind {
            0 => JournalKind::Add,
            1 => JournalKind::Remove,
            _ => return Err(JournalError::Corrupt),
        };
        let expected_key: [u8; 16] = operation_key
            .try_into()
            .map_err(|_| JournalError::Corrupt)?;
        if record.operation.kind != expected_kind
            || record.operation.key != OperationKey(expected_key)
            || record.version != version as u64
            || record.state as i64 != state
        {
            return Err(JournalError::Corrupt);
        }
        Ok(record)
    }
}

impl sealed::Journal for SqliteWorktreeJournal {}

impl DurableOperationJournal for SqliteWorktreeJournal {
    fn insert_intent(
        &self,
        record: JournalRecord,
        context: JournalContext<'_>,
    ) -> Result<(), JournalError> {
        context.check()?;
        if record.version != 0
            || record.state != JournalState::Intent
            || record.scope.execution_lease == [0; 16]
            || record.scope.process_owner == [0; 16]
            || record.plan.scope != record.scope
            || record.plan.branch.is_empty()
            || record.plan.branch.len() > MAX_BRANCH_BYTES
            || !validate_base_revision(&record.plan.base_revision)
            || !record.plan.target.validate()
            || record.receipt.as_ref().is_some_and(|receipt| {
                !receipt_matches_plan(receipt, record.operation, &record.plan)
            })
        {
            return Err(JournalError::Corrupt);
        }
        let payload = encode_record(&record)?;
        let mut connection = self.connection(context)?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sqlite_error)?;
        let reservation: Option<(i64, Vec<u8>, Vec<u8>)> = transaction
            .query_row(
                "SELECT operation_kind, operation_key, owner
                 FROM worktree_reservation
                 WHERE workspace = ?1 AND repository = ?2 AND branch = ?3",
                rusqlite::params![
                    record.scope.workspace.0.to_vec(),
                    record.scope.repository.to_vec(),
                    &record.plan.branch
                ],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()
            .map_err(sqlite_error)?;
        let Some((kind, key, owner)) = reservation else {
            return Err(JournalError::ReservationMissing);
        };
        let expected_kind = match kind {
            0 => JournalKind::Add,
            1 => JournalKind::Remove,
            _ => return Err(JournalError::Corrupt),
        };
        let expected_key: [u8; 16] = key.try_into().map_err(|_| JournalError::Corrupt)?;
        let expected_owner: [u8; 16] = owner.try_into().map_err(|_| JournalError::Corrupt)?;
        if (JournalOperation {
            kind: expected_kind,
            key: OperationKey(expected_key),
        }) != record.operation
            || OperationOwner(expected_owner) != record.owner
        {
            return Err(JournalError::ReservationBusy);
        }
        let exists: Option<i64> = transaction
            .query_row(
                "SELECT 1 FROM worktree_journal WHERE operation_kind = ?1 AND operation_key = ?2",
                rusqlite::params![
                    sqlite_operation_kind(record.operation.kind),
                    record.operation.key.0.to_vec()
                ],
                |row| row.get(0),
            )
            .optional()
            .map_err(sqlite_error)?;
        if exists.is_some() {
            return Err(JournalError::Duplicate);
        }
        let count: i64 = transaction
            .query_row("SELECT COUNT(*) FROM worktree_journal", [], |row| {
                row.get(0)
            })
            .map_err(sqlite_error)?;
        if count as usize >= MAX_JOURNAL_OPERATIONS {
            return Err(JournalError::Full);
        }
        transaction
            .execute(
                "INSERT INTO worktree_journal (operation_kind, operation_key, version, state, payload)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                rusqlite::params![
                    sqlite_operation_kind(record.operation.kind),
                    record.operation.key.0.to_vec(),
                    record.version as i64,
                    record.state as i64,
                    payload,
                ],
            )
            .map_err(|error| {
                if matches!(error, rusqlite::Error::SqliteFailure(ref code, _) if code.code == rusqlite::ErrorCode::ConstraintViolation) {
                    JournalError::Duplicate
                } else {
                    JournalError::Corrupt
                }
            })?;
        transaction.commit().map_err(sqlite_error)
    }

    fn update_owned_cas(
        &self,
        operation: JournalOperation,
        owner: OperationOwner,
        expected_version: u64,
        expected_state: JournalState,
        next_state: JournalState,
        receipt: Option<CreatedWorktree>,
        context: JournalContext<'_>,
    ) -> Result<JournalRecord, JournalError> {
        context.check()?;
        let mut connection = self.connection(context)?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sqlite_error)?;
        let row: Option<(i64, i64, Vec<u8>)> = transaction
            .query_row(
                "SELECT version, state, payload FROM worktree_journal
                 WHERE operation_kind = ?1 AND operation_key = ?2",
                rusqlite::params![
                    sqlite_operation_kind(operation.kind),
                    operation.key.0.to_vec()
                ],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()
            .map_err(sqlite_error)?;
        let Some((version, state, payload)) = row else {
            return Err(JournalError::CasMismatch);
        };
        let mut record = Self::row_to_record(
            sqlite_operation_kind(operation.kind),
            operation.key.0.to_vec(),
            version,
            state,
            payload,
        )?;
        if record.owner != owner {
            return Err(JournalError::OwnerMismatch);
        }
        if record.version != expected_version || record.state != expected_state {
            return Err(JournalError::CasMismatch);
        }
        if !valid_state_transition(record.state, next_state) {
            return Err(JournalError::CasMismatch);
        }
        if receipt
            .as_ref()
            .is_some_and(|receipt| !receipt_matches_plan(receipt, operation, &record.plan))
        {
            return Err(JournalError::Corrupt);
        }
        record.state = next_state;
        record.version = record.version.checked_add(1).ok_or(JournalError::Corrupt)?;
        record.receipt = receipt;
        let encoded = encode_record(&record)?;
        let changed = transaction
            .execute(
                "UPDATE worktree_journal SET version = ?1, state = ?2, payload = ?3
                 WHERE operation_kind = ?4 AND operation_key = ?5 AND version = ?6 AND state = ?7",
                rusqlite::params![
                    record.version as i64,
                    record.state as i64,
                    encoded,
                    sqlite_operation_kind(operation.kind),
                    operation.key.0.to_vec(),
                    expected_version as i64,
                    expected_state as i64,
                ],
            )
            .map_err(sqlite_error)?;
        if changed != 1 {
            return Err(JournalError::CasMismatch);
        }
        transaction.commit().map_err(sqlite_error)?;
        Ok(record)
    }

    fn get(
        &self,
        operation: JournalOperation,
        context: JournalContext<'_>,
    ) -> Result<Option<JournalRecord>, JournalError> {
        context.check()?;
        let connection = self.connection(context)?;
        connection
            .query_row(
                "SELECT operation_kind, operation_key, version, state, payload
                 FROM worktree_journal WHERE operation_kind = ?1 AND operation_key = ?2",
                rusqlite::params![
                    sqlite_operation_kind(operation.kind),
                    operation.key.0.to_vec()
                ],
                |row| {
                    Self::row_to_record(
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                    )
                    .map_err(|_| rusqlite::Error::InvalidQuery)
                },
            )
            .optional()
            .map_err(|error| {
                if matches!(error, rusqlite::Error::QueryReturnedNoRows) {
                    JournalError::Corrupt
                } else {
                    sqlite_error(error)
                }
            })
    }

    fn records(
        &self,
        limit: usize,
        context: JournalContext<'_>,
    ) -> Result<Vec<JournalRecord>, JournalError> {
        context.check()?;
        let connection = self.connection(context)?;
        let mut statement = connection
            .prepare(
                "SELECT operation_kind, operation_key, version, state, payload
                 FROM worktree_journal ORDER BY operation_kind, operation_key LIMIT ?1",
            )
            .map_err(sqlite_error)?;
        let rows = statement
            .query_map([limit.min(MAX_JOURNAL_OPERATIONS) as i64], |row| {
                Self::row_to_record(
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                )
                .map_err(|_| rusqlite::Error::InvalidQuery)
            })
            .map_err(sqlite_error)?;
        let mut result = Vec::with_capacity(limit.min(MAX_JOURNAL_OPERATIONS));
        for row in rows {
            context.check()?;
            result.push(row.map_err(sqlite_error)?);
        }
        Ok(result)
    }

    fn reconcile_reservations(
        &self,
        scope: &WorkspaceScope,
        context: JournalContext<'_>,
    ) -> Result<(), JournalError> {
        context.check()?;
        let mut connection = self.connection(context)?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sqlite_error)?;
        let reservations: Vec<(String, i64, Vec<u8>)> = {
            let mut statement = transaction
                .prepare(
                    "SELECT branch, operation_kind, operation_key
                     FROM worktree_reservation
                     WHERE workspace = ?1 AND repository = ?2
                     LIMIT ?3",
                )
                .map_err(sqlite_error)?;
            let rows = statement
                .query_map(
                    rusqlite::params![
                        scope.workspace.0.to_vec(),
                        scope.repository.to_vec(),
                        (MAX_JOURNAL_OPERATIONS + 1) as i64
                    ],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
                .map_err(sqlite_error)?;
            let mut result = Vec::with_capacity(MAX_JOURNAL_OPERATIONS + 1);
            for row in rows {
                context.check()?;
                result.push(row.map_err(sqlite_error)?);
            }
            result
        };
        if reservations.len() > MAX_JOURNAL_OPERATIONS {
            return Err(JournalError::Full);
        }
        for (branch, operation_kind, operation_key) in reservations {
            context.check()?;
            let payload: Option<Vec<u8>> = transaction
                .query_row(
                    "SELECT payload FROM worktree_journal
                     WHERE operation_kind = ?1 AND operation_key = ?2",
                    rusqlite::params![operation_kind, operation_key],
                    |row| row.get(0),
                )
                .optional()
                .map_err(sqlite_error)?;
            let valid = payload
                .map(|payload| decode_record(&payload))
                .transpose()?
                .is_some_and(|record| {
                    sqlite_operation_kind(record.operation.kind) == operation_kind
                        && record.operation.key.0.as_slice() == operation_key.as_slice()
                        && record.scope.stable_eq(scope)
                        && record.plan.branch == branch
                });
            if !valid {
                transaction
                    .execute(
                        "DELETE FROM worktree_reservation
                         WHERE workspace = ?1 AND repository = ?2 AND branch = ?3
                           AND operation_kind = ?4 AND operation_key = ?5",
                        rusqlite::params![
                            scope.workspace.0.to_vec(),
                            scope.repository.to_vec(),
                            branch,
                            operation_kind,
                            operation_key
                        ],
                    )
                    .map_err(sqlite_error)?;
            }
        }
        transaction.commit().map_err(sqlite_error)
    }

    fn reserve(
        &self,
        scope: &WorkspaceScope,
        branch: &str,
        operation: JournalOperation,
        owner: OperationOwner,
        context: JournalContext<'_>,
    ) -> Result<(), JournalError> {
        context.check()?;
        if branch.is_empty() || branch.len() > MAX_BRANCH_BYTES {
            return Err(JournalError::Corrupt);
        }
        if scope.execution_lease == [0; 16] {
            return Err(JournalError::Corrupt);
        }
        let mut connection = self.connection(context)?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sqlite_error)?;
        let existing: Option<(i64, Vec<u8>, Vec<u8>)> = transaction
            .query_row(
                "SELECT operation_kind, operation_key, owner FROM worktree_reservation
                 WHERE workspace = ?1 AND repository = ?2 AND branch = ?3",
                rusqlite::params![
                    scope.workspace.0.to_vec(),
                    scope.repository.to_vec(),
                    branch
                ],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()
            .map_err(sqlite_error)?;
        if let Some((kind, key, existing_owner)) = existing {
            let expected_key: [u8; 16] = key.try_into().map_err(|_| JournalError::Corrupt)?;
            let expected_kind = match kind {
                0 => JournalKind::Add,
                1 => JournalKind::Remove,
                _ => return Err(JournalError::Corrupt),
            };
            let expected_owner: [u8; 16] = existing_owner
                .try_into()
                .map_err(|_| JournalError::Corrupt)?;
            if (JournalOperation {
                kind: expected_kind,
                key: OperationKey(expected_key),
            }) == operation
            {
                if expected_owner == owner.0 {
                    transaction.commit().map_err(sqlite_error)?;
                    return Ok(());
                }
                return Err(JournalError::OperationInFlight);
            }
            return Err(JournalError::ReservationBusy);
        }
        let count: i64 = transaction
            .query_row("SELECT COUNT(*) FROM worktree_reservation", [], |row| {
                row.get(0)
            })
            .map_err(sqlite_error)?;
        if count as usize >= MAX_JOURNAL_OPERATIONS {
            return Err(JournalError::Full);
        }
        transaction
            .execute(
                "INSERT INTO worktree_reservation
                 (workspace, repository, branch, operation_kind, operation_key, owner)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                rusqlite::params![
                    scope.workspace.0.to_vec(),
                    scope.repository.to_vec(),
                    branch,
                    sqlite_operation_kind(operation.kind),
                    operation.key.0.to_vec(),
                    owner.0.to_vec(),
                ],
            )
            .map_err(|error| {
                if matches!(error, rusqlite::Error::SqliteFailure(ref code, _) if code.code == rusqlite::ErrorCode::ConstraintViolation) {
                    JournalError::ReservationBusy
                } else {
                    JournalError::Corrupt
                }
            })?;
        transaction.commit().map_err(sqlite_error)
    }

    fn release(
        &self,
        scope: &WorkspaceScope,
        branch: &str,
        operation: JournalOperation,
        owner: OperationOwner,
        context: JournalContext<'_>,
    ) -> Result<(), JournalError> {
        context.check()?;
        let mut connection = self.connection(context)?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sqlite_error)?;
        let changed = transaction
            .execute(
                "DELETE FROM worktree_reservation
                 WHERE workspace = ?1 AND repository = ?2 AND branch = ?3
                   AND operation_kind = ?4 AND operation_key = ?5 AND owner = ?6",
                rusqlite::params![
                    scope.workspace.0.to_vec(),
                    scope.repository.to_vec(),
                    branch,
                    sqlite_operation_kind(operation.kind),
                    operation.key.0.to_vec(),
                    owner.0.to_vec(),
                ],
            )
            .map_err(sqlite_error)?;
        if changed == 1 {
            transaction.commit().map_err(sqlite_error)?;
            return Ok(());
        }
        let existing: Option<i64> = transaction
            .query_row(
                "SELECT 1 FROM worktree_reservation
                 WHERE workspace = ?1 AND repository = ?2 AND branch = ?3",
                rusqlite::params![
                    scope.workspace.0.to_vec(),
                    scope.repository.to_vec(),
                    branch
                ],
                |row| row.get(0),
            )
            .optional()
            .map_err(sqlite_error)?;
        if existing.is_some() {
            Err(JournalError::OwnerMismatch)
        } else {
            // Idempotent release after a successful atomic settlement.
            transaction.commit().map_err(sqlite_error)?;
            Ok(())
        }
    }

    fn settle_and_release(
        &self,
        operation: JournalOperation,
        expected_version: u64,
        expected_state: JournalState,
        next_state: JournalState,
        receipt: Option<CreatedWorktree>,
        scope: &WorkspaceScope,
        branch: &str,
        owner: OperationOwner,
        context: JournalContext<'_>,
    ) -> Result<JournalRecord, JournalError> {
        context.check()?;
        if branch.is_empty() || branch.len() > MAX_BRANCH_BYTES {
            return Err(JournalError::Corrupt);
        }
        if scope.execution_lease == [0; 16] {
            return Err(JournalError::Corrupt);
        }
        let mut connection = self.connection(context)?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sqlite_error)?;
        let row: Option<(i64, i64, Vec<u8>)> = transaction
            .query_row(
                "SELECT version, state, payload FROM worktree_journal
                 WHERE operation_kind = ?1 AND operation_key = ?2",
                rusqlite::params![
                    sqlite_operation_kind(operation.kind),
                    operation.key.0.to_vec()
                ],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()
            .map_err(sqlite_error)?;
        let Some((version, state, payload)) = row else {
            return Err(JournalError::CasMismatch);
        };
        let mut record = Self::row_to_record(
            sqlite_operation_kind(operation.kind),
            operation.key.0.to_vec(),
            version,
            state,
            payload,
        )?;
        if record.version != expected_version || record.state != expected_state {
            return Err(JournalError::CasMismatch);
        }
        if record.scope != *scope || record.plan.branch != branch || record.owner != owner {
            return Err(JournalError::OwnerMismatch);
        }
        if !valid_state_transition(record.state, next_state) {
            return Err(JournalError::CasMismatch);
        }
        if receipt
            .as_ref()
            .is_some_and(|receipt| !receipt_matches_plan(receipt, operation, &record.plan))
        {
            return Err(JournalError::Corrupt);
        }
        record.state = next_state;
        record.version = record.version.checked_add(1).ok_or(JournalError::Corrupt)?;
        record.receipt = receipt;
        let encoded = encode_record(&record)?;
        let changed = transaction
            .execute(
                "UPDATE worktree_journal SET version = ?1, state = ?2, payload = ?3
                 WHERE operation_kind = ?4 AND operation_key = ?5 AND version = ?6 AND state = ?7",
                rusqlite::params![
                    record.version as i64,
                    record.state as i64,
                    encoded,
                    sqlite_operation_kind(operation.kind),
                    operation.key.0.to_vec(),
                    expected_version as i64,
                    expected_state as i64,
                ],
            )
            .map_err(sqlite_error)?;
        if changed != 1 {
            return Err(JournalError::CasMismatch);
        }
        let released = transaction
            .execute(
                "DELETE FROM worktree_reservation
                 WHERE workspace = ?1 AND repository = ?2 AND branch = ?3
                   AND operation_kind = ?4 AND operation_key = ?5 AND owner = ?6",
                rusqlite::params![
                    scope.workspace.0.to_vec(),
                    scope.repository.to_vec(),
                    branch,
                    sqlite_operation_kind(operation.kind),
                    operation.key.0.to_vec(),
                    owner.0.to_vec(),
                ],
            )
            .map_err(sqlite_error)?;
        if released != 1 {
            return Err(JournalError::OwnerMismatch);
        }
        transaction.commit().map_err(sqlite_error)?;
        Ok(record)
    }

    fn claim_recovery(
        &self,
        operation: JournalOperation,
        expected_version: u64,
        expected_state: JournalState,
        expected_owner: OperationOwner,
        new_owner: OperationOwner,
        context: JournalContext<'_>,
    ) -> Result<JournalRecord, JournalError> {
        context.check()?;
        let mut connection = self.connection(context)?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sqlite_error)?;
        let row: Option<(i64, i64, Vec<u8>)> = transaction
            .query_row(
                "SELECT version, state, payload FROM worktree_journal
                 WHERE operation_kind = ?1 AND operation_key = ?2",
                rusqlite::params![
                    sqlite_operation_kind(operation.kind),
                    operation.key.0.to_vec()
                ],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()
            .map_err(sqlite_error)?;
        let Some((version, state, payload)) = row else {
            return Err(JournalError::OwnerMismatch);
        };
        let mut record = Self::row_to_record(
            sqlite_operation_kind(operation.kind),
            operation.key.0.to_vec(),
            version,
            state,
            payload,
        )?;
        if record.version != expected_version
            || record.state != expected_state
            || record.owner != expected_owner
        {
            return Err(JournalError::OwnerMismatch);
        }
        let previous_version = record.version;
        record.owner = new_owner;
        record.version = record.version.checked_add(1).ok_or(JournalError::Corrupt)?;
        let encoded = encode_record(&record)?;
        let changed = transaction
            .execute(
                "UPDATE worktree_journal SET version = ?1, payload = ?2
                 WHERE operation_kind = ?3 AND operation_key = ?4
                   AND version = ?5 AND state = ?6",
                rusqlite::params![
                    record.version as i64,
                    encoded,
                    sqlite_operation_kind(operation.kind),
                    operation.key.0.to_vec(),
                    previous_version as i64,
                    expected_state as i64,
                ],
            )
            .map_err(sqlite_error)?;
        if changed != 1 {
            return Err(JournalError::OwnerMismatch);
        }
        let reservation_changed = transaction
            .execute(
                "UPDATE worktree_reservation SET owner = ?1
                 WHERE workspace = ?2 AND repository = ?3 AND branch = ?4
                   AND operation_kind = ?5 AND operation_key = ?6 AND owner = ?7",
                rusqlite::params![
                    new_owner.0.to_vec(),
                    record.scope.workspace.0.to_vec(),
                    record.scope.repository.to_vec(),
                    record.plan.branch,
                    sqlite_operation_kind(operation.kind),
                    operation.key.0.to_vec(),
                    expected_owner.0.to_vec(),
                ],
            )
            .map_err(sqlite_error)?;
        if reservation_changed == 0 {
            let existing: Option<i64> = transaction
                .query_row(
                    "SELECT 1 FROM worktree_reservation
                     WHERE workspace = ?1 AND repository = ?2 AND branch = ?3",
                    rusqlite::params![
                        record.scope.workspace.0.to_vec(),
                        record.scope.repository.to_vec(),
                        &record.plan.branch,
                    ],
                    |row| row.get(0),
                )
                .optional()
                .map_err(sqlite_error)?;
            if existing.is_some() {
                return Err(JournalError::OwnerMismatch);
            }
            let count: i64 = transaction
                .query_row("SELECT COUNT(*) FROM worktree_reservation", [], |row| {
                    row.get(0)
                })
                .map_err(sqlite_error)?;
            if count as usize >= MAX_JOURNAL_OPERATIONS {
                return Err(JournalError::Full);
            }
            transaction
                .execute(
                    "INSERT INTO worktree_reservation
                     (workspace, repository, branch, operation_kind, operation_key, owner)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    rusqlite::params![
                        record.scope.workspace.0.to_vec(),
                        record.scope.repository.to_vec(),
                        &record.plan.branch,
                        sqlite_operation_kind(operation.kind),
                        operation.key.0.to_vec(),
                        new_owner.0.to_vec(),
                    ],
                )
                .map_err(|error| {
                    if matches!(error, rusqlite::Error::SqliteFailure(ref code, _) if code.code == rusqlite::ErrorCode::ConstraintViolation) {
                        JournalError::OwnerMismatch
                    } else {
                        JournalError::Corrupt
                    }
                })?;
        } else if reservation_changed != 1 {
            return Err(JournalError::OwnerMismatch);
        }
        transaction.commit().map_err(sqlite_error)?;
        Ok(record)
    }
}

#[cfg(test)]
pub(crate) type SqliteTestJournal = SqliteWorktreeJournal;

#[cfg(test)]
impl SqliteWorktreeJournal {
    pub(crate) fn open(path: &Path) -> Result<Self, JournalError> {
        let store = WorktreeJournalStore::for_test(path)?;
        Self::from_store(Arc::new(store))
    }
}

#[cfg(test)]
#[derive(Clone)]
pub(crate) struct TestGitWorktreeExecutor {
    state: Arc<Mutex<TestExecutorState>>,
}

#[cfg(test)]
#[derive(Clone)]
struct TestExecutorState {
    collision_attempts: usize,
    cancel_after_add: bool,
    fail_compensation: bool,
    interrupt_after_add: bool,
    tracked: bool,
    untracked: bool,
    unpushed: bool,
    nested: bool,
    linked: bool,
    foreign: bool,
    main_checkout: bool,
    process_count: u32,
    zero_observation: u64,
    process_fence_mismatch: bool,
    cancel_after_remove: bool,
    linked_identity_mismatch: bool,
    next_commit: u64,
    adds: usize,
    removes: usize,
    active: BTreeMap<OperationKey, CreatedWorktree>,
    pause_before_add: Option<(Arc<Barrier>, Arc<Barrier>)>,
    preview_control: Option<TestWorkspaceControl>,
}

#[cfg(test)]
impl fmt::Debug for TestGitWorktreeExecutor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("TestGitWorktreeExecutor(REDACTED)")
    }
}

#[cfg(test)]
impl TestGitWorktreeExecutor {
    pub(crate) fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(TestExecutorState {
                collision_attempts: 0,
                cancel_after_add: false,
                fail_compensation: false,
                interrupt_after_add: false,
                tracked: false,
                untracked: false,
                unpushed: false,
                nested: false,
                linked: false,
                foreign: false,
                main_checkout: false,
                process_count: 0,
                zero_observation: 0,
                process_fence_mismatch: false,
                cancel_after_remove: false,
                linked_identity_mismatch: false,
                next_commit: 1,
                adds: 0,
                removes: 0,
                active: BTreeMap::new(),
                pause_before_add: None,
                preview_control: None,
            })),
        }
    }

    pub(crate) fn set_collision_attempts(&self, attempts: usize) {
        self.mutate(|state| state.collision_attempts = attempts);
    }

    pub(crate) fn cancel_after_add(&self, value: bool) {
        self.mutate(|state| state.cancel_after_add = value);
    }

    pub(crate) fn fail_compensation(&self, value: bool) {
        self.mutate(|state| state.fail_compensation = value);
    }

    pub(crate) fn interrupt_after_add(&self, value: bool) {
        self.mutate(|state| state.interrupt_after_add = value);
    }

    pub(crate) fn set_dirty(&self, value: bool) {
        self.mutate(|state| state.tracked = value);
    }

    pub(crate) fn set_untracked(&self, value: bool) {
        self.mutate(|state| state.untracked = value);
    }

    pub(crate) fn set_unpushed(&self, value: bool) {
        self.mutate(|state| state.unpushed = value);
    }

    pub(crate) fn set_nested(&self, value: bool) {
        self.mutate(|state| state.nested = value);
    }

    pub(crate) fn set_linked(&self, value: bool) {
        self.mutate(|state| state.linked = value);
    }

    pub(crate) fn set_foreign(&self, value: bool) {
        self.mutate(|state| state.foreign = value);
    }

    pub(crate) fn set_main_checkout(&self, value: bool) {
        self.mutate(|state| state.main_checkout = value);
    }

    pub(crate) fn set_process_count(&self, value: u32) {
        self.mutate(|state| state.process_count = value);
    }

    pub(crate) fn set_process_fence_mismatch(&self, value: bool) {
        self.mutate(|state| state.process_fence_mismatch = value);
    }

    pub(crate) fn cancel_after_remove(&self, value: bool) {
        self.mutate(|state| state.cancel_after_remove = value);
    }

    pub(crate) fn replace_linked_identity(&self, value: bool) {
        self.mutate(|state| state.linked_identity_mismatch = value);
    }

    pub(crate) fn pause_before_add(&self) -> (Arc<Barrier>, Arc<Barrier>) {
        let entered = Arc::new(Barrier::new(2));
        let release = Arc::new(Barrier::new(2));
        self.mutate(|state| state.pause_before_add = Some((entered.clone(), release.clone())));
        (entered, release)
    }

    pub(crate) fn forget(&self, operation: JournalOperation) {
        self.mutate(|state| {
            state.active.remove(&operation.key);
        });
    }

    pub(crate) fn invalidate_after_preview(&self, control: TestWorkspaceControl) {
        self.mutate(|state| state.preview_control = Some(control));
    }

    pub(crate) fn add_count(&self) -> usize {
        self.read(|state| state.adds)
    }

    pub(crate) fn active_count(&self) -> usize {
        self.read(|state| state.active.len())
    }

    fn mutate(&self, f: impl FnOnce(&mut TestExecutorState)) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        f(&mut state);
    }

    fn read<T>(&self, f: impl FnOnce(&TestExecutorState) -> T) -> T {
        let state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        f(&state)
    }
}

#[cfg(test)]
impl Default for TestGitWorktreeExecutor {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
impl sealed::Executor for TestGitWorktreeExecutor {}

#[cfg(test)]
impl GitWorktreeExecutor for TestGitWorktreeExecutor {
    fn probe(
        &self,
        workspace: &ResolvedWorkspace,
        plan: &WorktreePlan,
        cancellation: &CancellationToken,
        budget: ExecutionBudget,
    ) -> Result<ProbeResult, ExecutorError> {
        if cancellation.is_cancelled() {
            return Err(ExecutorError::Cancelled);
        }
        if budget.expired() {
            return Err(ExecutorError::Deadline);
        }
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if state.linked_identity_mismatch || workspace.linked.repository == [0; 32] {
            return Err(ExecutorError::IdentityMismatch);
        }
        if state.collision_attempts > 0 {
            state.collision_attempts -= 1;
            Ok(ProbeResult::Collision)
        } else if state.active.values().any(|receipt| {
            receipt.scope.workspace == workspace.scope.workspace
                && receipt.scope.repository == workspace.scope.repository
                && receipt.branch == plan.branch
        }) {
            Ok(ProbeResult::Collision)
        } else {
            let mut base_commit = [0; 32];
            base_commit[..8].copy_from_slice(&state.next_commit.to_le_bytes());
            let root = std::env::current_exe()
                .ok()
                .and_then(|path| path.parent().map(Path::to_path_buf))
                .unwrap_or_else(|| PathBuf::from("C:\\"));
            let target = if plan.target.validate() {
                plan.target.clone()
            } else {
                WorktreeTarget::for_test(
                    root.clone(),
                    root.join(format!(
                        "devmanager-test-worktree-{}",
                        plan.branch.replace('/', "-")
                    )),
                )
            };
            let base_revision = format!("{:064x}", state.next_commit);
            Ok(ProbeResult::Available {
                base_commit,
                base_revision,
                target,
            })
        }
    }

    fn add(
        &self,
        workspace: &ResolvedWorkspace,
        operation: JournalOperation,
        plan: &WorktreePlan,
        cancellation: &CancellationToken,
        budget: ExecutionBudget,
    ) -> Result<AddResult, ExecutorError> {
        if cancellation.is_cancelled() {
            return Err(ExecutorError::Cancelled);
        }
        if budget.expired() {
            return Err(ExecutorError::Deadline);
        }
        let pause = {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            state.pause_before_add.take()
        };
        if let Some((entered, release)) = pause {
            entered.wait();
            release.wait();
        }
        if cancellation.is_cancelled() || budget.expired() {
            return Err(if cancellation.is_cancelled() {
                ExecutorError::Cancelled
            } else {
                ExecutorError::Deadline
            });
        }
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(existing) = state.active.get(&operation.key) {
            return Ok(AddResult::Applied(existing.clone()));
        }
        if plan.scope != workspace.scope || plan.linked != workspace.linked {
            return Err(ExecutorError::IdentityMismatch);
        }
        let mut commit = plan.base_commit;
        if commit == ZERO_FINGERPRINT {
            commit[..8].copy_from_slice(&state.next_commit.to_le_bytes());
        }
        state.next_commit += 1;
        let receipt = CreatedWorktree {
            operation_id: operation.key,
            scope: plan.scope,
            branch: plan.branch.clone(),
            base_commit: commit,
            base_revision: plan.base_revision.clone(),
            target: plan.target.clone(),
            identity: plan.identity,
            linked: plan.linked,
        };
        state.adds += 1;
        state.active.insert(operation.key, receipt.clone());
        if state.cancel_after_add {
            cancellation.cancel();
        }
        if state.interrupt_after_add {
            return Ok(AddResult::InterruptedAfterSideEffect);
        }
        Ok(AddResult::Applied(receipt))
    }

    fn inspect(
        &self,
        _workspace: &ResolvedWorkspace,
        operation: JournalOperation,
        plan: &WorktreePlan,
        expected_receipt: Option<&CreatedWorktree>,
        cancellation: &CancellationToken,
        budget: ExecutionBudget,
    ) -> Result<RecoveryLookup, ExecutorError> {
        if cancellation.is_cancelled() {
            return Err(ExecutorError::Cancelled);
        }
        if budget.expired() {
            return Err(ExecutorError::Deadline);
        }
        let state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(receipt) = state.active.get(&operation.key).cloned() else {
            return Ok(RecoveryLookup::Absent);
        };
        if receipt.branch != plan.branch
            || receipt.scope != plan.scope
            || receipt.identity != plan.identity
            || receipt.base_commit != plan.base_commit
            || receipt.base_revision != plan.base_revision
            || receipt.target != plan.target
            || receipt.linked != plan.linked
            || receipt.scope.repository != plan.repository
            || expected_receipt.is_some_and(|expected| expected != &receipt)
        {
            return Err(ExecutorError::IdentityMismatch);
        }
        Ok(RecoveryLookup::Applied(receipt))
    }

    fn compensate(
        &self,
        _workspace: &ResolvedWorkspace,
        operation: JournalOperation,
        _receipt: &CreatedWorktree,
        cancellation: &CancellationToken,
        budget: ExecutionBudget,
    ) -> Result<(), ExecutorError> {
        if cancellation.is_cancelled() {
            return Err(ExecutorError::Cancelled);
        }
        if budget.expired() {
            return Err(ExecutorError::Deadline);
        }
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if state.fail_compensation {
            return Err(ExecutorError::CompensationFailed);
        }
        state.active.remove(&operation.key);
        Ok(())
    }

    fn preview(
        &self,
        _workspace: &ResolvedWorkspace,
        receipt: &CreatedWorktree,
        cancellation: &CancellationToken,
        budget: ExecutionBudget,
    ) -> Result<CleanupSnapshot, ExecutorError> {
        if cancellation.is_cancelled() {
            return Err(ExecutorError::Cancelled);
        }
        if budget.expired() {
            return Err(ExecutorError::Deadline);
        }
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if !state.active.values().any(|value| value == receipt) {
            if let Some(control) = state.preview_control.take() {
                drop(state);
                control.bump_action_epoch();
            }
            return Err(ExecutorError::NotFound);
        }
        Ok(CleanupSnapshot {
            tracked: if state.tracked {
                CleanupState::Dirty
            } else {
                CleanupState::Clean
            },
            untracked: if state.untracked {
                CleanupState::Dirty
            } else {
                CleanupState::Clean
            },
            unpushed: if state.unpushed {
                CleanupState::Dirty
            } else {
                CleanupState::Clean
            },
            nested: if state.nested {
                CleanupState::Dirty
            } else {
                CleanupState::Clean
            },
            linked: if state.linked {
                CleanupState::Dirty
            } else {
                CleanupState::Clean
            },
            foreign: if state.foreign {
                CleanupState::Dirty
            } else {
                CleanupState::Clean
            },
            main_checkout: if state.main_checkout {
                CleanupState::Dirty
            } else {
                CleanupState::Clean
            },
        })
    }

    fn prove_process_zero(
        &self,
        workspace: &ResolvedWorkspace,
        receipt: &CreatedWorktree,
        cancellation: &CancellationToken,
        budget: ExecutionBudget,
    ) -> Result<ProcessZeroProof, ExecutorError> {
        if cancellation.is_cancelled() {
            return Err(ExecutorError::Cancelled);
        }
        if budget.expired() {
            return Err(ExecutorError::Deadline);
        }
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if state.process_count != 0 {
            return Err(ExecutorError::ProcessNotZero);
        }
        if receipt.scope.workspace != workspace.identity {
            return Err(ExecutorError::IdentityMismatch);
        }
        state.zero_observation = state.zero_observation.saturating_add(1).max(1);
        let Some(expected_fence) = workspace.process_fence.clone() else {
            return Err(ExecutorError::ProcessNotZero);
        };
        let mut fence = expected_fence;
        if state.process_fence_mismatch {
            fence = RegistryManagedProcessFence::new(
                ResourceFence::new(ResourceId::new(), fence.resource().runtime_generation),
                fence.owner(),
                fence.root().clone(),
            );
        }
        Ok(ProcessZeroProof {
            identity: workspace.identity,
            fence,
            zero_observation: state.zero_observation,
        })
    }

    fn remove(
        &self,
        workspace: &ResolvedWorkspace,
        _operation: JournalOperation,
        receipt: &CreatedWorktree,
        proof: ProcessZeroProof,
        cancellation: &CancellationToken,
        budget: ExecutionBudget,
    ) -> Result<(), ExecutorError> {
        if cancellation.is_cancelled() {
            return Err(ExecutorError::Cancelled);
        }
        if budget.expired() {
            return Err(ExecutorError::Deadline);
        }
        if proof.identity != receipt.scope.workspace
            || proof.zero_observation == 0
            || workspace.process_fence.as_ref() != Some(&proof.fence)
        {
            return Err(ExecutorError::ProcessNotZero);
        }
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some((key, existing)) = state
            .active
            .iter()
            .find(|(_, existing)| *existing == receipt)
            .map(|(key, existing)| (*key, existing.clone()))
        else {
            return Err(ExecutorError::NotFound);
        };
        if existing != *receipt {
            return Err(ExecutorError::IdentityMismatch);
        }
        state.active.remove(&key);
        state.removes += 1;
        if state.cancel_after_remove {
            cancellation.cancel();
        }
        Ok(())
    }
}

#[cfg(test)]
#[path = "../../tests/worktree_service.rs"]
mod focused_test_source;

#[cfg(test)]
mod focused_tests {
    crate::worktree_service_focused_tests!();
}
