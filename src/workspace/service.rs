use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};

use sha2::{Digest, Sha256};

use crate::domain::task::{
    RepositoryFingerprint, WorkspaceBindingFact, WorkspaceBindingKind, WorkspaceChoice,
    WorkspacePathFact, WorkspaceRef,
};

use super::model::{
    durable_refs_same_location, is_within, path_identity_key, relative_location,
    PendingWorktreeCandidate, RepositoryIdentity, WorkspaceBinding, WorkspaceKind,
    WorkspaceProjectRoots, WorkspaceRequest, WorkspaceResolution, WorkspaceResource,
};
use crate::domain::{ClientId, CommandId, ProjectId, RequestId, TaskId};
use uuid::Uuid;

#[derive(Clone, PartialEq, Eq)]
pub enum WorkspaceError {
    ProjectNotConfigured(ProjectId),
    PathDoesNotExist(PathBuf),
    NotDirectory(PathBuf),
    PathResolution {
        path: PathBuf,
        reason: String,
    },
    NotRepository(PathBuf),
    OutsideProject {
        path: PathBuf,
        project_root: PathBuf,
    },
    DifferentRepository(PathBuf),
    MainRootMismatch {
        configured_root: PathBuf,
        repository_root: PathBuf,
    },
    ChoiceRequired,
    ExternalConfirmationRequired,
    MissingWorktreePath,
    MissingWorktreeBranch,
    InvalidBranch,
    PendingWorktree(PendingWorktreeCandidate),
    WorktreeBranchMismatch {
        path: PathBuf,
        requested: String,
        actual: String,
    },
    LinkedWorktreeBranchUnavailable(PathBuf),
    UnregisteredLinkedWorktree(PathBuf),
    PersistedWorktreeNotLinked(PathBuf),
    RepositoryFingerprintMismatch {
        path: PathBuf,
        expected: RepositoryFingerprint,
        actual: RepositoryFingerprint,
    },
    RebindRequired,
    WorkspaceImmutable,
    LiveResources(Vec<WorkspaceResource>),
}

impl std::fmt::Debug for WorkspaceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let code = match self {
            Self::ProjectNotConfigured(_) => "ProjectNotConfigured",
            Self::PathDoesNotExist(_) => "PathDoesNotExist",
            Self::NotDirectory(_) => "NotDirectory",
            Self::PathResolution { .. } => "PathResolution",
            Self::NotRepository(_) => "NotRepository",
            Self::OutsideProject { .. } => "OutsideProject",
            Self::DifferentRepository(_) => "DifferentRepository",
            Self::MainRootMismatch { .. } => "MainRootMismatch",
            Self::ChoiceRequired => "ChoiceRequired",
            Self::ExternalConfirmationRequired => "ExternalConfirmationRequired",
            Self::MissingWorktreePath => "MissingWorktreePath",
            Self::MissingWorktreeBranch => "MissingWorktreeBranch",
            Self::InvalidBranch => "InvalidBranch",
            Self::PendingWorktree(_) => "PendingWorktree",
            Self::WorktreeBranchMismatch { .. } => "WorktreeBranchMismatch",
            Self::LinkedWorktreeBranchUnavailable(_) => "LinkedWorktreeBranchUnavailable",
            Self::UnregisteredLinkedWorktree(_) => "UnregisteredLinkedWorktree",
            Self::PersistedWorktreeNotLinked(_) => "PersistedWorktreeNotLinked",
            Self::RepositoryFingerprintMismatch { .. } => "RepositoryFingerprintMismatch",
            Self::RebindRequired => "RebindRequired",
            Self::WorkspaceImmutable => "WorkspaceImmutable",
            Self::LiveResources(_) => "LiveResources",
        };
        write!(f, "WorkspaceError::{code}")
    }
}

impl std::fmt::Display for WorkspaceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ProjectNotConfigured(_) => f.write_str("host project is not configured"),
            Self::PathDoesNotExist(_) => f.write_str("workspace path does not exist"),
            Self::NotDirectory(_) => f.write_str("workspace path is not a directory"),
            Self::PathResolution { .. } => f.write_str("workspace path cannot be resolved"),
            Self::NotRepository(_) => f.write_str("workspace is not inside a Git repository"),
            Self::OutsideProject { .. } => f.write_str("workspace resolves outside the project"),
            Self::DifferentRepository(_) => {
                f.write_str("workspace belongs to a different repository")
            }
            Self::MainRootMismatch { .. } => {
                f.write_str("Main workspace root does not match the project")
            }
            Self::ChoiceRequired => f.write_str("workspace choice requires an explicit answer"),
            Self::ExternalConfirmationRequired => {
                f.write_str("external workspace requires explicit confirmation")
            }
            Self::MissingWorktreePath => f.write_str("new worktree requires a resolved path"),
            Self::MissingWorktreeBranch => f.write_str("new worktree requires a branch"),
            Self::InvalidBranch => f.write_str("new worktree branch is invalid"),
            Self::PendingWorktree(_) => f.write_str("new worktree is pending creation"),
            Self::WorktreeBranchMismatch { .. } => {
                f.write_str("linked worktree branch does not match")
            }
            Self::LinkedWorktreeBranchUnavailable(_) => {
                f.write_str("linked worktree branch is unavailable")
            }
            Self::UnregisteredLinkedWorktree(_) => {
                f.write_str("linked worktree registration is unavailable")
            }
            Self::PersistedWorktreeNotLinked(_) => {
                f.write_str("persisted worktree is no longer linked")
            }
            Self::RepositoryFingerprintMismatch { .. } => {
                f.write_str("workspace repository identity changed")
            }
            Self::RebindRequired => f.write_str("workspace requires an explicit host rebind"),
            Self::WorkspaceImmutable => f.write_str("task workspace is immutable after binding"),
            Self::LiveResources(_) => f.write_str("workspace has live resources"),
        }
    }
}

impl std::error::Error for WorkspaceError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkspaceLeaseError {
    ScopeMismatch,
    InvalidAdmission,
    SpentGeneration,
    InvalidGeneration,
    StaleGeneration,
    Revoked,
}

impl std::fmt::Display for WorkspaceLeaseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ScopeMismatch => {
                f.write_str("workspace lease scope does not belong to this task")
            }
            Self::InvalidAdmission => f.write_str("workspace lease admission is invalid"),
            Self::SpentGeneration => f.write_str("workspace lease generation is spent"),
            Self::InvalidGeneration => f.write_str("workspace lease generation is invalid"),
            Self::StaleGeneration => f.write_str("workspace lease generation is stale"),
            Self::Revoked => f.write_str("workspace lease is revoked or no longer active"),
        }
    }
}

impl std::error::Error for WorkspaceLeaseError {}

/// Opaque proof that a host-owned project resolved a workspace request before
/// a durable CreateTask command was admitted to the kernel.
///
/// The fields are intentionally private. Callers can only obtain this value
/// from [`WorkspaceService::bind_authorized`], after the service has resolved
/// the request against its configured project root.
pub struct WorkspaceAuthorization {
    project_id: ProjectId,
    task_id: TaskId,
    client_id: ClientId,
    connection_id: Uuid,
    request_id: RequestId,
    command_id: CommandId,
    workspace_identity: [u8; 32],
    action_epoch: u64,
    runtime_generation: u64,
    project_root: PathBuf,
    project_root_identity: String,
    binding: WorkspaceBinding,
    pins: Vec<PinnedPath>,
}

impl std::fmt::Debug for WorkspaceAuthorization {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("WorkspaceAuthorization(REDACTED)")
    }
}

impl WorkspaceAuthorization {
    fn new(
        project_id: ProjectId,
        task_id: TaskId,
        client_id: ClientId,
        connection_id: Uuid,
        request_id: RequestId,
        command_id: CommandId,
        project_root: &Path,
        project_root_identity: &str,
        binding: &WorkspaceBinding,
        pins: Vec<PinnedPath>,
    ) -> Self {
        Self::new_with_generation(
            project_id,
            task_id,
            client_id,
            connection_id,
            request_id,
            command_id,
            project_root,
            project_root_identity,
            binding,
            pins,
            0,
            0,
        )
    }

    fn new_with_generation(
        project_id: ProjectId,
        task_id: TaskId,
        client_id: ClientId,
        connection_id: Uuid,
        request_id: RequestId,
        command_id: CommandId,
        project_root: &Path,
        project_root_identity: &str,
        binding: &WorkspaceBinding,
        pins: Vec<PinnedPath>,
        action_epoch: u64,
        runtime_generation: u64,
    ) -> Self {
        let fingerprint = binding
            .durable_ref()
            .repository_fingerprint()
            .map(RepositoryFingerprint::as_str)
            .unwrap_or("");
        let workspace_identity = Sha256::digest(fingerprint.as_bytes()).into();
        Self {
            project_id,
            task_id,
            client_id,
            connection_id,
            request_id,
            command_id,
            workspace_identity,
            action_epoch,
            runtime_generation,
            project_root: project_root.to_path_buf(),
            project_root_identity: project_root_identity.to_string(),
            binding: binding.clone(),
            pins,
        }
    }

    pub(crate) fn permits(
        &self,
        task_id: TaskId,
        project_id: ProjectId,
        client_id: ClientId,
        connection_id: Uuid,
        request_id: RequestId,
        command_id: CommandId,
        workspace: &WorkspaceRef,
    ) -> bool {
        self.permits_with_generation(
            task_id,
            project_id,
            client_id,
            connection_id,
            request_id,
            command_id,
            workspace,
            self.action_epoch,
            self.runtime_generation,
        )
    }

    pub(crate) fn permits_with_generation(
        &self,
        task_id: TaskId,
        project_id: ProjectId,
        client_id: ClientId,
        connection_id: Uuid,
        request_id: RequestId,
        command_id: CommandId,
        workspace: &WorkspaceRef,
        action_epoch: u64,
        runtime_generation: u64,
    ) -> bool {
        self.task_id == task_id
            && self.project_id == project_id
            && self.client_id == client_id
            && self.connection_id == connection_id
            && self.request_id == request_id
            && self.command_id == command_id
            && self.action_epoch == action_epoch
            && self.runtime_generation == runtime_generation
            && self.workspace_identity == workspace_identity_for_ref(workspace)
            && durable_refs_same_location(self.binding.durable_ref(), workspace)
            && self.binding_is_current()
    }

    /// Return the host-private binding only after every authority dimension
    /// and the live filesystem pins have been checked.  Downstream services
    /// (for example Git) must use this seam instead of extracting `binding`
    /// directly; a stale action/runtime generation therefore cannot revive a
    /// path authority by retaining an older token.
    pub(crate) fn validated_binding(
        &self,
        task_id: TaskId,
        project_id: ProjectId,
        client_id: ClientId,
        connection_id: Uuid,
        request_id: RequestId,
        command_id: CommandId,
        workspace: &WorkspaceRef,
        action_epoch: u64,
        runtime_generation: u64,
    ) -> Option<&WorkspaceBinding> {
        self.permits_with_generation(
            task_id,
            project_id,
            client_id,
            connection_id,
            request_id,
            command_id,
            workspace,
            action_epoch,
            runtime_generation,
        )
        .then_some(&self.binding)
    }

    /// Return a cloned handle only from the retained pin that participated in
    /// the same live authorization check. Downstream workspace services use
    /// this sealed seam instead of accepting a path/handle tuple supplied by
    /// an arbitrary caller.
    pub(crate) fn retained_pin_for_fact(
        &self,
        fact: &WorkspacePathFact,
    ) -> Option<WorkspacePinnedPath> {
        self.pins
            .iter()
            .find(|pin| {
                pin.path == fact.path()
                    && pin.identity == fact.identity()
                    && pin.matches_current_path()
            })
            .and_then(WorkspacePinnedPath::from_pinned)
    }

    pub(crate) fn request_id(&self) -> RequestId {
        self.request_id
    }

    pub(crate) fn connection_id(&self) -> Uuid {
        self.connection_id
    }

    pub(crate) fn action_epoch(&self) -> u64 {
        self.action_epoch
    }

    pub(crate) fn runtime_generation(&self) -> u64 {
        self.runtime_generation
    }

    fn binding_is_current(&self) -> bool {
        let Some(expected) = self.binding.durable_ref().host_binding() else {
            return false;
        };
        expected.validate().is_ok()
            && self.pins.iter().all(PinnedPath::matches_current_path)
            && pins_match_fact(&self.pins, expected)
            && configured_root_identity_matches(&self.project_root, &self.project_root_identity)
            && capture_binding_fact(
                self.binding.kind(),
                &self.project_root,
                self.binding.path(),
                self.binding.branch(),
            )
            .is_ok_and(|(actual, _)| actual == *expected)
    }
}

fn workspace_identity_for_ref(workspace: &WorkspaceRef) -> [u8; 32] {
    let fingerprint = workspace
        .repository_fingerprint()
        .map(RepositoryFingerprint::as_str)
        .unwrap_or("");
    Sha256::digest(fingerprint.as_bytes()).into()
}

#[cfg(test)]
mod fingerprint_tests {
    use super::{repository_fingerprint, PinnedPath, WorkspaceService};
    use crate::domain::{ClientId, CommandId, ProjectId, RequestId};
    use crate::workspace::model::{WorkspaceProjectRoots, WorkspaceRequest};
    use std::fs;
    use std::io;
    use std::path::Path;
    use uuid::Uuid;

    #[test]
    fn missing_repository_metadata_does_not_produce_a_fingerprint() {
        assert!(
            repository_fingerprint(Path::new("missing-repository-metadata")).is_none(),
            "a missing Git directory must fail closed instead of hashing only its path"
        );
    }

    #[test]
    fn replacement_after_a_directory_pin_fails_exact_identity_revalidation() {
        let temp = tempfile::tempdir().expect("identity tempdir");
        let path = temp.path().join("repository");
        fs::create_dir(&path).expect("identity directory");
        let pinned = PinnedPath::open(&path).expect("pin identity directory");

        let old_path = temp.path().join("repository-old");
        fs::rename(&path, &old_path).expect("replace original directory");
        fs::create_dir(&path).expect("replacement directory");

        assert!(
            !pinned.matches_current_path(),
            "a replacement at the same path must not retain the original identity"
        );
    }

    #[test]
    fn nested_ancestor_swap_fails_exact_identity_revalidation() {
        let temp = tempfile::tempdir().expect("nested identity tempdir");
        let parent = temp.path().join("parent");
        let nested = parent.join("nested");
        let target = nested.join("metadata");
        fs::create_dir_all(&nested).expect("nested directory");
        fs::write(&target, "original").expect("metadata");
        let pinned = PinnedPath::open(&target).expect("pin nested metadata");

        if let Err(error) = fs::rename(&nested, parent.join("nested-old")) {
            #[cfg(windows)]
            {
                // Windows may reject a rename while the pinned descendant
                // handles are held.  That is the strongest safe outcome: the
                // nested swap cannot proceed around the authority.
                assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
                assert!(pinned.matches_current_path());
                return;
            }
            #[cfg(not(windows))]
            panic!("swap nested directory: {error}");
        }
        fs::create_dir(&nested).expect("replacement nested directory");
        fs::write(&target, "replacement").expect("replacement metadata");

        assert!(
            !pinned.matches_current_path(),
            "a nested ancestor replacement must invalidate the held path authority"
        );
    }

