use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::domain::task::{RepositoryFingerprint, WorkspaceChoice, WorkspaceRef};
use crate::domain::ProjectId;

/// Host-owned mapping from the authenticated ProjectId to its configured root.
///
/// The client may select a project id, but never supplies the root used to
/// resolve its workspace request. Each admitted root retains the filesystem
/// identity captured during ConfigStore/root validation so a later directory
/// replacement at the same path fails closed.
#[derive(Clone, PartialEq, Eq)]
pub struct WorkspaceProjectRoots {
    roots: BTreeMap<ProjectId, ConfiguredProjectRoot>,
    config_ids: BTreeMap<String, ProjectId>,
}

/// One host-configured project root together with the stable filesystem
/// identity captured when that root was admitted. The path is only a locator;
/// authorization compares the retained identity, never a caller-supplied root.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct ConfiguredProjectRoot {
    path: PathBuf,
    identity: String,
}

impl ConfiguredProjectRoot {
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn identity(&self) -> &str {
        &self.identity
    }
}

impl fmt::Debug for WorkspaceProjectRoots {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("WorkspaceProjectRoots(REDACTED)")
    }
}

#[derive(Clone, PartialEq, Eq)]
pub enum WorkspaceProjectRootsError {
    LegacyProjectIdRequiresAdapter(String),
    InvalidConfigProjectId,
    ConfigProjectIdCollision,
    EmptyProjectRoot(ProjectId),
    MalformedProjectRoot(PathBuf),
    DuplicateProjectId(ProjectId),
    AmbiguousProjectRoot {
        root: PathBuf,
        first: ProjectId,
        second: ProjectId,
    },
    TooManyProjects,
}

impl fmt::Debug for WorkspaceProjectRootsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("WorkspaceProjectRootsError(REDACTED)")
    }
}

impl fmt::Display for WorkspaceProjectRootsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::LegacyProjectIdRequiresAdapter(value) => write!(
                f,
                "legacy config project id requires a host adapter ({})",
                bounded_code(value)
            ),
            Self::InvalidConfigProjectId => f.write_str("invalid host project id"),
            Self::ConfigProjectIdCollision => f.write_str("host project id mapping collision"),
            Self::EmptyProjectRoot(project_id) => {
                let _ = project_id;
                f.write_str("host project has an empty root")
            }
            Self::MalformedProjectRoot(path) => {
                let _ = path;
                f.write_str("host project root is malformed")
            }
            Self::DuplicateProjectId(project_id) => {
                let _ = project_id;
                f.write_str("duplicate host project id")
            }
            Self::AmbiguousProjectRoot { first, second, .. } => {
                write!(
                    f,
                    "host project roots are ambiguous ({} / {})",
                    first, second
                )
            }
            Self::TooManyProjects => f.write_str("host project collection exceeds its bound"),
        }
    }
}

impl std::error::Error for WorkspaceProjectRootsError {}

impl WorkspaceProjectRoots {
    pub(crate) fn empty() -> Self {
        Self {
            roots: BTreeMap::new(),
            config_ids: BTreeMap::new(),
        }
    }

    pub(crate) fn try_from_pairs<I>(pairs: I) -> Result<Self, WorkspaceProjectRootsError>
    where
        I: IntoIterator<Item = (ProjectId, PathBuf)>,
    {
        const MAX_WORKSPACE_PROJECTS: usize = 256;
        let mut roots = BTreeMap::new();
        let mut project_ids = BTreeSet::new();
        let mut identities = BTreeMap::<String, (ProjectId, PathBuf)>::new();
        let mut iterator = pairs.into_iter();
        let mut pairs: Vec<(ProjectId, PathBuf)> = Vec::new();
        if iterator.size_hint().0 > MAX_WORKSPACE_PROJECTS {
            return Err(WorkspaceProjectRootsError::TooManyProjects);
        }
        while let Some(pair) = iterator.next() {
            if pairs.len() >= MAX_WORKSPACE_PROJECTS {
                return Err(WorkspaceProjectRootsError::TooManyProjects);
            }
            pairs.push(pair);
        }
        for (project_id, _) in &pairs {
            if !project_ids.insert(*project_id) {
                return Err(WorkspaceProjectRootsError::DuplicateProjectId(*project_id));
            }
        }
        for (project_id, root) in pairs {
            if root.as_os_str().is_empty() {
                return Err(WorkspaceProjectRootsError::EmptyProjectRoot(project_id));
            }
            if root.to_string_lossy().contains('\0') {
                return Err(WorkspaceProjectRootsError::MalformedProjectRoot(root));
            }

            let validated = crate::workspace::service::validate_host_workspace_path(&root, true)
                .map_err(|_| WorkspaceProjectRootsError::MalformedProjectRoot(root.clone()))?;
            let configured = ConfiguredProjectRoot {
                path: validated.path,
                identity: validated.identity,
            };

            if let Some((first, first_root)) = identities.get(&configured.identity) {
                return Err(WorkspaceProjectRootsError::AmbiguousProjectRoot {
                    root: first_root.clone(),
                    first: *first,
                    second: project_id,
                });
            }
            identities.insert(
                configured.identity.clone(),
                (project_id, configured.path.clone()),
            );
            roots.insert(project_id, configured);
        }
        Ok(Self {
            roots,
            config_ids: BTreeMap::new(),
        })
    }