    #[test]
    fn adding_a_hard_link_invalidates_a_previously_pinned_file() {
        let temp = tempfile::tempdir().expect("hard-link tempdir");
        let path = temp.path().join("metadata");
        fs::write(&path, "hard-link sentinel").expect("metadata");
        let pinned = PinnedPath::open(&path).expect("pin metadata");
        let alias = temp.path().join("metadata-alias");
        if fs::hard_link(&path, &alias).is_err() {
            // Some Windows test volumes disable hard-link creation. The
            // production check remains exercised on supported filesystems.
            return;
        }

        assert!(
            !pinned.matches_current_path(),
            "an admitted file must not remain valid after its hard-link count changes"
        );
    }

    #[test]
    fn held_file_read_rejects_same_length_content_rewrite() {
        let temp = tempfile::tempdir().expect("content tempdir");
        let path = temp.path().join("metadata");
        fs::write(&path, "before-content").expect("metadata");
        let pinned = PinnedPath::open(&path).expect("pin metadata");
        fs::write(&path, "after-content").expect("rewrite metadata");

        assert!(
            pinned.read_to_string().is_err(),
            "read_to_string must recheck the pinned content fingerprint, not only length"
        );
    }

    #[cfg(unix)]
    #[test]
    fn group_or_world_writable_paths_are_rejected_by_the_acl_policy() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().expect("permissions tempdir");
        let path = temp.path().join("metadata");
        fs::write(&path, "permission sentinel").expect("metadata");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o666)).expect("permissive mode");

        let error = PinnedPath::open(&path).expect_err("permissive path must fail closed");
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
    }

    #[cfg(unix)]
    #[test]
    fn permission_change_after_pin_invalidates_identity_and_held_reads() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().expect("permissions tempdir");
        let path = temp.path().join("metadata");
        fs::write(&path, "permission sentinel").expect("metadata");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).expect("private mode");
        let pinned = PinnedPath::open(&path).expect("pin metadata");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).expect("changed mode");

        assert!(!pinned.matches_current_path());
        assert!(pinned.read_to_string().is_err());
    }

    #[cfg(windows)]
    #[test]
    fn acl_descriptor_change_invalidates_a_pinned_path() {
        use std::process::Command;

        let temp = tempfile::tempdir().expect("ACL tempdir");
        let path = temp.path().join("metadata");
        fs::write(&path, "ACL sentinel").expect("metadata");
        let pinned = PinnedPath::open(&path).expect("pin metadata");

        // Use the stable Everyone SID instead of a localized trustee name. The
        // explicit permissive grant changes only this temporary file's
        // descriptor; the policy is descriptor stability, so this change must
        // invalidate the authority. Cleanup removes it before the temp
        // directory is dropped.
        let changed = Command::new("icacls.exe")
            .arg(&path)
            .args(["/grant", "*S-1-1-0:(D)"])
            .status()
            .expect("run ACL grant");
        assert!(
            changed.success(),
            "ACL grant should succeed in the test fixture"
        );

        assert!(!pinned.matches_current_path());
        let restored = Command::new("icacls.exe")
            .arg(&path)
            .args(["/remove:g", "*S-1-1-0"])
            .status()
            .expect("restore ACL fixture");
        assert!(restored.success(), "ACL cleanup should succeed");
    }

    #[test]
    fn consumed_workspace_authority_rejects_a_replaced_repository() {
        let temp = tempfile::tempdir().expect("authority tempdir");
        let repository = temp.path().join("repository");
        fs::create_dir(&repository).expect("repository directory");
        fs::create_dir(repository.join(".git")).expect("git directory");
        fs::write(
            repository.join(".git").join("HEAD"),
            "ref: refs/heads/main\n",
        )
        .expect("repository HEAD");
        let project_id = ProjectId::new();
        let roots = WorkspaceProjectRoots::try_from_pairs([(project_id, repository.clone())])
            .expect("project roots");
        let mut service = WorkspaceService::for_project(project_id, &roots).expect("service");
        let task_id = service.task_id();
        let client_id = ClientId::new();
        let connection_id = Uuid::now_v7();
        let request_id = RequestId::new();
        let command_id = CommandId::new();
        let (_binding, authorization) = service
            .bind_authorized(
                WorkspaceRequest::main(),
                task_id,
                client_id,
                connection_id,
                request_id,
                command_id,
            )
            .expect("workspace authority");

        let replacement = fs::rename(repository.join(".git"), repository.join(".git-old"));
        #[cfg(windows)]
        if replacement.is_err() {
            // The live Windows directory/file pins intentionally deny a
            // parent-directory swap while authority is in flight. That is
            // itself fail-closed; the integration replay tests cover the
            // post-drop replacement path.
            return;
        }
        #[cfg(not(windows))]
        replacement.expect("replace git directory");
        fs::create_dir(repository.join(".git")).expect("replacement git directory");

        assert!(!authorization.binding_is_current());
    }

    #[test]
    fn validated_binding_requires_exact_action_and_runtime_generation() {
        let temp = tempfile::tempdir().expect("validated binding tempdir");
        let repository = temp.path().join("repository");
        fs::create_dir(&repository).expect("repository directory");
        fs::create_dir(repository.join(".git")).expect("git directory");
        fs::write(
            repository.join(".git").join("HEAD"),
            "ref: refs/heads/main\n",
        )
        .expect("repository HEAD");
        let project_id = ProjectId::new();
        let roots = WorkspaceProjectRoots::try_from_pairs([(project_id, repository)])
            .expect("project roots");
        let mut service = WorkspaceService::for_project(project_id, &roots).expect("service");
        let task_id = service.task_id();
        let client_id = ClientId::new();
        let connection_id = Uuid::now_v7();
        let request_id = RequestId::new();
        let command_id = CommandId::new();
        let (binding, authorization) = service
            .bind_authorized_with_generation(
                WorkspaceRequest::main(),
                task_id,
                client_id,
                connection_id,
                request_id,
                command_id,
                7,
                11,
            )
            .expect("workspace authority");

        assert!(authorization
            .validated_binding(
                task_id,
                project_id,
                client_id,
                connection_id,
                request_id,
                command_id,
                binding.durable_ref(),
                7,
                11,
            )
            .is_some());
        assert!(authorization
            .validated_binding(
                task_id,
                project_id,
                client_id,
                connection_id,
                request_id,
                command_id,
                binding.durable_ref(),
                8,
                11,
            )
            .is_none());
        assert!(authorization
            .validated_binding(
                task_id,
                project_id,
                client_id,
                connection_id,
                request_id,
                command_id,
                binding.durable_ref(),
                7,
                12,
            )
            .is_none());
    }
}

#[cfg(test)]
mod lease_security_tests {
    use super::{
        WorkspaceError, WorkspaceLeaseAdmission, WorkspaceLeaseError, WorkspaceService,
        MAX_LEASE_GENERATION_KEYS,
    };
    use crate::domain::{ClientId, CommandId, ProjectId, RequestId, TaskId};
    use crate::workspace::model::{WorkspaceProjectRoots, WorkspaceRequest, WorkspaceResource};
    use std::fs;
    use uuid::Uuid;

    fn service() -> WorkspaceService {
        let root = tempfile::tempdir().expect("lease project root");
        fs::create_dir(root.path().join(".git")).expect("lease repository");
        fs::write(
            root.path().join(".git").join("HEAD"),
            "ref: refs/heads/main\n",
        )
        .expect("lease repository HEAD");
        // Keep the fixture alive for the duration of the test service. The
        // service stores the canonical root, so the tempdir is intentionally
        // leaked only inside this test process.
        let root = root.into_path();
        let project_id = ProjectId::new();
        let roots = WorkspaceProjectRoots::try_from_pairs([(project_id, root)])
            .expect("lease project roots");
        WorkspaceService::for_project(project_id, &roots).expect("lease service")
    }

    #[test]
    fn lease_admission_is_opaque_one_shot_and_tombstones_spent_generations() {
        let service = service();
        let task_id = service.task_id();
        let client_id = ClientId::new();
        let connection_id = Uuid::now_v7();
        let request_id = RequestId::new();
        let command_id = CommandId::new();

        let admission = service
            .issue_resource_admission(
                task_id,
                WorkspaceResource::Process,
                client_id,
                connection_id,
                request_id,
                command_id,
            )
            .expect("host lease admission");
        let generation = admission.generation_for_test();
        let lease = service
            .acquire_resource(admission)
            .expect("one-shot lease admission");
        drop(lease);

        assert!(matches!(
            service.acquire_resource(service.forge_admission_for_test(
                task_id,
                WorkspaceResource::Process,
                client_id,
                connection_id,
                request_id,
                command_id,
                generation,
            )),
            Err(WorkspaceLeaseError::SpentGeneration)
        ));
        assert!(matches!(
            service.acquire_resource(service.forge_admission_for_test(
                task_id,
                WorkspaceResource::Process,
                client_id,
                connection_id,
                request_id,
                command_id,
                generation + 1,
            )),
            Err(WorkspaceLeaseError::InvalidAdmission)
        ));
    }

    #[test]
    fn forged_lease_contexts_fail_for_every_authority_dimension() {
        let workspace_service = service();
        let task_id = workspace_service.task_id();
        let client_id = ClientId::new();
        let connection_id = Uuid::now_v7();
        let request_id = RequestId::new();
        let command_id = CommandId::new();
        let generation = workspace_service
            .issue_resource_admission(
                task_id,
                WorkspaceResource::Process,
                client_id,
                connection_id,
                request_id,
                command_id,
            )
            .expect("host lease admission")
            .generation_for_test();

        let variants = [
            (
                TaskId::new(),
                WorkspaceResource::Process,
                client_id,
                connection_id,
                request_id,
                command_id,
            ),
            (
                task_id,
                WorkspaceResource::File,
                client_id,
                connection_id,
                request_id,
                command_id,
            ),
            (
                task_id,
                WorkspaceResource::Process,
                ClientId::new(),
                connection_id,
                request_id,
                command_id,
            ),
            (
                task_id,
                WorkspaceResource::Process,
                client_id,
                Uuid::now_v7(),
                request_id,
                command_id,
            ),
            (
                task_id,
                WorkspaceResource::Process,
                client_id,
                connection_id,
                RequestId::new(),
                command_id,
            ),
            (
                task_id,
                WorkspaceResource::Process,
                client_id,
                connection_id,
                request_id,
                CommandId::new(),
            ),
        ];
        for (task, resource, client, connection, request, command) in variants {
            assert!(matches!(
                workspace_service.acquire_resource(workspace_service.forge_admission_for_test(
                    task, resource, client, connection, request, command, generation,
                )),
                Err(WorkspaceLeaseError::InvalidAdmission | WorkspaceLeaseError::ScopeMismatch)
            ));
        }

        let other_service = service();
        assert!(matches!(
            other_service.acquire_resource(workspace_service.forge_admission_for_test(
                task_id,
                WorkspaceResource::Process,
                client_id,
                connection_id,
                request_id,
                command_id,
                generation,
            )),
            Err(WorkspaceLeaseError::ScopeMismatch)
        ));
    }

    #[test]
    fn issued_older_generation_is_stale_after_a_newer_generation_is_admitted() {
        let service = service();
        let task_id = service.task_id();
        let client_id = ClientId::new();
        let connection_id = Uuid::now_v7();
        let request_id = RequestId::new();
        let command_id = CommandId::new();
        let first = service
            .issue_resource_admission(
                task_id,
                WorkspaceResource::Process,
                client_id,
                connection_id,
                request_id,
                command_id,
            )
            .expect("first admission");
        let second = service
            .issue_resource_admission(
                task_id,
                WorkspaceResource::Process,
                client_id,
                connection_id,
                request_id,
                command_id,
            )
            .expect("second admission");
        let second_lease = service.acquire_resource(second).expect("newer lease");
        assert!(matches!(
            service.acquire_resource(first),
            Err(WorkspaceLeaseError::StaleGeneration)
        ));
        drop(second_lease);
    }

    #[test]
    fn revoked_admission_cannot_be_reacquired() {
        let service = service();
        let task_id = service.task_id();
        let client_id = ClientId::new();
        let connection_id = Uuid::now_v7();
        let request_id = RequestId::new();
        let command_id = CommandId::new();
        let admission = service
            .issue_resource_admission(
                task_id,
                WorkspaceResource::Process,
                client_id,
                connection_id,
                request_id,
                command_id,
            )
            .expect("revocable admission");
        let generation = admission.generation_for_test();
        service.revoke_resource(admission);

        assert!(matches!(
            service.acquire_resource(service.forge_admission_for_test(
                task_id,
                WorkspaceResource::Process,
                client_id,
                connection_id,
                request_id,
                command_id,
                generation,
            )),
            Err(WorkspaceLeaseError::Revoked)
        ));
    }

    #[test]
    fn live_resource_blocks_binding_until_the_opaque_lease_is_dropped() {
        let mut service = service();
        let task_id = service.task_id();
        let admission = service
            .issue_resource_admission(
                task_id,
                WorkspaceResource::Process,
                ClientId::new(),
                Uuid::now_v7(),
                RequestId::new(),
                CommandId::new(),
            )
            .expect("host admission");
        let lease = service.acquire_resource(admission).expect("lease");
        assert!(matches!(
            service.bind(WorkspaceRequest::main()),
            Err(WorkspaceError::LiveResources(_))
        ));
        drop(lease);
        service
            .bind(WorkspaceRequest::main())
            .expect("binding after lease release");
    }

    #[test]
    fn lease_debug_surface_is_bounded_and_does_not_echo_scope_ids() {
        let service = service();
        let task_id = service.task_id();
        let admission = service
            .issue_resource_admission(
                task_id,
                WorkspaceResource::Process,
                ClientId::new(),
                Uuid::now_v7(),
                RequestId::new(),
                CommandId::new(),
            )
            .expect("host lease admission");
        let lease = service.acquire_resource(admission).expect("lease");
        let rendered = format!("{lease:?}");
        assert!(rendered.len() <= 64);
        assert_eq!(rendered, "WorkspaceResourceLease(REDACTED)");
    }

    #[test]
    fn repeated_identical_lease_keys_cannot_grow_generation_state_without_bound() {
        let service = service();
        let task_id = service.task_id();
        let client_id = ClientId::new();
        let connection_id = Uuid::now_v7();
        let request_id = RequestId::new();
        let command_id = CommandId::new();

        for _ in 0..=MAX_LEASE_GENERATION_KEYS {
            let _ = service.issue_resource_admission(
                task_id,
                WorkspaceResource::Process,
                client_id,
                connection_id,
                request_id,
                command_id,
            );
        }

        let state = service
            .coordinator
            .live_resources
            .lock()
            .expect("lease state");
        let retained = state
            .active
            .len()
            .saturating_add(state.issued.len())
            .saturating_add(state.spent.len())
            .saturating_add(state.revoked.len());
        assert!(
            retained <= MAX_LEASE_GENERATION_KEYS,
            "lease authority state exceeded its bound: {retained}"
        );
    }

    #[allow(dead_code)]
    fn _opaque_type_is_not_publicly_serializable(_: WorkspaceLeaseAdmission) {}
}

type LiveResourceCounts = Arc<Mutex<LeaseState>>;

const MAX_LEASE_GENERATION_KEYS: usize = 4_096;
const LEASE_ADMISSION_TTL: std::time::Duration = std::time::Duration::from_secs(5 * 60);

#[derive(Clone, Copy, PartialEq, Eq, Ord, PartialOrd)]
struct LeaseGenerationKey {
    workspace_identity: [u8; 32],
    task_id: TaskId,
    resource: WorkspaceResource,
    client_id: ClientId,
    connection_id: Uuid,
    request_id: RequestId,
    command_id: CommandId,
    action_epoch: u64,
    runtime_generation: u64,
}

#[derive(Clone, Copy, PartialEq, Eq, Ord, PartialOrd)]
struct LeaseScope {
    coordinator_id: Uuid,
    generation_key: LeaseGenerationKey,
    generation: u64,
}

struct WorkspaceLeaseAdmission {
    scope: LeaseScope,
}

impl std::fmt::Debug for WorkspaceLeaseAdmission {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("WorkspaceLeaseAdmission(REDACTED)")
    }
}

impl WorkspaceLeaseAdmission {
    #[cfg(test)]
    fn generation_for_test(&self) -> u64 {
        self.scope.generation
    }
}

#[derive(Default)]
struct LeaseState {
    active: BTreeMap<LeaseScope, usize>,
    latest_generation: BTreeMap<LeaseGenerationKey, u64>,
    issued: BTreeSet<LeaseScope>,
    spent: BTreeSet<LeaseScope>,
    revoked: BTreeSet<LeaseScope>,
    issued_at: BTreeMap<LeaseScope, std::time::Instant>,
}

/// Shared counted lease state for all workspace services that represent one
/// host runtime. It is injected explicitly so independent service instances
/// cannot rebind around a lease held by a sibling thread.
pub struct WorkspaceResourceCoordinator {
    coordinator_id: Uuid,
    live_resources: LiveResourceCounts,
}

impl std::fmt::Debug for WorkspaceResourceCoordinator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("WorkspaceResourceCoordinator(REDACTED)")
    }
}

impl Clone for WorkspaceResourceCoordinator {
    fn clone(&self) -> Self {
        Self {
            coordinator_id: self.coordinator_id,
            live_resources: Arc::clone(&self.live_resources),
        }
    }
}

impl Default for WorkspaceResourceCoordinator {
    fn default() -> Self {
        Self {
            coordinator_id: Uuid::now_v7(),
            live_resources: Arc::new(Mutex::new(LeaseState::default())),
        }
    }
}

impl WorkspaceResourceCoordinator {
    pub fn new() -> Self {
        Self::default()
    }

    fn issue(
        &self,
        workspace_identity: [u8; 32],
        task_id: TaskId,
        resource: WorkspaceResource,
        client_id: ClientId,
        connection_id: Uuid,
        request_id: RequestId,
        command_id: CommandId,
        action_epoch: u64,
        runtime_generation: u64,
    ) -> Result<WorkspaceLeaseAdmission, WorkspaceLeaseError> {
        let now = std::time::Instant::now();
        let generation_key = LeaseGenerationKey {
            workspace_identity,
            task_id,
            resource,
            client_id,
            connection_id,
            request_id,
            command_id,
            action_epoch,
            runtime_generation,
        };
        let mut state = self
            .live_resources
            .lock()
            .expect("workspace resource lease state poisoned");
        state.evict_expired(now);
        if !state.latest_generation.contains_key(&generation_key)
            && state.latest_generation.len() >= MAX_LEASE_GENERATION_KEYS
        {
            return Err(WorkspaceLeaseError::InvalidAdmission);
        }
        // Distinct-key admission is not the only way this state can grow: a
        // caller can repeatedly issue the exact same key and retain every
        // one-shot generation in `issued`.  Keep a bound over the union of all
        // retained scopes as well.  Existing issued, active, spent, and
        // revoked scopes are never evicted here, so an authority cannot be
        // silently revoked or later revived by capacity pressure.
        if state.generation_scope_count() >= MAX_LEASE_GENERATION_KEYS {
            return Err(WorkspaceLeaseError::InvalidAdmission);
        }
        let generation = state
            .latest_generation
            .get(&generation_key)
            .copied()
            .unwrap_or(0)
            .checked_add(1)
            .ok_or(WorkspaceLeaseError::InvalidGeneration)?;
        let scope = LeaseScope {
            coordinator_id: self.coordinator_id,
            generation_key,
            generation,
        };
        state.latest_generation.insert(generation_key, generation);
        state.issued.insert(scope);
        state.issued_at.insert(scope, now);
        Ok(WorkspaceLeaseAdmission { scope })
    }

    fn acquire(
        &self,
        admission: WorkspaceLeaseAdmission,
    ) -> Result<WorkspaceResourceLease, WorkspaceLeaseError> {
        let scope = admission.scope;
        if scope.coordinator_id != self.coordinator_id {
            return Err(WorkspaceLeaseError::ScopeMismatch);
        }
        if scope.generation == 0 {
            return Err(WorkspaceLeaseError::InvalidGeneration);
        }
        let mut state = self
            .live_resources
            .lock()
            .expect("workspace resource lease state poisoned");
        if state.revoked.contains(&scope) {
            return Err(WorkspaceLeaseError::Revoked);
        }
        if state
            .issued_at
            .get(&scope)
            .is_some_and(|issued| issued.elapsed() >= LEASE_ADMISSION_TTL)
        {
            state.issued.remove(&scope);
            state.issued_at.remove(&scope);
            state.revoked.insert(scope);
            return Err(WorkspaceLeaseError::Revoked);
        }
        if state.spent.contains(&scope) {
            return Err(WorkspaceLeaseError::SpentGeneration);
        }
        if !state.issued.remove(&scope) {
            return Err(WorkspaceLeaseError::InvalidAdmission);
        }
        state.issued_at.remove(&scope);
        let latest = state
            .latest_generation
            .get(&scope.generation_key)
            .copied()
            .unwrap_or(0);
        if scope.generation < latest {
            return Err(WorkspaceLeaseError::StaleGeneration);
        }
        if scope.generation > latest {
            return Err(WorkspaceLeaseError::InvalidAdmission);
        }
        state.spent.insert(scope);
        *state.active.entry(scope).or_default() += 1;
        Ok(WorkspaceResourceLease {
            coordinator: self.clone(),
            scope,
        })
    }

    fn release(&self, scope: LeaseScope) {
        let Ok(mut state) = self.live_resources.lock() else {
            return;
        };
        let Some(count) = state.active.get_mut(&scope) else {
            return;
        };
        if *count <= 1 {
            state.active.remove(&scope);
        } else {
            *count -= 1;
        }
    }

    fn ensure_active(&self, scope: LeaseScope) -> Result<(), WorkspaceLeaseError> {
        let state = self
            .live_resources
            .lock()
            .expect("workspace resource lease state poisoned");
        if scope.coordinator_id != self.coordinator_id {
            return Err(WorkspaceLeaseError::ScopeMismatch);
        }
        if state.revoked.contains(&scope) {
            return Err(WorkspaceLeaseError::Revoked);
        }
        if state.active.get(&scope).copied().unwrap_or(0) == 0 {
            return Err(WorkspaceLeaseError::Revoked);
        }
        Ok(())
    }

    fn revoke(&self, scope: LeaseScope) {
        let Ok(mut state) = self.live_resources.lock() else {
            return;
        };
        state.revoked.insert(scope);
        state.issued.remove(&scope);
        state.issued_at.remove(&scope);
        state.active.remove(&scope);
    }

    fn live_resources_for_task(&self, task_id: TaskId) -> Vec<WorkspaceResource> {
        let state = self
            .live_resources
            .lock()
            .expect("workspace resource lease state poisoned");
        state
            .active
            .keys()
            .filter(|scope| scope.generation_key.task_id == task_id)
            .map(|scope| scope.generation_key.resource)
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect()
    }
}

impl LeaseState {
    fn generation_scope_count(&self) -> usize {
        self.active
            .keys()
            .chain(self.issued.iter())
            .chain(self.spent.iter())
            .chain(self.revoked.iter())
            .copied()
            .collect::<BTreeSet<_>>()
            .len()
    }

    fn evict_expired(&mut self, now: std::time::Instant) {
        let expired = self
            .issued_at
            .iter()
            .filter(|(_, issued)| now.duration_since(**issued) >= LEASE_ADMISSION_TTL)
            .map(|(scope, _)| *scope)
            .collect::<Vec<_>>();
        for scope in expired {
            self.issued.remove(&scope);
            self.issued_at.remove(&scope);
            // An expired one-shot admission is permanently revoked. Keep the
            // tombstone until a newer generation makes it stale.
            self.revoked.insert(scope);
        }
        let stale = self
            .revoked
            .iter()
            .chain(self.spent.iter())
            .copied()
            .filter(|scope| {
                self.latest_generation
                    .get(&scope.generation_key)
                    .is_some_and(|latest| *latest > scope.generation)
                    && !self.active.contains_key(scope)
                    && !self.issued.contains(scope)
            })
            .collect::<BTreeSet<_>>();
        for scope in stale {
            self.revoked.remove(&scope);
            self.spent.remove(&scope);
        }
    }
}

/// Host-owned resolver and immutable task workspace binding.
pub struct WorkspaceService {
    task_id: TaskId,
    project_id: ProjectId,
    project_root: PathBuf,
    project_root_identity: String,
    binding: Option<WorkspaceBinding>,
    coordinator: WorkspaceResourceCoordinator,
}

/// An owned lease for one live workspace resource.
pub struct WorkspaceResourceLease {
    coordinator: WorkspaceResourceCoordinator,
    scope: LeaseScope,
}

impl std::fmt::Debug for WorkspaceResourceLease {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("WorkspaceResourceLease(REDACTED)")
    }
}

impl WorkspaceResourceLease {
    pub fn resource(&self) -> WorkspaceResource {
        self.scope.generation_key.resource
    }

    pub fn ensure_active(&self) -> Result<(), WorkspaceLeaseError> {
        self.coordinator.ensure_active(self.scope)
    }
}

impl Drop for WorkspaceResourceLease {
    fn drop(&mut self) {
        self.coordinator.release(self.scope);
    }
}

struct IssuedTask6WorkspaceLease {
    pin: WorkspacePinnedPath,
    handle: fs::File,
    write_handle: Option<fs::File>,
    lease: Option<WorkspaceResourceLease>,
    workspace_lease: [u8; 16],
    task_id: [u8; 16],
    client_id: [u8; 16],
    connection_id: [u8; 16],
    action_epoch: u64,
}

struct IssuedTask6LeaseHolder {
    lease: WorkspaceResourceLease,
}

impl super::files::Task6LiveLeaseGuard for IssuedTask6LeaseHolder {
    fn ensure_active(&self) -> bool {
        self.lease.ensure_active().is_ok()
    }
}

impl super::files::task6_bridge::Sealed for IssuedTask6WorkspaceLease {}

impl super::files::Task6WorkspaceLease for IssuedTask6WorkspaceLease {
    fn retained_root_path(&self) -> &Path {
        self.pin.path()
    }

    fn retained_root_handle(&self) -> &fs::File {
        &self.handle
    }

    fn retained_root_write_handle(&self) -> Option<&fs::File> {
        self.write_handle.as_ref()
    }

    fn workspace_lease(&self) -> [u8; 16] {
        self.workspace_lease
    }

    fn task_id(&self) -> [u8; 16] {
        self.task_id
    }

    fn client_id(&self) -> [u8; 16] {
        self.client_id
    }

    fn connection_id(&self) -> [u8; 16] {
        self.connection_id
    }

    fn action_epoch(&self) -> u64 {
        self.action_epoch
    }

    fn take_live_lease_guard(&mut self) -> super::files::OpaqueTask6LeaseGuard {
        match self.lease.take() {
            Some(lease) => {
                super::files::OpaqueTask6LeaseGuard::from_live(Box::new(IssuedTask6LeaseHolder {
                    lease,
                }))
            }
            None => super::files::OpaqueTask6LeaseGuard::default(),
        }
    }
}

/// Issue a production [`WorkspaceFileService`] from a live File lease and
/// revalidated workspace authorization. Path-only construction is rejected.
pub(crate) fn issue_file_service(
    authorization: &WorkspaceAuthorization,
    lease: WorkspaceResourceLease,
    task_id: TaskId,
    project_id: ProjectId,
    client_id: ClientId,
    connection_id: Uuid,
    request_id: RequestId,
    command_id: CommandId,
    workspace: &WorkspaceRef,
    action_epoch: u64,
    runtime_generation: u64,
) -> Result<super::files::WorkspaceFileService, super::files::FileServiceError> {
    issue_file_service_with_access(
        authorization,
        lease,
        task_id,
        project_id,
        client_id,
        connection_id,
        request_id,
        command_id,
        workspace,
        action_epoch,
        runtime_generation,
        FileServiceAccess::ReadWrite,
    )
}

/// Issue a production file service that can list and read but cannot mutate.
/// This intentionally skips mutation-recovery discovery so a bounded read is
/// available even when a large workspace exceeds the recovery scan bound.
pub(crate) fn issue_read_file_service(
    authorization: &WorkspaceAuthorization,
    lease: WorkspaceResourceLease,
    task_id: TaskId,
    project_id: ProjectId,
    client_id: ClientId,
    connection_id: Uuid,
    request_id: RequestId,
    command_id: CommandId,
    workspace: &WorkspaceRef,
    action_epoch: u64,
    runtime_generation: u64,
) -> Result<super::files::WorkspaceFileService, super::files::FileServiceError> {
    issue_file_service_with_access(
        authorization,
        lease,
        task_id,
        project_id,
        client_id,
        connection_id,
        request_id,
        command_id,
        workspace,
        action_epoch,
        runtime_generation,
        FileServiceAccess::ReadOnly,
    )
}

#[derive(Clone, Copy)]
enum FileServiceAccess {
    ReadOnly,
    ReadWrite,
}