    /// Adapt only a sealed ConfigStore issuer into the host-owned root map.
    /// The raw `(ProjectId, PathBuf)` shape stays behind this crate-private
    /// boundary; production callers cannot choose either side of the pair.
    pub(crate) fn from_config_issuer(
        issuer: &crate::config::ConfigWorkspaceIssuer,
    ) -> Result<Self, WorkspaceProjectRootsError> {
        let mut roots = Self::try_from_pairs(issuer.workspace_project_roots())?;
        roots.config_ids = issuer.workspace_project_config_ids().into_iter().collect();
        Ok(roots)
    }

    /// Build a workspace-root authority from the host's validated canonical
    /// configuration store. The configured ids and roots are never accepted
    /// as caller-supplied pairs; the store issues the opaque project mapping
    /// and the same revision/action/runtime fences used by host admission.
    pub fn from_host_config_store(
        store: &mut crate::config::ConfigStore,
        expected_revision: crate::config::ConfigRevision,
        action_epoch: u64,
        runtime_generation: u64,
    ) -> Result<Self, crate::config::ConfigError> {
        let issuer =
            store.issue_workspace_authority(expected_revision, action_epoch, runtime_generation)?;
        Self::from_config_issuer(&issuer).map_err(|_| {
            crate::config::ConfigError::new(
                crate::config::ConfigErrorKind::Validation,
                "configured workspace roots are unavailable",
            )
        })
    }

    #[cfg(test)]
    pub(crate) fn try_from_config<I>(projects: I) -> Result<Self, WorkspaceProjectRootsError>
    where
        I: IntoIterator<Item = (String, String)>,
    {
        const MAX_WORKSPACE_PROJECTS: usize = 256;
        let mut pairs = Vec::new();
        let mut config_ids = BTreeMap::new();
        let mut derived_ids = BTreeMap::new();
        let projects = projects.into_iter();
        if projects.size_hint().0 > MAX_WORKSPACE_PROJECTS {
            return Err(WorkspaceProjectRootsError::TooManyProjects);
        }
        for (raw_id, raw_root) in projects {
            if pairs.len() >= MAX_WORKSPACE_PROJECTS {
                return Err(WorkspaceProjectRootsError::TooManyProjects);
            }
            let config_id = validate_config_project_id(&raw_id)?;
            let project_id = ProjectId::parse(config_id).map_err(|_| {
                WorkspaceProjectRootsError::LegacyProjectIdRequiresAdapter(config_id.to_string())
            })?;
            if let Some(previous) = derived_ids.insert(project_id, config_id.to_string()) {
                if previous != config_id {
                    return Err(WorkspaceProjectRootsError::ConfigProjectIdCollision);
                }
            }
            let root = raw_root.trim();
            if root.is_empty() {
                return Err(WorkspaceProjectRootsError::EmptyProjectRoot(project_id));
            }
            pairs.push((project_id, PathBuf::from(root)));
            config_ids.insert(config_id.to_string(), project_id);
        }
        let mut roots = Self::try_from_pairs(pairs)?;
        roots.config_ids = config_ids;
        Ok(roots)
    }