#[allow(clippy::too_many_arguments)]
fn issue_file_service_with_access(
    authorization: &WorkspaceAuthorization,
    lease: WorkspaceResourceLease,
    task_id: TaskId,
    project_id: ProjectId,
    client_id: ClientId,
    connection_id: Uuid,
    request_id: RequestId,
    command_id: CommandId,
    workspace: &WorkspaceRef,
    action_epoch: u64,
    runtime_generation: u64,
    access: FileServiceAccess,
) -> Result<super::files::WorkspaceFileService, super::files::FileServiceError> {
    if lease.resource() != WorkspaceResource::File {
        return Err(super::files::FileServiceError::AuthorityUnavailable);
    }
    lease
        .ensure_active()
        .map_err(|_| super::files::FileServiceError::AuthorityUnavailable)?;
    let binding = authorization
        .validated_binding(
            task_id,
            project_id,
            client_id,
            connection_id,
            request_id,
            command_id,
            workspace,
            action_epoch,
            runtime_generation,
        )
        .ok_or(super::files::FileServiceError::AuthorityUnavailable)?;
    let fact = binding
        .durable_ref()
        .host_binding()
        .ok_or(super::files::FileServiceError::AuthorityUnavailable)?;
    let pin = authorization
        .retained_pin_for_fact(fact.workspace_root())
        .ok_or(super::files::FileServiceError::RootUnavailable)?;
    if !pin.is_dir() {
        return Err(super::files::FileServiceError::RootUnavailable);
    }
    let handle = pin
        .handle()
        .try_clone()
        .map_err(|_| super::files::FileServiceError::RootUnavailable)?;
    let write_handle = match access {
        FileServiceAccess::ReadOnly => None,
        FileServiceAccess::ReadWrite => pin
            .write_handle()
            .and_then(|handle| handle.try_clone().ok()),
    };
    let lease = IssuedTask6WorkspaceLease {
        pin,
        handle,
        write_handle,
        lease: Some(lease),
        workspace_lease: *request_id.as_bytes(),
        task_id: *task_id.as_bytes(),
        client_id: *client_id.as_bytes(),
        connection_id: *connection_id.as_bytes(),
        action_epoch,
    };
    match access {
        FileServiceAccess::ReadOnly => {
            super::files::WorkspaceFileService::from_task6_read_workspace(lease)
        }
        FileServiceAccess::ReadWrite => {
            super::files::WorkspaceFileService::from_task6_workspace(lease)
        }
    }
}

impl WorkspaceService {
    pub fn for_project(
        project_id: ProjectId,
        workspace_projects: &WorkspaceProjectRoots,
    ) -> Result<Self, WorkspaceError> {
        Self::with_coordinator(
            project_id,
            workspace_projects,
            WorkspaceResourceCoordinator::new(),
        )
    }

    pub fn with_coordinator(
        project_id: ProjectId,
        workspace_projects: &WorkspaceProjectRoots,
        coordinator: WorkspaceResourceCoordinator,
    ) -> Result<Self, WorkspaceError> {
        Self::with_task_coordinator(project_id, TaskId::new(), workspace_projects, coordinator)
    }

    pub(crate) fn with_task_coordinator(
        project_id: ProjectId,
        task_id: TaskId,
        workspace_projects: &WorkspaceProjectRoots,
        coordinator: WorkspaceResourceCoordinator,
    ) -> Result<Self, WorkspaceError> {
        let configured = workspace_projects
            .configured_root_for(project_id)
            .ok_or(WorkspaceError::ProjectNotConfigured(project_id))?;
        Ok(Self {
            task_id,
            project_id,
            project_root: configured.path().to_path_buf(),
            project_root_identity: configured.identity().to_string(),
            binding: None,
            coordinator,
        })
    }

    /// Rebuild host-owned metadata from the task's durable workspace value.
    /// The metadata remains a derived projection and is never serialized.
    pub fn from_durable(
        project_id: ProjectId,
        workspace_projects: &WorkspaceProjectRoots,
        durable_ref: &WorkspaceRef,
    ) -> Result<Self, WorkspaceError> {
        Self::from_durable_with_coordinator(
            project_id,
            workspace_projects,
            durable_ref,
            WorkspaceResourceCoordinator::new(),
        )
    }

    pub fn from_durable_with_coordinator(
        project_id: ProjectId,
        workspace_projects: &WorkspaceProjectRoots,
        durable_ref: &WorkspaceRef,
        coordinator: WorkspaceResourceCoordinator,
    ) -> Result<Self, WorkspaceError> {
        Self::from_durable_with_task_coordinator(
            project_id,
            TaskId::new(),
            workspace_projects,
            durable_ref,
            coordinator,
        )
    }

    pub(crate) fn from_durable_with_task_coordinator(
        project_id: ProjectId,
        task_id: TaskId,
        workspace_projects: &WorkspaceProjectRoots,
        durable_ref: &WorkspaceRef,
        coordinator: WorkspaceResourceCoordinator,
    ) -> Result<Self, WorkspaceError> {
        let mut service =
            Self::with_task_coordinator(project_id, task_id, workspace_projects, coordinator)?;
        let binding = service.resolve_durable(durable_ref)?;
        service.binding = Some(binding);
        Ok(service)
    }

    pub fn resolve(
        &self,
        request: WorkspaceRequest,
    ) -> Result<WorkspaceResolution, WorkspaceError> {
        let project_root = self.canonical_configured_project_root()?;
        match request.choice {
            WorkspaceChoice::Main => {
                let repository = discover_repository(&project_root)
                    .ok_or_else(|| WorkspaceError::NotRepository(project_root.clone()))?;
                if !same_path_identity(&project_root, repository.root()) {
                    return Err(WorkspaceError::MainRootMismatch {
                        configured_root: project_root,
                        repository_root: repository.root().to_path_buf(),
                    });
                }
                Ok(WorkspaceResolution::Resolved(build_binding(
                    WorkspaceKind::Main,
                    project_root.clone(),
                    None,
                    WorkspaceRef::Main,
                    Some(repository),
                    &project_root,
                )?))
            }
            WorkspaceChoice::NewWorktree => {
                let path = request.path.ok_or(WorkspaceError::MissingWorktreePath)?;
                let branch = request
                    .branch
                    .filter(|value| !value.trim().is_empty())
                    .ok_or(WorkspaceError::MissingWorktreeBranch)?;
                let project_repository = discover_repository(&project_root)
                    .ok_or_else(|| WorkspaceError::NotRepository(project_root.clone()))?;
                if path_contains_link_or_reparse_point(&path) {
                    return Err(WorkspaceError::OutsideProject {
                        path: path.clone(),
                        project_root: project_repository.root().to_path_buf(),
                    });
                }
                let (final_path, exists) = canonical_worktree_candidate(&path)?;
                if !is_within(project_repository.root(), &final_path) {
                    return Err(WorkspaceError::OutsideProject {
                        path: final_path,
                        project_root: project_repository.root().to_path_buf(),
                    });
                }

                if exists {
                    if let Some((repository, actual_branch)) = linked_worktree_details(&final_path)?
                    {
                        if repository.key() != project_repository.key() {
                            return Err(WorkspaceError::DifferentRepository(final_path));
                        }
                        if actual_branch != branch {
                            return Err(WorkspaceError::WorktreeBranchMismatch {
                                path: final_path,
                                requested: branch,
                                actual: actual_branch,
                            });
                        }
                        let durable_ref =
                            WorkspaceRef::worktree(final_path.clone(), actual_branch.clone())
                                .map_err(|_| WorkspaceError::InvalidBranch)?;
                        return Ok(WorkspaceResolution::Resolved(build_binding(
                            WorkspaceKind::Worktree,
                            final_path,
                            Some(actual_branch),
                            durable_ref,
                            Some(repository),
                            &project_root,
                        )?));
                    }
                }

                Ok(WorkspaceResolution::PendingWorktree(
                    PendingWorktreeCandidate {
                        path: final_path.clone(),
                        branch,
                        repository: project_repository.clone(),
                        relative_worktree_path: relative_location(
                            project_repository.root(),
                            &final_path,
                        ),
                    },
                ))
            }
            WorkspaceChoice::Ask => Err(WorkspaceError::ChoiceRequired),
            WorkspaceChoice::External => {
                if !request.external_confirmed {
                    return Err(WorkspaceError::ExternalConfirmationRequired);
                }
                let path = request
                    .path
                    .ok_or(WorkspaceError::PathDoesNotExist(PathBuf::new()))?;
                let final_path = canonical_existing_dir(&path)?;
                let repository = discover_repository(&final_path);
                let durable_ref = WorkspaceRef::external(final_path.clone())
                    .map_err(|_| WorkspaceError::PathDoesNotExist(final_path.clone()))?;
                Ok(WorkspaceResolution::Resolved(build_binding(
                    WorkspaceKind::External,
                    final_path,
                    None,
                    durable_ref,
                    repository,
                    &project_root,
                )?))
            }
        }
    }

    pub fn bind(&mut self, request: WorkspaceRequest) -> Result<WorkspaceBinding, WorkspaceError> {
        self.ensure_no_live_resources()?;
        let candidate = match self.resolve(request)? {
            WorkspaceResolution::Resolved(candidate) => candidate,
            WorkspaceResolution::PendingWorktree(candidate) => {
                return Err(WorkspaceError::PendingWorktree(candidate))
            }
        };
        match &self.binding {
            None => {
                self.binding = Some(candidate.clone());
                Ok(candidate)
            }
            Some(current) if current.same_workspace(&candidate) => Ok(current.clone()),
            Some(_) => Err(WorkspaceError::WorkspaceImmutable),
        }
    }

    /// Resolve and bind a request, returning the opaque authority required to
    /// persist the resulting durable workspace reference.
    pub(crate) fn bind_authorized(
        &mut self,
        request: WorkspaceRequest,
        task_id: TaskId,
        client_id: ClientId,
        connection_id: Uuid,
        request_id: RequestId,
        command_id: CommandId,
    ) -> Result<(WorkspaceBinding, WorkspaceAuthorization), WorkspaceError> {
        self.bind_authorized_with_generation(
            request,
            task_id,
            client_id,
            connection_id,
            request_id,
            command_id,
            0,
            0,
        )
    }

    /// Phase 6.1's ConfigStore issuer supplies the current action epoch and
    /// runtime generation through this seam. Until that issuer is wired, the
    /// compatibility method above deliberately mints only generation zero.
    pub(crate) fn bind_authorized_with_generation(
        &mut self,
        request: WorkspaceRequest,
        task_id: TaskId,
        client_id: ClientId,
        connection_id: Uuid,
        request_id: RequestId,
        command_id: CommandId,
        action_epoch: u64,
        runtime_generation: u64,
    ) -> Result<(WorkspaceBinding, WorkspaceAuthorization), WorkspaceError> {
        if task_id != self.task_id {
            return Err(WorkspaceError::PathResolution {
                path: self.project_root.clone(),
                reason: "task admission does not belong to this service".into(),
            });
        }
        let binding = self.bind(request)?;
        let fact = binding
            .durable_ref()
            .host_binding()
            .ok_or(WorkspaceError::RebindRequired)?;
        let pins = open_fact_pins(fact)?;
        let authorization = WorkspaceAuthorization::new_with_generation(
            self.project_id,
            task_id,
            client_id,
            connection_id,
            request_id,
            command_id,
            &self.project_root,
            &self.project_root_identity,
            &binding,
            pins,
            action_epoch,
            runtime_generation,
        );
        if !authorization.binding_is_current() {
            return Err(WorkspaceError::PathResolution {
                path: binding.path().to_path_buf(),
                reason: "workspace identity changed during authorization".into(),
            });
        }
        Ok((binding, authorization))
    }

    /// Revalidate the currently bound workspace against the exact Task
    /// snapshot and host admission identity. Path strings never authorize.
    pub(crate) fn authorize_current_with_generation(
        &self,
        expected_workspace: &WorkspaceRef,
        task_id: TaskId,
        client_id: ClientId,
        connection_id: Uuid,
        request_id: RequestId,
        command_id: CommandId,
        action_epoch: u64,
        runtime_generation: u64,
    ) -> Result<WorkspaceAuthorization, WorkspaceError> {
        if task_id != self.task_id {
            return Err(WorkspaceError::PathResolution {
                path: self.project_root.clone(),
                reason: "task admission does not belong to this service".into(),
            });
        }
        let binding = self.current().ok_or(WorkspaceError::RebindRequired)?;
        if !durable_refs_same_location(binding.durable_ref(), expected_workspace) {
            return Err(WorkspaceError::RebindRequired);
        }
        let fact = binding
            .durable_ref()
            .host_binding()
            .ok_or(WorkspaceError::RebindRequired)?;
        let pins = open_fact_pins(fact)?;
        let authorization = WorkspaceAuthorization::new_with_generation(
            self.project_id,
            task_id,
            client_id,
            connection_id,
            request_id,
            command_id,
            &self.project_root,
            &self.project_root_identity,
            binding,
            pins,
            action_epoch,
            runtime_generation,
        );
        if authorization
            .validated_binding(
                task_id,
                self.project_id,
                client_id,
                connection_id,
                request_id,
                command_id,
                expected_workspace,
                action_epoch,
                runtime_generation,
            )
            .is_none()
        {
            return Err(WorkspaceError::RebindRequired);
        }
        Ok(authorization)
    }

    pub fn current(&self) -> Option<&WorkspaceBinding> {
        self.binding.as_ref()
    }

    fn task_id(&self) -> TaskId {
        self.task_id
    }

    fn issue_resource_admission(
        &self,
        task_id: TaskId,
        resource: WorkspaceResource,
        client_id: ClientId,
        connection_id: Uuid,
        request_id: RequestId,
        command_id: CommandId,
    ) -> Result<WorkspaceLeaseAdmission, WorkspaceLeaseError> {
        self.issue_resource_admission_with_generation(
            task_id,
            resource,
            client_id,
            connection_id,
            request_id,
            command_id,
            0,
            0,
        )
    }

    fn issue_resource_admission_with_generation(
        &self,
        task_id: TaskId,
        resource: WorkspaceResource,
        client_id: ClientId,
        connection_id: Uuid,
        request_id: RequestId,
        command_id: CommandId,
        action_epoch: u64,
        runtime_generation: u64,
    ) -> Result<WorkspaceLeaseAdmission, WorkspaceLeaseError> {
        if task_id != self.task_id {
            return Err(WorkspaceLeaseError::ScopeMismatch);
        }
        if !configured_root_identity_matches(&self.project_root, &self.project_root_identity) {
            return Err(WorkspaceLeaseError::InvalidAdmission);
        }
        let workspace_identity = workspace_identity_token(&self.project_root)
            .ok_or(WorkspaceLeaseError::InvalidAdmission)?;
        self.coordinator.issue(
            workspace_identity,
            task_id,
            resource,
            client_id,
            connection_id,
            request_id,
            command_id,
            action_epoch,
            runtime_generation,
        )
    }

    #[cfg(test)]
    fn forge_admission_for_test(
        &self,
        task_id: TaskId,
        resource: WorkspaceResource,
        client_id: ClientId,
        connection_id: Uuid,
        request_id: RequestId,
        command_id: CommandId,
        generation: u64,
    ) -> WorkspaceLeaseAdmission {
        WorkspaceLeaseAdmission {
            scope: LeaseScope {
                coordinator_id: self.coordinator.coordinator_id,
                generation_key: LeaseGenerationKey {
                    workspace_identity: workspace_identity_token(&self.project_root)
                        .unwrap_or([0; 32]),
                    task_id,
                    resource,
                    client_id,
                    connection_id,
                    request_id,
                    command_id,
                    action_epoch: 0,
                    runtime_generation: 0,
                },
                generation,
            },
        }
    }

    fn acquire_resource(
        &self,
        admission: WorkspaceLeaseAdmission,
    ) -> Result<WorkspaceResourceLease, WorkspaceLeaseError> {
        if admission.scope.coordinator_id != self.coordinator.coordinator_id {
            return Err(WorkspaceLeaseError::ScopeMismatch);
        }
        if admission.scope.generation_key.task_id != self.task_id {
            return Err(WorkspaceLeaseError::ScopeMismatch);
        }
        self.coordinator.acquire(admission)
    }

    /// Issue and acquire one live resource lease for the exact Task, client,
    /// connection, request, command, and generation tuple.
    pub(crate) fn acquire_task_resource(
        &self,
        task_id: TaskId,
        resource: WorkspaceResource,
        client_id: ClientId,
        connection_id: Uuid,
        request_id: RequestId,
        command_id: CommandId,
        action_epoch: u64,
        runtime_generation: u64,
    ) -> Result<WorkspaceResourceLease, WorkspaceLeaseError> {
        let admission = self.issue_resource_admission_with_generation(
            task_id,
            resource,
            client_id,
            connection_id,
            request_id,
            command_id,
            action_epoch,
            runtime_generation,
        )?;
        self.acquire_resource(admission)
    }

    fn revoke_resource(&self, admission: WorkspaceLeaseAdmission) {
        if admission.scope.coordinator_id == self.coordinator.coordinator_id
            && admission.scope.generation_key.task_id == self.task_id
        {
            self.coordinator.revoke(admission.scope);
        }
    }

    #[cfg(test)]
    pub(crate) fn live_resources_for_task_for_test(&self) -> Vec<WorkspaceResource> {
        self.coordinator.live_resources_for_task(self.task_id)
    }

    fn ensure_no_live_resources(&self) -> Result<(), WorkspaceError> {
        let resources = self.coordinator.live_resources_for_task(self.task_id);
        if resources.is_empty() {
            Ok(())
        } else {
            Err(WorkspaceError::LiveResources(resources))
        }
    }

    pub fn close_and_rebind(
        &mut self,
        request: WorkspaceRequest,
    ) -> Result<WorkspaceBinding, WorkspaceError> {
        self.ensure_no_live_resources()?;
        let candidate = match self.resolve(request)? {
            WorkspaceResolution::Resolved(candidate) => candidate,
            WorkspaceResolution::PendingWorktree(candidate) => {
                return Err(WorkspaceError::PendingWorktree(candidate))
            }
        };
        self.binding = Some(candidate.clone());
        Ok(candidate)
    }

    fn resolve_durable(
        &self,
        durable_ref: &WorkspaceRef,
    ) -> Result<WorkspaceBinding, WorkspaceError> {
        let expected = durable_ref
            .host_binding()
            .ok_or(WorkspaceError::RebindRequired)?;
        expected
            .validate()
            .map_err(|_| WorkspaceError::RebindRequired)?;
        // Durable projections omit live paths. Re-resolve from the host-owned
        // project root already admitted on this service, then match the opaque
        // binding fingerprint. Never reconstruct a workspace from a serialized
        // path.
        let request = match expected.kind() {
            WorkspaceBindingKind::Main => WorkspaceRequest::main(),
            WorkspaceBindingKind::Worktree => {
                let path = expected.workspace_root().path();
                if path.as_os_str().is_empty() {
                    return Err(WorkspaceError::RebindRequired);
                }
                WorkspaceRequest::new_worktree(
                    path,
                    expected.branch().ok_or(WorkspaceError::RebindRequired)?,
                )
            }
            WorkspaceBindingKind::External => {
                let path = expected.workspace_root().path();
                if path.as_os_str().is_empty() {
                    WorkspaceRequest::confirmed_external(&self.project_root)
                } else {
                    WorkspaceRequest::confirmed_external(path)
                }
            }
        };
        let WorkspaceResolution::Resolved(binding) = self.resolve(request)? else {
            return Err(WorkspaceError::RebindRequired);
        };
        let actual = binding
            .durable_ref()
            .host_binding()
            .ok_or(WorkspaceError::RebindRequired)?;
        if actual.binding_fingerprint() == expected.binding_fingerprint() {
            Ok(binding)
        } else {
            Err(WorkspaceError::RepositoryFingerprintMismatch {
                path: self.project_root.clone(),
                expected: expected.binding_fingerprint().clone(),
                actual: actual.binding_fingerprint().clone(),
            })
        }
    }
}

fn build_binding(
    kind: WorkspaceKind,
    path: PathBuf,
    branch: Option<String>,
    _durable_ref: WorkspaceRef,
    repository: Option<RepositoryIdentity>,
    project_root: &Path,
) -> Result<WorkspaceBinding, WorkspaceError> {
    let (fact, _) = capture_binding_fact(kind, project_root, &path, branch.as_deref())?;
    let durable_ref = match kind {
        WorkspaceKind::External => WorkspaceRef::ExternalWithFingerprint {
            path: path.clone(),
            binding: fact,
        },
        WorkspaceKind::Main | WorkspaceKind::Worktree => WorkspaceRef::HostBound { binding: fact },
    };
    let relative_worktree_path = repository
        .as_ref()
        .and_then(|repository| relative_location(repository.root(), &path));
    let identity_key =
        stable_path_identity_key(&path).ok_or_else(|| WorkspaceError::PathResolution {
            path: path.clone(),
            reason: "workspace identity changed during binding".into(),
        })?;
    Ok(WorkspaceBinding::new(
        kind,
        path,
        identity_key,
        durable_ref,
        repository,
        relative_worktree_path,
        branch,
    ))
}

fn capture_binding_fact(
    kind: WorkspaceKind,
    project_root: &Path,
    workspace_path: &Path,
    branch: Option<&str>,
) -> Result<(WorkspaceBindingFact, Vec<PinnedPath>), WorkspaceError> {
    let project_root_pin =
        PinnedPath::open(project_root).map_err(|_| WorkspaceError::PathResolution {
            path: project_root.to_path_buf(),
            reason: "project root could not be pinned".into(),
        })?;
    let workspace_root_pin =
        PinnedPath::open(workspace_path).map_err(|_| WorkspaceError::PathResolution {
            path: workspace_path.to_path_buf(),
            reason: "workspace root could not be pinned".into(),
        })?;
    if !project_root_pin.is_dir || !workspace_root_pin.is_dir {
        return Err(WorkspaceError::NotDirectory(workspace_path.to_path_buf()));
    }

    let repository = match kind {
        WorkspaceKind::Main => discover_repository(project_root),
        WorkspaceKind::Worktree => linked_worktree_details(workspace_path)?.map(|value| value.0),
        WorkspaceKind::External => discover_repository(workspace_path),
    };
    let mut pins = vec![project_root_pin, workspace_root_pin];
    let mut repository_root = None;
    let mut common_git_dir = None;
    let mut admin_dir = None;
    let mut marker = None;
    let mut commondir = None;
    let mut gitdir = None;
    let mut head = None;

    if let Some(repository) = repository {
        let repository_root_pin =
            PinnedPath::open(repository.root()).map_err(|_| WorkspaceError::PathResolution {
                path: repository.root().to_path_buf(),
                reason: "repository root could not be pinned".into(),
            })?;
        let common_git_dir_pin =
            PinnedPath::open(repository.git_dir()).map_err(|_| WorkspaceError::PathResolution {
                path: repository.git_dir().to_path_buf(),
                reason: "common Git directory could not be pinned".into(),
            })?;
        pins.push(repository_root_pin);
        pins.push(common_git_dir_pin);
        repository_root = Some(pins[pins.len() - 2].to_fact()?);
        common_git_dir = Some(pins[pins.len() - 1].to_fact()?);

        if kind == WorkspaceKind::Worktree {
            let marker_path = workspace_path.join(".git");
            let marker_pin = PinnedPath::open(&marker_path).map_err(|_| {
                WorkspaceError::UnregisteredLinkedWorktree(workspace_path.to_path_buf())
            })?;
            let registered = registered_linked_worktree_dirs(workspace_path, &marker_pin)
                .ok_or_else(|| {
                    WorkspaceError::UnregisteredLinkedWorktree(workspace_path.to_path_buf())
                })?;
            marker = Some(registered.marker.to_fact()?);
            commondir = Some(registered.commondir_file.to_fact()?);
            gitdir = Some(registered.gitdir_file.to_fact()?);
            admin_dir = Some(registered.admin_dir.to_fact()?);
            head = Some(registered.head.to_fact()?);
            for registered_pin in registered.pins() {
                pins.push(PinnedPath::open(&registered_pin.path).map_err(|_| {
                    WorkspaceError::PathResolution {
                        path: workspace_path.to_path_buf(),
                        reason: "linked worktree metadata could not be pinned".into(),
                    }
                })?);
            }
        } else {
            let marker_path = repository.root().join(".git");
            let marker_pin = PinnedPath::open(&marker_path)
                .map_err(|_| WorkspaceError::NotRepository(repository.root().to_path_buf()))?;
            marker = Some(marker_pin.to_fact()?);
            pins.push(marker_pin);
            let head_pin = PinnedPath::open(&repository.git_dir().join("HEAD")).map_err(|_| {
                WorkspaceError::PathResolution {
                    path: repository.git_dir().join("HEAD"),
                    reason: "repository HEAD could not be pinned".into(),
                }
            })?;
            head = Some(head_pin.to_fact()?);
            pins.push(head_pin);
        }
    }

    let binding_kind = match kind {
        WorkspaceKind::Main => WorkspaceBindingKind::Main,
        WorkspaceKind::Worktree => WorkspaceBindingKind::Worktree,
        WorkspaceKind::External => WorkspaceBindingKind::External,
    };
    let fact = WorkspaceBindingFact::issue(
        binding_kind,
        pins[0].to_fact()?,
        pins[1].to_fact()?,
        repository_root,
        common_git_dir,
        admin_dir,
        marker,
        commondir,
        gitdir,
        head,
        branch.map(str::to_string),
    )
    .map_err(|_| WorkspaceError::PathResolution {
        path: workspace_path.to_path_buf(),
        reason: "workspace binding fact is invalid".into(),
    })?;
    if !pins.iter().all(PinnedPath::matches_current_path) {
        return Err(WorkspaceError::PathResolution {
            path: workspace_path.to_path_buf(),
            reason: "workspace identity changed during binding".into(),
        });
    }
    Ok((fact, pins))
}

fn open_fact_pins(fact: &WorkspaceBindingFact) -> Result<Vec<PinnedPath>, WorkspaceError> {
    let facts = [
        Some(fact.project_root()),
        Some(fact.workspace_root()),
        fact.repository_root(),
        fact.common_git_dir(),
        fact.admin_dir(),
        fact.marker(),
        fact.commondir(),
        fact.gitdir(),
        fact.head(),
    ];
    let mut pins = Vec::new();
    for path_fact in facts.into_iter().flatten() {
        let pin =
            PinnedPath::open(path_fact.path()).map_err(|_| WorkspaceError::PathResolution {
                path: path_fact.path().to_path_buf(),
                reason: "durable workspace fact cannot be reopened".into(),
            })?;
        if !pin_matches_fact(&pin, path_fact) {
            return Err(WorkspaceError::PathResolution {
                path: path_fact.path().to_path_buf(),
                reason: "durable workspace fact identity changed".into(),
            });
        }
        pins.push(pin);
    }
    Ok(pins)
}

fn pin_matches_fact(pin: &PinnedPath, fact: &WorkspacePathFact) -> bool {
    pin.path == fact.path()
        && pin.identity == fact.identity()
        && pin.content_length == fact.content_length()
        && pin.content_fingerprint.as_ref() == fact.content_fingerprint()
}

fn pins_match_fact(pins: &[PinnedPath], fact: &WorkspaceBindingFact) -> bool {
    let facts = [
        Some(fact.project_root()),
        Some(fact.workspace_root()),
        fact.repository_root(),
        fact.common_git_dir(),
        fact.admin_dir(),
        fact.marker(),
        fact.commondir(),
        fact.gitdir(),
        fact.head(),
    ];
    facts
        .into_iter()
        .flatten()
        .all(|path_fact| pins.iter().any(|pin| pin_matches_fact(pin, path_fact)))
}

fn canonical_worktree_candidate(path: &Path) -> Result<(PathBuf, bool), WorkspaceError> {
    let candidate =
        normalize_absolute_path(path).map_err(|error| WorkspaceError::PathResolution {
            path: path.to_path_buf(),
            reason: error.to_string(),
        })?;
    match PinnedPath::open(&candidate) {
        Ok(pinned) => {
            if !pinned.is_dir {
                return Err(WorkspaceError::NotDirectory(candidate));
            }
            if !pinned.matches_current_path() {
                return Err(WorkspaceError::PathResolution {
                    path: candidate,
                    reason: "worktree identity changed during resolution".into(),
                });
            }
            Ok((pinned.path, true))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let parent = candidate
                .parent()
                .ok_or_else(|| WorkspaceError::PathDoesNotExist(candidate.clone()))?
                .to_path_buf();
            let parent_pin =
                PinnedPath::open(&parent).map_err(|error| WorkspaceError::PathResolution {
                    path: parent.clone(),
                    reason: error.to_string(),
                })?;
            if !parent_pin.is_dir || !parent_pin.matches_current_path() {
                return Err(WorkspaceError::PathResolution {
                    path: parent,
                    reason: "worktree parent identity changed during resolution".into(),
                });
            }
            Ok((candidate, false))
        }
        Err(error) => Err(WorkspaceError::PathResolution {
            path: candidate,
            reason: error.to_string(),
        }),
    }
}

fn canonical_existing_dir(path: &Path) -> Result<PathBuf, WorkspaceError> {
    let pinned = PinnedPath::open(path).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            WorkspaceError::PathDoesNotExist(path.to_path_buf())
        } else {
            WorkspaceError::PathResolution {
                path: path.to_path_buf(),
                reason: error.to_string(),
            }
        }
    })?;
    if !pinned.is_dir {
        return Err(WorkspaceError::NotDirectory(pinned.path));
    }
    if !pinned.matches_current_path() {
        return Err(WorkspaceError::PathResolution {
            path: pinned.path,
            reason: "directory identity changed during resolution".into(),
        });
    }
    Ok(pinned.path)
}

fn configured_root_identity_matches(path: &Path, expected_identity: &str) -> bool {
    PinnedPath::open(path).is_ok_and(|pinned| {
        pinned.is_dir && pinned.identity == expected_identity && pinned.matches_current_path()
    })
}

impl WorkspaceService {
    /// Return the admitted project root for a host-owned runtime launch.
    /// Revalidates the pinned filesystem identity before exposing the path.
    pub(crate) fn runtime_working_directory(&self) -> Result<PathBuf, WorkspaceError> {
        self.canonical_configured_project_root()
    }