    pub fn project_id_for_config_id(&self, config_id: &str) -> Option<ProjectId> {
        self.config_ids.get(config_id.trim()).copied()
    }

    pub(crate) fn root_for(&self, project_id: ProjectId) -> Option<&Path> {
        self.roots.get(&project_id).map(|root| root.path.as_path())
    }

    pub(crate) fn configured_root_for(
        &self,
        project_id: ProjectId,
    ) -> Option<&ConfiguredProjectRoot> {
        self.roots.get(&project_id)
    }
}

const MAX_CONFIG_PROJECT_ID_BYTES: usize = 128;

fn validate_config_project_id(raw_id: &str) -> Result<&str, WorkspaceProjectRootsError> {
    let value = raw_id.trim();
    if value.is_empty()
        || value.len() > MAX_CONFIG_PROJECT_ID_BYTES
        || value.contains('\0')
        || value.chars().any(char::is_control)
    {
        return Err(WorkspaceProjectRootsError::InvalidConfigProjectId);
    }
    Ok(value)
}

fn bounded_code(value: &str) -> String {
    format!(
        "{} chars",
        value.chars().count().min(MAX_CONFIG_PROJECT_ID_BYTES)
    )
}

/// The kind of task that selects a creation-time workspace default.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskKind {
    AiCoding,
    GeneralTerminal,
}

/// Resolve the safe default without turning the choice into a durable path.
pub fn default_workspace_choice(
    task_kind: TaskKind,
    project_default: Option<WorkspaceChoice>,
) -> WorkspaceChoice {
    match task_kind {
        TaskKind::AiCoding => WorkspaceChoice::NewWorktree,
        TaskKind::GeneralTerminal => project_default.unwrap_or(WorkspaceChoice::Ask),
    }
}

/// The explicit data supplied when a creation-time choice is resolved.
///
/// The choice remains separate from the durable reference: this request is
/// disposable and is never stored in Task facts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceRequest {
    pub choice: WorkspaceChoice,
    pub path: Option<PathBuf>,
    pub branch: Option<String>,
    pub external_confirmed: bool,
}

/// A workspace choice that cannot yet be persisted because Phase 6.3 still
/// needs to materialize the linked worktree.
#[derive(Clone, PartialEq, Eq)]
pub struct PendingWorktreeCandidate {
    pub path: PathBuf,
    pub branch: String,
    pub repository: RepositoryIdentity,
    pub relative_worktree_path: Option<PathBuf>,
}

impl fmt::Debug for PendingWorktreeCandidate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("PendingWorktreeCandidate(REDACTED)")
    }
}

/// The result of resolving a creation-time workspace request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkspaceResolution {
    Resolved(WorkspaceBinding),
    PendingWorktree(PendingWorktreeCandidate),
}

impl WorkspaceRequest {
    pub fn main() -> Self {
        Self {
            choice: WorkspaceChoice::Main,
            path: None,
            branch: None,
            external_confirmed: false,
        }
    }

    pub fn new_worktree(path: impl AsRef<Path>, branch: impl Into<String>) -> Self {
        Self {
            choice: WorkspaceChoice::NewWorktree,
            path: Some(path.as_ref().to_path_buf()),
            branch: Some(branch.into()),
            external_confirmed: false,
        }
    }

    pub fn ask() -> Self {
        Self {
            choice: WorkspaceChoice::Ask,
            path: None,
            branch: None,
            external_confirmed: false,
        }
    }

    pub fn external(path: impl AsRef<Path>) -> Self {
        Self {
            choice: WorkspaceChoice::External,
            path: Some(path.as_ref().to_path_buf()),
            branch: None,
            external_confirmed: false,
        }
    }