    /// Re-open the configured project root and require the exact filesystem
    /// identity retained at ConfigStore/root admission. A replacement directory
    /// at the same path must fail closed.
    fn canonical_configured_project_root(&self) -> Result<PathBuf, WorkspaceError> {
        let pinned = PinnedPath::open(&self.project_root).map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                WorkspaceError::PathDoesNotExist(self.project_root.clone())
            } else {
                WorkspaceError::PathResolution {
                    path: self.project_root.clone(),
                    reason: error.to_string(),
                }
            }
        })?;
        if !pinned.is_dir {
            return Err(WorkspaceError::NotDirectory(pinned.path));
        }
        if pinned.identity != self.project_root_identity || !pinned.matches_current_path() {
            return Err(WorkspaceError::PathResolution {
                path: pinned.path,
                reason: "configured project root identity changed".into(),
            });
        }
        Ok(pinned.path)
    }
}

const MAX_PINNED_FILE_BYTES: u64 = 64 * 1024;

#[derive(Debug)]
struct PinnedPath {
    path: PathBuf,
    identity: String,
    file: fs::File,
    ancestors: Vec<PinnedAncestor>,
    is_dir: bool,
    hard_link_count: u64,
    content_length: Option<u64>,
    content_fingerprint: Option<RepositoryFingerprint>,
    permissions: u32,
    security_fingerprint: Option<[u8; 32]>,
}

/// A non-forgeable snapshot of one WorkspaceService-retained pin. Its fields
/// are private so WorktreeService can only receive it through the live
/// authorization seam above.
pub(crate) struct WorkspacePinnedPath {
    path: PathBuf,
    file: Arc<fs::File>,
    write_file: Option<Arc<fs::File>>,
    identity: String,
    is_dir: bool,
}

impl WorkspacePinnedPath {
    fn from_pinned(pin: &PinnedPath) -> Option<Self> {
        Some(Self {
            path: pin.path.clone(),
            file: Arc::new(pin.file.try_clone().ok()?),
            write_file: mutation_directory_handle(pin).map(Arc::new),
            identity: pin.identity.clone(),
            is_dir: pin.is_dir,
        })
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn handle(&self) -> Arc<fs::File> {
        Arc::clone(&self.file)
    }

    fn write_handle(&self) -> Option<Arc<fs::File>> {
        self.write_file.as_ref().map(Arc::clone)
    }

    pub(crate) fn identity(&self) -> &str {
        &self.identity
    }

    pub(crate) fn is_dir(&self) -> bool {
        self.is_dir
    }
}

#[derive(Debug)]
struct PinnedAncestor {
    path: PathBuf,
    identity: String,
    file: fs::File,
    hard_link_count: u64,
    permissions: u32,
    security_fingerprint: Option<[u8; 32]>,
}

fn mutation_directory_handle(pin: &PinnedPath) -> Option<fs::File> {
    if !pin.is_dir {
        return None;
    }

    #[cfg(unix)]
    let file = pin.file.try_clone().ok()?;

    #[cfg(windows)]
    let file = {
        use std::fs::OpenOptions;
        use std::os::windows::fs::OpenOptionsExt;
        use windows::Win32::Storage::FileSystem::{
            FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_DELETE,
            FILE_SHARE_READ, FILE_SHARE_WRITE,
        };

        OpenOptions::new()
            .read(true)
            .write(true)
            .share_mode(FILE_SHARE_READ.0 | FILE_SHARE_WRITE.0 | FILE_SHARE_DELETE.0)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS.0 | FILE_FLAG_OPEN_REPARSE_POINT.0)
            .open(&pin.path)
            .ok()?
    };

    #[cfg(not(any(unix, windows)))]
    let file = pin.file.try_clone().ok()?;

    let metadata = file.metadata().ok()?;
    if !metadata.is_dir()
        || is_reparse_point(&metadata)
        || stable_identity_from_file(&file).as_deref() != Some(pin.identity.as_str())
        || stable_identity_from_file(&pin.file).as_deref() != Some(pin.identity.as_str())
    {
        return None;
    }
    Some(file)
}

impl PinnedPath {
    fn open(path: &Path) -> io::Result<Self> {
        let path = normalize_absolute_path(path)?;
        let OpenedPath { file, ancestors } = open_no_follow_chain(&path)?;
        let metadata = file.metadata()?;
        let identity = stable_identity_from_file(&file).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::Unsupported,
                "filesystem does not expose a stable path identity",
            )
        })?;
        let is_dir = metadata.is_dir();
        let permissions = file_permissions(&metadata);
        reject_unsafe_permissions(permissions)?;
        let security_fingerprint = security_fingerprint(&file)?;
        let link_count = hard_link_count(&file, &metadata);
        if !is_dir && link_count > 1 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "workspace files with multiple hard links are not admissible",
            ));
        }
        let (content_length, content_fingerprint) = if is_dir {
            (None, None)
        } else {
            if metadata.len() > MAX_PINNED_FILE_BYTES {
                return Err(io::Error::new(
                    io::ErrorKind::FileTooLarge,
                    "workspace metadata file is too large",
                ));
            }
            let fingerprint = fingerprint_file(&file, metadata.len())?;
            (Some(metadata.len()), Some(fingerprint))
        };
        Ok(Self {
            path,
            identity,
            file,
            ancestors,
            is_dir,
            hard_link_count: link_count,
            content_length,
            content_fingerprint,
            permissions,
            security_fingerprint,
        })
    }

    fn read_to_string(&self) -> io::Result<String> {
        // Read through the already-held handle. Reopening by path here would
        // reintroduce a nested rename/swap race between validation and use.
        let mut file = self.file.try_clone()?;
        file.seek(SeekFrom::Start(0))?;
        if stable_identity_from_file(&file).as_deref() != Some(self.identity.as_str()) {
            return Err(io::Error::new(
                io::ErrorKind::Other,
                "workspace metadata identity changed while reading",
            ));
        }
        let metadata = file.metadata()?;
        if hard_link_count(&file, &metadata) != self.hard_link_count
            || file_permissions(&metadata) != self.permissions
            || security_fingerprint(&file)? != self.security_fingerprint
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "workspace metadata authority changed while reading",
            ));
        }
        let mut bytes = Vec::new();
        file.take(MAX_PINNED_FILE_BYTES + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() as u64 != self.content_length.unwrap_or(0) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "workspace metadata changed while reading",
            ));
        }
        if let Some(expected) = &self.content_fingerprint {
            let actual = fingerprint_bytes(&bytes)?;
            if &actual != expected {
                return Err(io::Error::new(
                    io::ErrorKind::Other,
                    "workspace metadata content changed while reading",
                ));
            }
        }
        String::from_utf8(bytes).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "workspace metadata is not valid UTF-8",
            )
        })
    }

    fn matches_current_path(&self) -> bool {
        Self::open(&self.path)
            .map(|current| {
                current.identity == self.identity
                    && current.is_dir == self.is_dir
                    && current.hard_link_count == self.hard_link_count
                    && current.permissions == self.permissions
                    && current.security_fingerprint == self.security_fingerprint
                    && current.content_length == self.content_length
                    && current.content_fingerprint == self.content_fingerprint
                    && current.ancestors.len() == self.ancestors.len()
                    && current
                        .ancestors
                        .iter()
                        .zip(&self.ancestors)
                        .all(|(current, pinned)| {
                            path_identity_key(&current.path) == path_identity_key(&pinned.path)
                                && current.identity == pinned.identity
                                && current.hard_link_count == pinned.hard_link_count
                                && stable_identity_from_file(&pinned.file).as_deref()
                                    == Some(pinned.identity.as_str())
                                && current.permissions == pinned.permissions
                                && current.security_fingerprint == pinned.security_fingerprint
                        })
            })
            .unwrap_or(false)
    }

    fn to_fact(&self) -> Result<WorkspacePathFact, WorkspaceError> {
        WorkspacePathFact::new(
            self.path.clone(),
            self.identity.clone(),
            self.content_length,
            self.content_fingerprint.clone(),
        )
        .map_err(|_| WorkspaceError::PathResolution {
            path: self.path.clone(),
            reason: "workspace path fact is invalid".into(),
        })
    }
}

/// Validate a host-configured path through the same held-handle, no-follow
/// traversal used by live workspace authorities. The returned path is only a
/// lexical absolute locator for host-private use; its authority is the handle
/// identity captured during this call.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct ValidatedHostWorkspacePath {
    pub(crate) path: PathBuf,
    pub(crate) identity: String,
}

pub(crate) fn validate_host_workspace_path(
    path: &Path,
    require_dir: bool,
) -> io::Result<ValidatedHostWorkspacePath> {
    let pinned = PinnedPath::open(path)?;
    if require_dir && !pinned.is_dir {
        return Err(io::Error::new(
            io::ErrorKind::NotADirectory,
            "workspace path is not a directory",
        ));
    }
    if !pinned.matches_current_path() {
        return Err(io::Error::new(
            io::ErrorKind::Other,
            "workspace path changed during validation",
        ));
    }
    Ok(ValidatedHostWorkspacePath {
        path: pinned.path,
        identity: pinned.identity,
    })
}

#[cfg(windows)]
fn is_reparse_point(metadata: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    use windows::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;

    metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0
}

#[cfg(not(windows))]
fn is_reparse_point(_: &fs::Metadata) -> bool {
    false
}

fn path_contains_link_or_reparse_point(path: &Path) -> bool {
    let Ok(path) = normalize_absolute_path(path) else {
        return true;
    };
    let mut current = PathBuf::new();
    for component in path.components() {
        current.push(component.as_os_str());
        let Ok(metadata) = fs::symlink_metadata(&current) else {
            continue;
        };
        if metadata.file_type().is_symlink() || is_reparse_point(&metadata) {
            return true;
        }
    }
    false
}

fn fingerprint_file(file: &fs::File, length: u64) -> io::Result<RepositoryFingerprint> {
    let mut reader = file.try_clone()?.take(MAX_PINNED_FILE_BYTES + 1);
    let mut bytes = Vec::with_capacity(length as usize);
    reader.read_to_end(&mut bytes)?;
    if bytes.len() as u64 != length {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "workspace metadata length changed while hashing",
        ));
    }
    fingerprint_bytes(&bytes)
}

fn fingerprint_bytes(bytes: &[u8]) -> io::Result<RepositoryFingerprint> {
    let digest = Sha256::digest(bytes);
    RepositoryFingerprint::from_host_token(format!(
        "sha256:{}",
        digest
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    ))
    .map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "workspace metadata digest invalid",
        )
    })
}

fn open_read_handle(path: &Path) -> io::Result<fs::File> {
    #[cfg(windows)]
    {
        use std::fs::OpenOptions;
        use std::os::windows::fs::OpenOptionsExt;
        use windows::Win32::Storage::FileSystem::{
            FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_DELETE,
            FILE_SHARE_READ, FILE_SHARE_WRITE,
        };

        OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ.0 | FILE_SHARE_WRITE.0 | FILE_SHARE_DELETE.0)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS.0 | FILE_FLAG_OPEN_REPARSE_POINT.0)
            .open(path)
    }
    #[cfg(not(windows))]
    {
        use std::fs::OpenOptions;
        use std::os::unix::fs::OpenOptionsExt;
        // O_NOFOLLOW prevents the final component from being redirected by a
        // symlink. Ancestors are checked separately before this open.
        #[cfg(any(target_os = "linux", target_os = "android"))]
        const NOFOLLOW: i32 = 0x20000;
        #[cfg(any(target_os = "macos", target_os = "ios"))]
        const NOFOLLOW: i32 = 0x100;
        #[cfg(any(target_os = "freebsd", target_os = "openbsd", target_os = "netbsd"))]
        const NOFOLLOW: i32 = 0x100;
        OpenOptions::new()
            .read(true)
            .custom_flags(NOFOLLOW)
            .open(path)
    }
}

struct OpenedPath {
    file: fs::File,
    ancestors: Vec<PinnedAncestor>,
}

fn normalize_absolute_path(path: &Path) -> io::Result<PathBuf> {
    let candidate = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    let mut normalized = PathBuf::new();
    for component in candidate.components() {
        match component {
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            Component::RootDir => normalized.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                if !normalized.pop() {
                    return Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        "workspace path escapes its filesystem root",
                    ));
                }
            }
            Component::Normal(name) => normalized.push(name),
        }
    }
    if normalized.as_os_str().is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "workspace path is empty",
        ));
    }

    #[cfg(windows)]
    {
        // Keep the host-private locator in Windows' canonical extended form
        // without asking the filesystem to resolve it.  Handle-relative
        // traversal below remains the authority check; this conversion only
        // makes the lexical representation stable for drive/UNC comparisons
        // and long paths.
        let text = normalized.to_string_lossy();
        let lower = text.to_ascii_lowercase();
        if lower.starts_with("\\\\?\\") {
            return Ok(normalized);
        }
        if text.starts_with("\\\\") {
            return Ok(PathBuf::from(format!("\\\\?\\UNC{}", &text[1..])));
        }
        return Ok(PathBuf::from(format!("\\\\?\\{}", text)));
    }

    #[cfg(not(windows))]
    Ok(normalized)
}

#[cfg(unix)]
fn open_no_follow_chain(path: &Path) -> io::Result<OpenedPath> {
    use std::ffi::CString;
    use std::os::fd::{AsRawFd, FromRawFd, RawFd};
    use std::os::unix::ffi::OsStrExt;

    // Opening every component relative to the already-held directory fd is
    // the race-resistant Unix primitive.  The final O_NOFOLLOW prevents a
    // last-component symlink, while O_DIRECTORY makes each ancestor a real
    // directory rather than a file or link.
    const AT_FDCWD: RawFd = -100;
    const O_RDONLY: i32 = 0;
    const O_CLOEXEC: i32 = 0o2000000;
    #[cfg(any(target_os = "linux", target_os = "android"))]
    const O_DIRECTORY: i32 = 0o200000;
    #[cfg(any(target_os = "macos", target_os = "ios"))]
    const O_DIRECTORY: i32 = 0x00100000;
    #[cfg(any(target_os = "freebsd", target_os = "openbsd", target_os = "netbsd"))]
    const O_DIRECTORY: i32 = 0x00010000;
    #[cfg(any(target_os = "linux", target_os = "android"))]
    const O_NOFOLLOW: i32 = 0o400000;
    #[cfg(any(
        target_os = "macos",
        target_os = "ios",
        target_os = "freebsd",
        target_os = "openbsd",
        target_os = "netbsd"
    ))]
    const O_NOFOLLOW: i32 = 0x00000100;

    extern "C" {
        fn openat(
            dirfd: RawFd,
            pathname: *const std::os::raw::c_char,
            flags: i32,
            mode: u32,
        ) -> RawFd;
    }

    let components = path
        .components()
        .filter_map(|component| match component {
            Component::Normal(name) => Some(name.as_bytes()),
            Component::RootDir | Component::Prefix(_) | Component::CurDir => None,
            Component::ParentDir => None,
        })
        .collect::<Vec<_>>();

    let root = CString::new(Path::new("/").as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "workspace root contains NUL"))?;
    let root_fd = unsafe {
        openat(
            AT_FDCWD,
            root.as_ptr(),
            O_RDONLY | O_DIRECTORY | O_NOFOLLOW | O_CLOEXEC,
            0,
        )
    };
    if root_fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: root_fd was returned by openat and is owned by this File.
    let mut current = unsafe { fs::File::from_raw_fd(root_fd) };
    let mut ancestors = Vec::with_capacity(components.len());
    let mut current_path = PathBuf::from("/");

    // Retain the exact root handle as the first ancestor. This makes the
    // identity chain complete for both a root target and every nested path;
    // callers can therefore detect a root/parent swap before using a pin.
    let root_metadata = current.metadata()?;
    let root_identity = stable_identity_from_file(&current).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::Unsupported,
            "stable path identity unavailable",
        )
    })?;
    let root_permissions = file_permissions(&root_metadata);
    reject_unsafe_permissions(root_permissions)?;
    let root_security_fingerprint = security_fingerprint(&current)?;
    ancestors.push(PinnedAncestor {
        path: current_path.clone(),
        identity: root_identity,
        file: current.try_clone()?,
        hard_link_count: hard_link_count(&current, &root_metadata),
        permissions: root_permissions,
        security_fingerprint: root_security_fingerprint,
    });

    if components.is_empty() {
        return Ok(OpenedPath {
            file: current,
            ancestors,
        });
    }

    for (index, component) in components.iter().enumerate() {
        let name = CString::new(*component).map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidInput, "workspace path contains NUL")
        })?;
        let final_component = index + 1 == components.len();
        let flags =
            O_RDONLY | O_NOFOLLOW | O_CLOEXEC | if final_component { 0 } else { O_DIRECTORY };
        let fd = unsafe { openat(current.as_raw_fd(), name.as_ptr(), flags, 0) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: fd was returned by openat and is owned by this File.
        let opened = unsafe { fs::File::from_raw_fd(fd) };
        current_path.push(std::ffi::OsStr::from_bytes(component));
        let metadata = opened.metadata()?;
        let identity = stable_identity_from_file(&opened).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::Unsupported,
                "stable path identity unavailable",
            )
        })?;
        let permissions = file_permissions(&metadata);
        reject_unsafe_permissions(permissions)?;
        let security_fingerprint = security_fingerprint(&opened)?;
        if final_component {
            return Ok(OpenedPath {
                file: opened,
                ancestors,
            });
        }
        ancestors.push(PinnedAncestor {
            path: current_path.clone(),
            identity,
            file: opened.try_clone()?,
            hard_link_count: hard_link_count(&opened, &metadata),
            permissions,
            security_fingerprint,
        });
        current = opened;
    }
    unreachable!("non-empty path must return its final component")
}

#[cfg(windows)]
fn open_no_follow_chain(path: &Path) -> io::Result<OpenedPath> {
    // Windows has no CreateFile analogue of openat. Open and retain every
    // ancestor with FILE_FLAG_OPEN_REPARSE_POINT, then revalidate each exact
    // handle identity after opening the final component. A junction/symlink
    // therefore cannot be silently followed, and a nested rename is detected
    // before the pin is admitted.
    let mut ancestors = Vec::new();
    let mut chain = path.ancestors().collect::<Vec<_>>();
    chain.reverse();
    for (index, ancestor) in chain.iter().enumerate() {
        if ancestor.as_os_str().is_empty() {
            continue;
        }
        let file = open_read_handle(ancestor)?;
        let metadata = file.metadata()?;
        if is_reparse_point(&metadata) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "workspace path contains a symbolic link or reparse point",
            ));
        }
        let permissions = file_permissions(&metadata);
        reject_unsafe_permissions(permissions)?;
        let security_fingerprint = security_fingerprint(&file)?;
        let identity = stable_identity_from_file(&file).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::Unsupported,
                "stable path identity unavailable",
            )
        })?;
        let link_count = hard_link_count(&file, &metadata);
        let is_final = index + 1 == chain.len();
        if is_final {
            return Ok(OpenedPath { file, ancestors });
        }
        ancestors.push(PinnedAncestor {
            path: ancestor.to_path_buf(),
            identity,
            file,
            hard_link_count: link_count,
            permissions,
            security_fingerprint,
        });
    }
    Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        "workspace path has no final component",
    ))
}

#[cfg(not(any(unix, windows)))]
fn open_no_follow_chain(path: &Path) -> io::Result<OpenedPath> {
    let file = open_read_handle(path)?;
    let metadata = file.metadata()?;
    let permissions = file_permissions(&metadata);
    reject_unsafe_permissions(permissions)?;
    let _security_fingerprint = security_fingerprint(&file)?;
    Ok(OpenedPath {
        file,
        ancestors: Vec::new(),
    })
}

#[cfg(unix)]
fn file_permissions(metadata: &fs::Metadata) -> u32 {
    use std::os::unix::fs::MetadataExt;
    metadata.mode()
}

#[cfg(not(unix))]
fn file_permissions(metadata: &fs::Metadata) -> u32 {
    u32::from(metadata.permissions().readonly())
}

fn reject_unsafe_permissions(permissions: u32) -> io::Result<()> {
    // Admission policy is platform-specific and deliberately explicit. Unix
    // rejects group/world writes (the mode bits are the complete local ACL
    // surface used here). Windows readonly metadata is not an ACL policy: the
    // owner, group, and DACL are inspected and hashed by `security_fingerprint`
    // and any inspection failure or later descriptor change fails closed.
    #[cfg(unix)]
    if permissions & 0o022 != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "workspace path has group/world writable permissions",
        ));
    }
    let _ = permissions;
    Ok(())
}

#[cfg(windows)]
fn security_fingerprint(file: &fs::File) -> io::Result<Option<[u8; 32]>> {
    use std::os::windows::io::AsRawHandle;
    use windows::Win32::Foundation::{LocalFree, HANDLE, HLOCAL};
    use windows::Win32::Security::Authorization::{GetSecurityInfo, SE_FILE_OBJECT};
    use windows::Win32::Security::{
        GetSecurityDescriptorLength, DACL_SECURITY_INFORMATION, GROUP_SECURITY_INFORMATION,
        OWNER_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR,
    };

    let mut descriptor = PSECURITY_DESCRIPTOR::default();
    let status = unsafe {
        GetSecurityInfo(
            HANDLE(file.as_raw_handle()),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION | GROUP_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            None,
            None,
            None,
            None,
            Some(&mut descriptor),
        )
    };
    if status.0 != 0 || descriptor.is_invalid() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "workspace ACL could not be inspected",
        ));
    }
    let length = unsafe { GetSecurityDescriptorLength(descriptor) } as usize;
    if length == 0 {
        unsafe {
            LocalFree(Some(HLOCAL(descriptor.0)));
        }
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "workspace ACL descriptor is empty",
        ));
    }
    // The descriptor is allocated by GetSecurityInfo and remains valid until
    // LocalFree. Copy only the digest; never retain or expose ACL bytes.
    let digest = unsafe {
        let bytes = std::slice::from_raw_parts(descriptor.0 as *const u8, length);
        let digest = Sha256::digest(bytes);
        LocalFree(Some(HLOCAL(descriptor.0)));
        digest.into()
    };
    Ok(Some(digest))
}

#[cfg(not(windows))]
fn security_fingerprint(_: &fs::File) -> io::Result<Option<[u8; 32]>> {
    Ok(None)
}

#[cfg(unix)]
fn hard_link_count(file: &fs::File, metadata: &fs::Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;
    let _ = file;
    metadata.nlink()
}

#[cfg(windows)]
fn hard_link_count(file: &fs::File, _: &fs::Metadata) -> u64 {
    use std::os::windows::io::AsRawHandle;
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::Storage::FileSystem::{
        GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION,
    };
    let mut information = BY_HANDLE_FILE_INFORMATION::default();
    if unsafe { GetFileInformationByHandle(HANDLE(file.as_raw_handle()), &mut information) }.is_ok()
    {
        u64::from(information.nNumberOfLinks)
    } else {
        u64::MAX
    }
}

#[cfg(not(any(unix, windows)))]
fn hard_link_count(_: &fs::File, _: &fs::Metadata) -> u64 {
    1
}

fn stable_identity_from_file(file: &fs::File) -> Option<String> {
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
        Some(format!(
            "windows:{:08x}:{:016x}",
            information.dwVolumeSerialNumber, file_index
        ))
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let metadata = file.metadata().ok()?;
        Some(format!("unix:{}:{}", metadata.dev(), metadata.ino()))
    }
    #[cfg(not(any(windows, unix)))]
    {
        let _ = file;
        None
    }
}

fn stable_path_identity_key(path: &Path) -> Option<String> {
    let pinned = PinnedPath::open(path).ok()?;
    if !pinned.matches_current_path() {
        return None;
    }
    Some(pinned.identity)
}

fn workspace_identity_token(path: &Path) -> Option<[u8; 32]> {
    let pinned = PinnedPath::open(path).ok()?;
    if !pinned.matches_current_path() {
        return None;
    }
    let digest = Sha256::digest(pinned.identity.as_bytes());
    Some(digest.into())
}

fn same_path_identity(left: &Path, right: &Path) -> bool {
    let left = PinnedPath::open(left).ok();
    let right = PinnedPath::open(right).ok();
    match (left, right) {
        (Some(left), Some(right)) => {
            left.identity == right.identity
                && left.matches_current_path()
                && right.matches_current_path()
        }
        _ => false,
    }
}

fn discover_repository(workspace: &Path) -> Option<RepositoryIdentity> {
    let mut current = normalize_absolute_path(workspace).ok()?;
    loop {
        let current_pin = PinnedPath::open(&current).ok()?;
        if !current_pin.is_dir || !current_pin.matches_current_path() {
            return None;
        }
        let marker = current.join(".git");
        let marker_metadata = fs::symlink_metadata(&marker).ok();
        let Some(_) = marker_metadata else {
            if !current.pop() {
                return None;
            }
            continue;
        };
        let marker_pin = PinnedPath::open(&marker).ok()?;
        if marker_pin.is_dir {
            return repository_identity(
                current.clone(),
                marker_pin.path.clone(),
                &[&current_pin, &marker_pin],
            );
        }
        if let Some(repository) = linked_worktree_repository(&current, &marker_pin) {
            return Some(repository);
        }
        if !current.pop() {
            return None;
        }
    }
}

struct RegisteredLinkedWorktree {
    worktree_root: PinnedPath,
    marker: PinnedPath,
    git_dir: PinnedPath,
    common_git_dir: PinnedPath,
    commondir_file: PinnedPath,
    gitdir_file: PinnedPath,
    admin_dir: PinnedPath,
    head: PinnedPath,
    additional_pins: Vec<PinnedPath>,
}

impl RegisteredLinkedWorktree {
    fn pins(&self) -> Vec<&PinnedPath> {
        let mut pins = vec![
            &self.worktree_root,
            &self.marker,
            &self.git_dir,
            &self.common_git_dir,
            &self.commondir_file,
            &self.gitdir_file,
            &self.admin_dir,
            &self.head,
        ];
        pins.extend(self.additional_pins.iter());
        pins
    }

    fn matches_current_paths(&self) -> bool {
        self.pins()
            .into_iter()
            .all(PinnedPath::matches_current_path)
    }
}

fn linked_worktree_repository(
    worktree_root: &Path,
    marker: &PinnedPath,
) -> Option<RepositoryIdentity> {
    let registered = registered_linked_worktree_dirs(worktree_root, marker)?;
    let root = registered.common_git_dir.path.parent()?.to_path_buf();
    let root = normalize_absolute_path(&root).ok()?;
    let pins = registered.pins();
    let repository = repository_identity(root, registered.common_git_dir.path.clone(), &pins)?;
    registered.matches_current_paths().then_some(repository)
}

fn linked_worktree_details(
    worktree_root: &Path,
) -> Result<Option<(RepositoryIdentity, String)>, WorkspaceError> {
    let marker = worktree_root.join(".git");
    let Ok(marker_pin) = PinnedPath::open(&marker) else {
        return Ok(None);
    };
    if marker_pin.is_dir {
        return Ok(None);
    }
    let registered = registered_linked_worktree_dirs(worktree_root, &marker_pin)
        .ok_or_else(|| WorkspaceError::UnregisteredLinkedWorktree(worktree_root.to_path_buf()))?;
    let pins = registered.pins();
    let repository = repository_identity(
        normalize_absolute_path(registered.common_git_dir.path.parent().ok_or_else(|| {
            WorkspaceError::LinkedWorktreeBranchUnavailable(worktree_root.to_path_buf())
        })?)
        .map_err(|_| {
            WorkspaceError::LinkedWorktreeBranchUnavailable(worktree_root.to_path_buf())
        })?,
        registered.common_git_dir.path.clone(),
        &pins,
    )
    .ok_or_else(|| WorkspaceError::UnregisteredLinkedWorktree(worktree_root.to_path_buf()))?;
    let head = registered
        .head
        .read_to_string()
        .and_then(|contents| {
            if !registered.head.matches_current_path() {
                return Err(io::Error::new(
                    io::ErrorKind::Other,
                    "linked worktree HEAD identity changed",
                ));
            }
            Ok(contents)
        })
        .map_err(|_| {
            WorkspaceError::LinkedWorktreeBranchUnavailable(worktree_root.to_path_buf())
        })?;
    let branch = unique_head_branch(&head).ok_or_else(|| {
        WorkspaceError::LinkedWorktreeBranchUnavailable(worktree_root.to_path_buf())
    })?;
    if !registered.matches_current_paths() {
        return Err(WorkspaceError::UnregisteredLinkedWorktree(
            worktree_root.to_path_buf(),
        ));
    }
    Ok(Some((repository, branch.to_string())))
}

/// Read Git's linked-worktree registration and verify both sides of the
/// relationship before any branch or repository identity is trusted.
fn registered_linked_worktree_dirs(
    worktree_root: &Path,
    marker: &PinnedPath,
) -> Option<RegisteredLinkedWorktree> {
    let worktree_root = PinnedPath::open(worktree_root).ok()?;
    if !worktree_root.is_dir || marker.is_dir {
        return None;
    }
    let gitdir = unique_metadata_line(&marker.read_to_string().ok()?, "gitdir:")?;
    let git_dir = PathBuf::from(gitdir);
    let git_dir = if git_dir.is_absolute() {
        git_dir
    } else {
        worktree_root.path.join(git_dir)
    };
    let git_dir = normalize_absolute_path(&git_dir).ok()?;
    let git_dir = PinnedPath::open(&git_dir).ok()?;
    if !git_dir.is_dir {
        return None;
    }

    let common_dir = PinnedPath::open(&git_dir.path.join("commondir")).ok()?;
    let common = PathBuf::from(unique_metadata_line(
        &common_dir.read_to_string().ok()?,
        "",
    )?);
    let common = if common.is_absolute() {
        common
    } else {
        git_dir.path.join(common)
    };
    let common_git_dir = normalize_absolute_path(&common).ok()?;
    let common_git_dir = PinnedPath::open(&common_git_dir).ok()?;
    if !common_git_dir.is_dir {
        return None;
    }

    let admin_root = normalize_absolute_path(&common_git_dir.path.join("worktrees")).ok()?;
    let admin_root = PinnedPath::open(&admin_root).ok()?;
    let admin_parent = git_dir.path.parent()?;
    let admin_parent = PinnedPath::open(admin_parent).ok()?;
    if admin_parent.identity != admin_root.identity {
        return None;
    }

    let back_reference_file = PinnedPath::open(&git_dir.path.join("gitdir")).ok()?;
    let back_reference = PathBuf::from(unique_metadata_line(
        &back_reference_file.read_to_string().ok()?,
        "",
    )?);
    let back_reference = if back_reference.is_absolute() {
        back_reference
    } else {
        git_dir.path.join(back_reference)
    };
    let back_reference = normalize_absolute_path(&back_reference).ok()?;
    let back_reference = PinnedPath::open(&back_reference).ok()?;
    let canonical_marker = normalize_absolute_path(&marker.path).ok()?;
    let canonical_marker = PinnedPath::open(&canonical_marker).ok()?;
    if back_reference.identity != canonical_marker.identity
        || back_reference.identity != marker.identity
    {
        return None;
    }

    let head = PinnedPath::open(&git_dir.path.join("HEAD")).ok()?;
    let registered = RegisteredLinkedWorktree {
        worktree_root,
        marker: PinnedPath::open(&marker.path).ok()?,
        git_dir,
        common_git_dir,
        commondir_file: common_dir,
        gitdir_file: back_reference_file,
        admin_dir: admin_root,
        head,
        additional_pins: vec![admin_parent, back_reference, canonical_marker],
    };
    registered.matches_current_paths().then_some(registered)
}