    pub fn confirmed_external(path: impl AsRef<Path>) -> Self {
        Self {
            choice: WorkspaceChoice::External,
            path: Some(path.as_ref().to_path_buf()),
            branch: None,
            external_confirmed: true,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceKind {
    Main,
    Worktree,
    External,
}

/// A stable identity for the common Git directory found while resolving a
/// workspace. The key is compared using final Windows identity semantics.
#[derive(Clone, PartialEq, Eq)]
pub struct RepositoryIdentity {
    root: PathBuf,
    git_dir: PathBuf,
    key: String,
    fingerprint: RepositoryFingerprint,
}

impl fmt::Debug for RepositoryIdentity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("RepositoryIdentity(REDACTED)")
    }
}

impl RepositoryIdentity {
    pub(crate) fn new(
        root: PathBuf,
        git_dir: PathBuf,
        key: String,
        fingerprint: RepositoryFingerprint,
    ) -> Self {
        Self {
            root,
            git_dir,
            key,
            fingerprint,
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn git_dir(&self) -> &Path {
        &self.git_dir
    }

    pub fn fingerprint(&self) -> &RepositoryFingerprint {
        &self.fingerprint
    }

    pub(crate) fn key(&self) -> &str {
        &self.key
    }
}

#[cfg(test)]
mod project_root_tests {
    use super::{path_identity_key, WorkspaceProjectRoots};
    use crate::domain::ProjectId;
    use std::fs;
    use std::path::PathBuf;
    use std::process::Command;

    #[test]
    fn project_root_lookup_is_keyed_by_the_host_project_id() {
        let project_id = ProjectId::new();
        let temp = tempfile::tempdir().expect("host root tempdir");
        let root = temp.path().to_path_buf();
        let roots = WorkspaceProjectRoots::try_from_pairs([(project_id, root.clone())])
            .expect("project roots");

        assert_eq!(
            roots.root_for(project_id).map(path_identity_key),
            Some(path_identity_key(&root))
        );
        let configured = roots
            .configured_root_for(project_id)
            .expect("configured root");
        assert!(!configured.identity().is_empty());
        assert_eq!(
            path_identity_key(configured.path()),
            path_identity_key(&root)
        );
    }

    #[test]
    fn replaced_directory_at_configured_root_path_fails_closed_on_resolve() {
        use crate::workspace::model::WorkspaceRequest;
        use crate::workspace::service::WorkspaceService;

        let project_id = ProjectId::new();
        let temp = tempfile::tempdir().expect("replaceable root tempdir");
        let root = temp.path().join("project");
        fs::create_dir(&root).expect("project root");
        fs::create_dir(root.join(".git")).expect("repository marker");
        fs::write(root.join(".git").join("HEAD"), "ref: refs/heads/main\n")
            .expect("repository HEAD");
        let roots = WorkspaceProjectRoots::try_from_pairs([(project_id, root.clone())])
            .expect("project roots");
        let admitted_identity = roots
            .configured_root_for(project_id)
            .expect("configured root")
            .identity()
            .to_string();
        let service = WorkspaceService::for_project(project_id, &roots).expect("workspace service");

        fs::remove_dir_all(&root).expect("remove admitted root");
        fs::create_dir(&root).expect("replacement root");
        fs::create_dir(root.join(".git")).expect("replacement repository marker");
        fs::write(root.join(".git").join("HEAD"), "ref: refs/heads/main\n")
            .expect("replacement HEAD");

        let error = service
            .resolve(WorkspaceRequest::main())
            .expect_err("replaced configured root must fail closed");
        assert!(
            matches!(
                error,
                crate::workspace::service::WorkspaceError::PathResolution { ref reason, .. }
                    if reason.contains("configured project root identity changed")
            ),
            "unexpected resolve error: {error:?}"
        );
        let replacement = crate::workspace::service::validate_host_workspace_path(&root, true)
            .expect("replacement path still validates as a directory");
        assert_ne!(
            replacement.identity, admitted_identity,
            "replacement at the same path must not reuse the admitted identity"
        );
    }

    #[test]
    fn arbitrary_config_project_id_requires_a_config_store_issuer() {
        let temp = tempfile::tempdir().expect("config project root");
        let result = WorkspaceProjectRoots::try_from_config([(
            "legacy-project-key".to_string(),
            temp.path().to_string_lossy().into_owned(),
        )]);
        assert!(matches!(
            result,
            Err(super::WorkspaceProjectRootsError::LegacyProjectIdRequiresAdapter(_))
        ));
    }

    #[test]
    fn duplicate_host_project_ids_fail_closed() {
        let first = ProjectId::new();
        let result = WorkspaceProjectRoots::try_from_pairs([
            (first, PathBuf::from("first-root")),
            (first, PathBuf::from("second-root")),
        ]);

        assert!(matches!(
            result,
            Err(super::WorkspaceProjectRootsError::DuplicateProjectId(project_id))
                if project_id == first
        ));
    }

    #[test]
    fn linked_configured_roots_are_rejected_before_final_path_resolution() {
        let temp = tempfile::tempdir().expect("project root tempdir");
        let root = temp.path().join("project");
        fs::create_dir(&root).expect("project root");
        let alias = temp.path().join("project-alias");

        #[cfg(windows)]
        {
            let output = Command::new("cmd")
                .args([
                    "/C",
                    "mklink",
                    "/J",
                    alias.to_str().expect("junction path"),
                    root.to_str().expect("root path"),
                ])
                .output()
                .expect("create project junction");
            assert!(
                output.status.success(),
                "project junction failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }

        #[cfg(not(windows))]
        std::os::unix::fs::symlink(&root, &alias).expect("create project symlink");

        let first = ProjectId::new();
        let second = ProjectId::new();
        let result = WorkspaceProjectRoots::try_from_pairs([(first, root), (second, alias)]);

        assert!(matches!(
            result,
            Err(super::WorkspaceProjectRootsError::MalformedProjectRoot(_))
        ));
    }
}

/// The host-owned, resolved workspace snapshot used by later resource
/// services. `durable_ref` preserves the existing task wire shape while the
/// surrounding fields keep the identity needed for safe comparisons.
#[derive(Clone, PartialEq, Eq)]
pub struct WorkspaceBinding {
    kind: WorkspaceKind,
    path: PathBuf,
    identity_key: String,
    durable_ref: WorkspaceRef,
    repository: Option<RepositoryIdentity>,
    relative_worktree_path: Option<PathBuf>,
    branch: Option<String>,
}

impl fmt::Debug for WorkspaceBinding {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("WorkspaceBinding(REDACTED)")
    }
}

impl WorkspaceBinding {
    pub(crate) fn new(
        kind: WorkspaceKind,
        path: PathBuf,
        identity_key: String,
        durable_ref: WorkspaceRef,
        repository: Option<RepositoryIdentity>,
        relative_worktree_path: Option<PathBuf>,
        branch: Option<String>,
    ) -> Self {
        Self {
            kind,
            path,
            identity_key,
            durable_ref,
            repository,
            relative_worktree_path,
            branch,
        }
    }

    pub fn kind(&self) -> WorkspaceKind {
        self.kind
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn durable_ref(&self) -> &WorkspaceRef {
        &self.durable_ref
    }

    pub fn repository(&self) -> Option<&RepositoryIdentity> {
        self.repository.as_ref()
    }

    pub fn relative_worktree_path(&self) -> Option<&Path> {
        self.relative_worktree_path.as_deref()
    }

    pub fn branch(&self) -> Option<&str> {
        self.branch.as_deref()
    }

    pub(crate) fn identity_key(&self) -> &str {
        &self.identity_key
    }

    pub(crate) fn durable_ref_mut(&mut self) -> &mut WorkspaceRef {
        &mut self.durable_ref
    }
}

impl WorkspaceBinding {
    pub fn same_workspace(&self, other: &Self) -> bool {
        let same_repository = match (&self.repository, &other.repository) {
            (None, None) => true,
            (Some(left), Some(right)) => {
                left.key() == right.key() && left.fingerprint() == right.fingerprint()
            }
            _ => false,
        };
        durable_refs_same_location(&self.durable_ref, &other.durable_ref)
            && self.identity_key == other.identity_key
            && self.kind == other.kind
            && self.branch == other.branch
            && same_repository
            && self.relative_worktree_path == other.relative_worktree_path
    }
}

pub(crate) fn durable_refs_same_location(left: &WorkspaceRef, right: &WorkspaceRef) -> bool {
    match (left, right) {
        (WorkspaceRef::Main, WorkspaceRef::Main)
        | (WorkspaceRef::Main, WorkspaceRef::MainWithFingerprint { .. })
        | (WorkspaceRef::MainWithFingerprint { .. }, WorkspaceRef::Main) => true,
        (
            WorkspaceRef::MainWithFingerprint {
                repository_fingerprint: left,
            },
            WorkspaceRef::MainWithFingerprint {
                repository_fingerprint: right,
            },
        ) => left == right,
        (
            WorkspaceRef::Worktree {
                path: left_path,
                branch: left_branch,
            },
            WorkspaceRef::Worktree {
                path: right_path,
                branch: right_branch,
            },
        )
        | (
            WorkspaceRef::Worktree {
                path: left_path,
                branch: left_branch,
            },
            WorkspaceRef::WorktreeWithFingerprint {
                path: right_path,
                branch: right_branch,
                ..
            },
        )
        | (
            WorkspaceRef::WorktreeWithFingerprint {
                path: left_path,
                branch: left_branch,
                ..
            },
            WorkspaceRef::Worktree {
                path: right_path,
                branch: right_branch,
            },
        ) => {
            path_identity_key(left_path) == path_identity_key(right_path)
                && left_branch == right_branch
        }
        (
            WorkspaceRef::WorktreeWithFingerprint {
                path: left_path,
                branch: left_branch,
                repository_fingerprint: left_fingerprint,
            },
            WorkspaceRef::WorktreeWithFingerprint {
                path: right_path,
                branch: right_branch,
                repository_fingerprint: right_fingerprint,
            },
        ) => {
            path_identity_key(left_path) == path_identity_key(right_path)
                && left_branch == right_branch
                && left_fingerprint == right_fingerprint
        }
        (WorkspaceRef::External { path: left }, WorkspaceRef::External { path: right }) => {
            path_identity_key(left) == path_identity_key(right)
        }
        (WorkspaceRef::HostBound { binding: left }, WorkspaceRef::HostBound { binding: right }) => {
            left.kind() == right.kind() && left.binding_fingerprint() == right.binding_fingerprint()
        }
        (
            WorkspaceRef::ExternalWithFingerprint { binding: left, .. },
            WorkspaceRef::ExternalWithFingerprint { binding: right, .. },
        ) => {
            left.kind() == right.kind() && left.binding_fingerprint() == right.binding_fingerprint()
        }
        _ => false,
    }
}

/// The resource categories that make a task workspace live and therefore
/// block implicit rebinding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Ord, PartialOrd)]
pub enum WorkspaceResource {
    Process,
    File,
    Git,
    Browser,
}

/// Return the comparison key for a final/canonical Windows path.
///
/// This intentionally operates on the final path supplied by the resolver;
/// it is public so protocol/UI callers can compare identity without using a
/// display string. On non-Windows hosts it retains case sensitivity.
pub fn path_identity_key(path: &Path) -> String {
    let value = path.to_string_lossy().into_owned();

    #[cfg(windows)]
    {
        let mut value = value.replace('/', "\\");
        let lower = value.to_ascii_lowercase();
        if lower.starts_with("\\\\?\\unc\\") {
            value = format!("\\\\{}", &value[8..]);
        } else if lower.starts_with("\\\\?\\") {
            value = value[4..].to_string();
        }
        while value.len() > 1 && value.ends_with('\\') {
            value.pop();
        }
        value.to_lowercase()
    }

    #[cfg(not(windows))]
    {
        value.replace('\\', "/")
    }
}

pub(crate) fn is_within(parent: &Path, candidate: &Path) -> bool {
    let parent = path_identity_key(parent);
    let candidate = path_identity_key(candidate);
    candidate == parent
        || candidate
            .strip_prefix(&parent)
            .is_some_and(|suffix| suffix.starts_with('\\') || suffix.starts_with('/'))
}

pub(crate) fn relative_location(parent: &Path, candidate: &Path) -> Option<PathBuf> {
    if path_identity_key(parent) == path_identity_key(candidate) {
        return Some(PathBuf::from("."));
    }

    if let Ok(relative) = candidate.strip_prefix(parent) {
        return Some(relative.to_path_buf());
    }

    #[cfg(windows)]
    {
        let parent_text = parent.to_string_lossy().replace('/', "\\");
        let candidate_text = candidate.to_string_lossy().replace('/', "\\");
        let parent_key = parent_text.to_ascii_lowercase();
        let candidate_key = candidate_text.to_ascii_lowercase();
        let suffix = candidate_key.strip_prefix(&parent_key)?;
        let suffix = suffix.strip_prefix('\\')?;
        let offset = candidate_text.len().checked_sub(suffix.len())?;
        Some(PathBuf::from(
            candidate_text[offset..].trim_start_matches('\\'),
        ))
    }

    #[cfg(not(windows))]
    {
        None
    }
}