fn unique_metadata_line(contents: &str, prefix: &str) -> Option<String> {
    let mut values = contents
        .lines()
        .filter_map(|line| line.strip_prefix(prefix).map(str::trim))
        .filter(|value| !value.is_empty());
    let value = values.next()?.to_string();
    values.next().is_none().then_some(value)
}

fn unique_head_branch(contents: &str) -> Option<String> {
    let mut branches = contents
        .lines()
        .filter_map(|line| line.strip_prefix("ref: refs/heads/").map(str::trim))
        .filter(|branch| !branch.is_empty());
    let branch = branches.next()?.to_string();
    branches.next().is_none().then_some(branch)
}

fn repository_identity(
    root: PathBuf,
    git_dir: PathBuf,
    pins: &[&PinnedPath],
) -> Option<RepositoryIdentity> {
    let root_pin = PinnedPath::open(&root).ok()?;
    let git_pin = PinnedPath::open(&git_dir).ok()?;
    if !root_pin.is_dir
        || !git_pin.is_dir
        || !root_pin.matches_current_path()
        || !git_pin.matches_current_path()
        || !pins.iter().all(|pin| pin.matches_current_path())
    {
        return None;
    }
    let key = git_pin.identity.clone();
    let mut all_pins = vec![&root_pin, &git_pin];
    all_pins.extend(pins.iter().copied());
    let fingerprint = repository_fingerprint_for_pins(&all_pins)?;
    Some(RepositoryIdentity::new(root, git_dir, key, fingerprint))
}

fn repository_fingerprint(git_dir: &Path) -> Option<RepositoryFingerprint> {
    let pinned = PinnedPath::open(git_dir).ok()?;
    if !pinned.is_dir || !pinned.matches_current_path() {
        return None;
    }
    repository_fingerprint_for_pins(&[&pinned])
}

fn repository_fingerprint_for_pins(pins: &[&PinnedPath]) -> Option<RepositoryFingerprint> {
    let mut hasher = Sha256::new();
    hasher.update(b"devmanager-repository-fingerprint-v3\0");
    for pin in pins {
        // A Windows path's display spelling is not its identity.  Fold the
        // host-private locator using the same final-path comparison key so a
        // drive/UNC case variation cannot make one repository immutable.
        hasher.update(path_identity_key(&pin.path).as_bytes());
        hasher.update([0]);
        hasher.update(pin.identity.as_bytes());
        hasher.update([0]);
        hasher.update(pin.content_length.unwrap_or(0).to_le_bytes());
        hasher.update([0]);
        if let Some(fingerprint) = &pin.content_fingerprint {
            hasher.update(fingerprint.as_str().as_bytes());
        }
        hasher.update([0xff]);
    }

    let digest = hasher.finalize();
    let token = digest
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    RepositoryFingerprint::from_host_token(format!("sha256:{token}")).ok()
}

#[cfg(test)]
mod cockpit_authority_tests {
    use super::{issue_file_service, issue_read_file_service, WorkspaceService};
    use crate::domain::task::WorkspaceRef;
    use crate::domain::{ClientId, CommandId, ProjectId, RequestId, TaskId};
    use crate::git::command::{issue_git_host_binding, GitCancellation, GitRepository};
    use crate::workspace::files::ReadOptions;
    use crate::workspace::model::{WorkspaceProjectRoots, WorkspaceRequest, WorkspaceResource};
    use std::fs;
    use std::process::Command as ProcessCommand;
    use uuid::Uuid;

    fn bound_service() -> (tempfile::TempDir, WorkspaceService, TaskId) {
        let root = tempfile::tempdir().expect("workspace root");
        let output = ProcessCommand::new("git")
            .args(["init", "--quiet"])
            .current_dir(root.path())
            .output()
            .expect("git init");
        assert!(output.status.success());
        fs::write(root.path().join("README.md"), "ok\n").expect("readme");
        fs::write(root.path().join(".env"), "SECRET=1\n").expect("env");
        fs::write(root.path().join("blob.bin"), [0u8, 1, 2, 255]).expect("binary");
        let project_id = ProjectId::new();
        let task_id = TaskId::new();
        let roots =
            WorkspaceProjectRoots::try_from_pairs([(project_id, root.path().to_path_buf())])
                .expect("roots");
        let mut service = WorkspaceService::with_task_coordinator(
            project_id,
            task_id,
            &roots,
            super::WorkspaceResourceCoordinator::new(),
        )
        .expect("service");
        let client_id = ClientId::new();
        let request_id = RequestId::new();
        let command_id = CommandId::new();
        service
            .bind_authorized_with_generation(
                WorkspaceRequest::main(),
                task_id,
                client_id,
                Uuid::now_v7(),
                request_id,
                command_id,
                1,
                1,
            )
            .expect("bind");
        (root, service, task_id)
    }

    #[test]
    fn persisted_main_workspace_reopens_from_host_project_roots() {
        let (root, service, task_id) = bound_service();
        let durable = service.current().expect("binding").durable_ref().clone();
        let encoded = serde_json::to_vec(&durable).expect("encode durable workspace");
        let restored: WorkspaceRef =
            serde_json::from_slice(&encoded).expect("decode durable workspace");
        assert!(
            restored
                .host_binding()
                .expect("host binding")
                .project_root()
                .path()
                .as_os_str()
                .is_empty(),
            "durable workspace facts must omit live paths"
        );
        let project_id = service.project_id;
        let roots =
            WorkspaceProjectRoots::try_from_pairs([(project_id, root.path().to_path_buf())])
                .expect("roots");
        let loaded = WorkspaceService::from_durable_with_task_coordinator(
            project_id,
            task_id,
            &roots,
            &restored,
            super::WorkspaceResourceCoordinator::new(),
        )
        .expect("reopen persisted main workspace");
        loaded
            .runtime_working_directory()
            .expect("runtime working directory after persist");
        let client_id = ClientId::new();
        let connection_id = Uuid::now_v7();
        let request_id = RequestId::new();
        let command_id = CommandId::from_bytes(*request_id.as_bytes()).expect("command");
        loaded
            .authorize_current_with_generation(
                &restored,
                task_id,
                client_id,
                connection_id,
                request_id,
                command_id,
                1,
                1,
            )
            .expect("authorize rehydrated durable workspace");
    }

    #[test]
    fn admitted_git_status_and_file_list_read_use_live_leases() {
        let (_root, service, task_id) = bound_service();
        let binding = service.current().expect("binding").durable_ref().clone();
        let client_id = ClientId::new();
        let connection_id = Uuid::now_v7();
        let request_id = RequestId::new();
        let command_id = CommandId::from_bytes(*request_id.as_bytes()).expect("command");
        let authorization = service
            .authorize_current_with_generation(
                &binding,
                task_id,
                client_id,
                connection_id,
                request_id,
                command_id,
                1,
                1,
            )
            .expect("authorize");
        let git_lease = service
            .acquire_task_resource(
                task_id,
                WorkspaceResource::Git,
                client_id,
                connection_id,
                request_id,
                command_id,
                1,
                1,
            )
            .expect("git lease");
        let git_binding = issue_git_host_binding(
            &authorization,
            git_lease,
            task_id,
            service.project_id,
            client_id,
            connection_id,
            request_id,
            command_id,
            &binding,
            1,
            1,
        )
        .expect("git binding");
        let repository =
            GitRepository::from_host_binding(git_binding, GitCancellation::new()).expect("repo");
        let status = repository.status().expect("status");
        assert!(!status.is_detached);

        let file_lease = service
            .acquire_task_resource(
                task_id,
                WorkspaceResource::File,
                client_id,
                connection_id,
                request_id,
                command_id,
                1,
                1,
            )
            .expect("file lease");
        let files = issue_file_service(
            &authorization,
            file_lease,
            task_id,
            service.project_id,
            client_id,
            connection_id,
            request_id,
            command_id,
            &binding,
            1,
            1,
        )
        .expect("files");
        assert!(service
            .live_resources_for_task_for_test()
            .contains(&WorkspaceResource::File));
        let listed = files.list(None, 16).expect("list");
        assert!(listed
            .iter()
            .any(|entry| entry.path.as_str() == "README.md"));
        let read = files
            .read("README.md", ReadOptions::default())
            .expect("read");
        assert!(read.total_bytes > 0);
        assert!(matches!(
            files.read(".env", ReadOptions::default()),
            Err(crate::workspace::files::FileServiceError::SecretLikePath)
        ));
        let binary = files
            .read("blob.bin", ReadOptions::default())
            .expect("binary");
        assert_eq!(
            binary.content_kind,
            crate::workspace::files::ContentKind::Binary
        );
        assert!(files.read("../secret", ReadOptions::default()).is_err());
        drop(files);
        assert!(!service
            .live_resources_for_task_for_test()
            .contains(&WorkspaceResource::File));
    }

    #[test]
    fn read_only_file_service_stays_available_when_global_cleanup_scan_is_bounded() {
        let (root, service, task_id) = bound_service();
        // The mutation-capable service deliberately bounds its recursive
        // recovery scan at 1,024 directories. Read-only browsing must not be
        // disabled merely because a legitimate workspace is larger.
        for index in 0..1_025 {
            fs::create_dir(root.path().join(format!("large-{index:04}")))
                .expect("large workspace directory");
        }

        let binding = service.current().expect("binding").durable_ref().clone();
        let client_id = ClientId::new();
        let connection_id = Uuid::now_v7();
        let request_id = RequestId::new();
        let command_id = CommandId::from_bytes(*request_id.as_bytes()).expect("command");
        let authorization = service
            .authorize_current_with_generation(
                &binding,
                task_id,
                client_id,
                connection_id,
                request_id,
                command_id,
                1,
                1,
            )
            .expect("authorize");
        let lease = service
            .acquire_task_resource(
                task_id,
                WorkspaceResource::File,
                client_id,
                connection_id,
                request_id,
                command_id,
                1,
                1,
            )
            .expect("file lease");

        let files = issue_read_file_service(
            &authorization,
            lease,
            task_id,
            service.project_id,
            client_id,
            connection_id,
            request_id,
            command_id,
            &binding,
            1,
            1,
        )
        .expect("read-only file service");
        let read = files
            .read("README.md", ReadOptions::default())
            .expect("read large workspace");
        assert_eq!(read.total_bytes, 3);
    }

    #[test]
    fn file_list_root_revalidation_is_bounded_independently_of_entry_count() {
        let (_root, service, task_id) = bound_service();
        let binding = service.current().expect("binding").durable_ref().clone();
        let client_id = ClientId::new();
        let connection_id = Uuid::now_v7();
        let request_id = RequestId::new();
        let command_id = CommandId::from_bytes(*request_id.as_bytes()).expect("command");
        let authorization = service
            .authorize_current_with_generation(
                &binding,
                task_id,
                client_id,
                connection_id,
                request_id,
                command_id,
                1,
                1,
            )
            .expect("authorize");
        let lease = service
            .acquire_task_resource(
                task_id,
                WorkspaceResource::File,
                client_id,
                connection_id,
                request_id,
                command_id,
                1,
                1,
            )
            .expect("file lease");
        let files = issue_read_file_service(
            &authorization,
            lease,
            task_id,
            service.project_id,
            client_id,
            connection_id,
            request_id,
            command_id,
            &binding,
            1,
            1,
        )
        .expect("read-only file service");

        files.reset_root_revalidations_for_test();
        let entries = files.list(None, 16).expect("list workspace root");
        assert!(entries.len() >= 4);
        assert_eq!(
            files.root_revalidation_count_for_test(),
            2,
            "list must validate the named root before and after the handle-relative snapshot, not once per entry"
        );
    }

    #[test]
    fn stale_foreign_and_mismatched_workspace_authority_fail_closed() {
        let (_root, service, task_id) = bound_service();
        let binding = service.current().expect("binding").durable_ref().clone();
        let client_id = ClientId::new();
        let connection_id = Uuid::now_v7();
        let request_id = RequestId::new();
        let command_id = CommandId::from_bytes(*request_id.as_bytes()).expect("command");
        assert!(service
            .authorize_current_with_generation(
                &binding,
                TaskId::new(),
                client_id,
                connection_id,
                request_id,
                command_id,
                1,
                1,
            )
            .is_err());
        let authorization = service
            .authorize_current_with_generation(
                &binding,
                task_id,
                client_id,
                connection_id,
                request_id,
                command_id,
                1,
                1,
            )
            .expect("auth");
        let lease = service
            .acquire_task_resource(
                task_id,
                WorkspaceResource::Git,
                client_id,
                connection_id,
                request_id,
                command_id,
                1,
                1,
            )
            .expect("lease");
        assert!(issue_git_host_binding(
            &authorization,
            lease,
            task_id,
            service.project_id,
            client_id,
            connection_id,
            request_id,
            command_id,
            &binding,
            9,
            1,
        )
        .is_err());
        let lease = service
            .acquire_task_resource(
                task_id,
                WorkspaceResource::Git,
                client_id,
                connection_id,
                request_id,
                command_id,
                1,
                1,
            )
            .expect("lease");
        assert!(issue_git_host_binding(
            &authorization,
            lease,
            task_id,
            service.project_id,
            ClientId::new(),
            connection_id,
            request_id,
            command_id,
            &binding,
            1,
            1,
        )
        .is_err());
        let file_lease = service
            .acquire_task_resource(
                task_id,
                WorkspaceResource::File,
                client_id,
                connection_id,
                request_id,
                command_id,
                1,
                1,
            )
            .expect("file lease");
        assert!(issue_git_host_binding(
            &authorization,
            file_lease,
            task_id,
            service.project_id,
            client_id,
            connection_id,
            request_id,
            command_id,
            &binding,
            1,
            1,
        )
        .is_err());
    }
}
