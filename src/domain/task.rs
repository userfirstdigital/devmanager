use std::fmt;
use std::path::{Path, PathBuf};

use serde::de::{self, Deserializer};
use serde::ser::Error as SerError;
use serde::{Deserialize, Serialize, Serializer};
use sha2::{Digest, Sha256};

use crate::domain::canonical;
use crate::domain::id::{EnvironmentId, ProjectId, TaskId};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskValidationError {
    EmptyTitle,
    EmptyDescription,
    EmptyPath,
    EmptyBranch,
    EmptyRepositoryFingerprint,
    InvalidRepositoryFingerprint,
    EmptyPrincipalAuthority,
    EmptyPrincipalSubject,
    InvalidCreateState,
}

impl std::fmt::Display for TaskValidationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EmptyTitle => write!(f, "task title must be non-empty"),
            Self::EmptyDescription => write!(f, "task description must be non-empty when present"),
            Self::EmptyPath => write!(f, "workspace path must be non-empty"),
            Self::EmptyBranch => write!(f, "worktree branch must be non-empty"),
            Self::EmptyRepositoryFingerprint => {
                write!(f, "repository fingerprint must be non-empty")
            }
            Self::InvalidRepositoryFingerprint => {
                write!(
                    f,
                    "repository fingerprint must be a host-issued sha256 token"
                )
            }
            Self::EmptyPrincipalAuthority => write!(f, "principal authority must be non-empty"),
            Self::EmptyPrincipalSubject => write!(f, "principal subject must be non-empty"),
            Self::InvalidCreateState => {
                write!(
                    f,
                    "created task must be open with action_epoch 0 and revision 1"
                )
            }
        }
    }
}

impl std::error::Error for TaskValidationError {}

/// Immutable identity of the repository backing a task workspace.
///
/// The value is produced by the host after resolving the repository and is
/// persisted with new workspace references. It is deliberately opaque to
/// clients; callers may compare it but cannot manufacture a trusted identity
/// from a display path.
#[derive(Clone, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct RepositoryFingerprint(String);

impl fmt::Debug for RepositoryFingerprint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("RepositoryFingerprint(REDACTED)")
    }
}

impl RepositoryFingerprint {
    pub(crate) fn from_host_token(token: String) -> Result<Self, TaskValidationError> {
        if token.trim().is_empty() {
            return Err(TaskValidationError::EmptyRepositoryFingerprint);
        }
        let Some(digest) = token.strip_prefix("sha256:") else {
            return Err(TaskValidationError::InvalidRepositoryFingerprint);
        };
        if digest.len() != 64
            || !digest
                .bytes()
                .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
        {
            return Err(TaskValidationError::InvalidRepositoryFingerprint);
        }
        Ok(Self(token))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for RepositoryFingerprint {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let token = String::deserialize(deserializer)?;
        Self::from_host_token(token).map_err(de::Error::custom)
    }
}

const MAX_DURABLE_PATH_BYTES: usize = 32 * 1024;
const MAX_DURABLE_IDENTITY_BYTES: usize = 256;
const MAX_DURABLE_BRANCH_BYTES: usize = 512;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceBindingKind {
    Main,
    Worktree,
    External,
}

/// A host-captured path identity and, for metadata files, its exact content
/// digest and length. The type is public only as an opaque member of the
/// host-bound workspace fact; its fields and constructors remain crate-local.
#[derive(Clone, PartialEq, Eq)]
pub struct WorkspacePathFact {
    path: PathBuf,
    identity: String,
    /// Stable opaque handle allocated by the trusted host.  This is the only
    /// identity that crosses a durable/client boundary; `path` and `identity`
    /// remain host-private members of the live binding.
    opaque_id: String,
    content_length: Option<u64>,
    content_fingerprint: Option<RepositoryFingerprint>,
}

impl fmt::Debug for WorkspacePathFact {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("WorkspacePathFact(REDACTED)")
    }
}

impl WorkspacePathFact {
    pub(crate) fn new(
        path: PathBuf,
        identity: String,
        content_length: Option<u64>,
        content_fingerprint: Option<RepositoryFingerprint>,
    ) -> Result<Self, TaskValidationError> {
        let mut opaque_hasher = Sha256::new();
        opaque_hasher.update(b"devmanager-workspace-fact-id-v1\0");
        opaque_hasher.update(identity.as_bytes());
        let opaque_id = opaque_hasher
            .finalize()
            .iter()
            .take(16)
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let fact = Self {
            path,
            identity,
            opaque_id,
            content_length,
            content_fingerprint,
        };
        fact.validate()?;
        Ok(fact)
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn identity(&self) -> &str {
        &self.identity
    }

    pub(crate) fn opaque_id(&self) -> &str {
        &self.opaque_id
    }

    pub(crate) fn content_length(&self) -> Option<u64> {
        self.content_length
    }

    pub(crate) fn content_fingerprint(&self) -> Option<&RepositoryFingerprint> {
        self.content_fingerprint.as_ref()
    }

    fn validate(&self) -> Result<(), TaskValidationError> {
        if self.path.as_os_str().is_empty() {
            // A deserialized fact is intentionally pathless.  It can be
            // compared as a durable token, but the host must rebind it before
            // using it for filesystem access.
            if !self.identity.is_empty() {
                return Err(TaskValidationError::InvalidRepositoryFingerprint);
            }
        } else {
            check_path(&self.path)?;
            if self.path.to_str().is_none()
                || self.path.to_string_lossy().len() > MAX_DURABLE_PATH_BYTES
            {
                return Err(TaskValidationError::InvalidRepositoryFingerprint);
            }
            if self.identity.is_empty() || self.identity.len() > MAX_DURABLE_IDENTITY_BYTES {
                return Err(TaskValidationError::InvalidRepositoryFingerprint);
            }
        }
        if self.opaque_id.is_empty()
            || self.opaque_id.len() > MAX_DURABLE_IDENTITY_BYTES
            || !self
                .opaque_id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
        {
            return Err(TaskValidationError::InvalidRepositoryFingerprint);
        }
        if self.content_length.is_some() != self.content_fingerprint.is_some() {
            return Err(TaskValidationError::InvalidRepositoryFingerprint);
        }
        Ok(())
    }
}

/// The wire form deliberately omits the host path, filesystem identity, and
/// any other locator material.  A pathless fact is a durable reference only;
/// it is never sufficient authorization to open a file.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkspacePathFactWire {
    workspace_id: String,
    content_length: Option<u64>,
    content_fingerprint: Option<RepositoryFingerprint>,
}

impl Serialize for WorkspacePathFact {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        WorkspacePathFactWire {
            workspace_id: self.opaque_id.clone(),
            content_length: self.content_length,
            content_fingerprint: self.content_fingerprint.clone(),
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for WorkspacePathFact {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = WorkspacePathFactWire::deserialize(deserializer)?;
        let fact = Self {
            path: PathBuf::new(),
            identity: String::new(),
            opaque_id: wire.workspace_id,
            content_length: wire.content_length,
            content_fingerprint: wire.content_fingerprint,
        };
        fact.validate().map_err(de::Error::custom)?;
        Ok(fact)
    }
}

/// The durable, host-issued workspace authority projection. It contains the
/// paths needed to re-open the same workspace, but it is not a caller-facing
/// authority: only the host can construct a valid instance in this crate.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceBindingFact {
    kind: WorkspaceBindingKind,
    project_root: WorkspacePathFact,
    workspace_root: WorkspacePathFact,
    repository_root: Option<WorkspacePathFact>,
    common_git_dir: Option<WorkspacePathFact>,
    admin_dir: Option<WorkspacePathFact>,
    marker: Option<WorkspacePathFact>,
    commondir: Option<WorkspacePathFact>,
    gitdir: Option<WorkspacePathFact>,
    head: Option<WorkspacePathFact>,
    branch: Option<String>,
    binding_fingerprint: RepositoryFingerprint,
}

impl fmt::Debug for WorkspaceBindingFact {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("WorkspaceBindingFact(REDACTED)")
    }
}

impl WorkspaceBindingFact {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn issue(
        kind: WorkspaceBindingKind,
        project_root: WorkspacePathFact,
        workspace_root: WorkspacePathFact,
        repository_root: Option<WorkspacePathFact>,
        common_git_dir: Option<WorkspacePathFact>,
        admin_dir: Option<WorkspacePathFact>,
        marker: Option<WorkspacePathFact>,
        commondir: Option<WorkspacePathFact>,
        gitdir: Option<WorkspacePathFact>,
        head: Option<WorkspacePathFact>,
        branch: Option<String>,
    ) -> Result<Self, TaskValidationError> {
        let mut fact = Self {
            kind,
            project_root,
            workspace_root,
            repository_root,
            common_git_dir,
            admin_dir,
            marker,
            commondir,
            gitdir,
            head,
            branch,
            binding_fingerprint: RepositoryFingerprint::from_host_token(
                "sha256:".to_string() + &"0".repeat(64),
            )?,
        };
        fact.binding_fingerprint = fact.compute_fingerprint()?;
        fact.validate()?;
        Ok(fact)
    }

    pub(crate) fn project_root(&self) -> &WorkspacePathFact {
        &self.project_root
    }

    pub(crate) fn kind(&self) -> WorkspaceBindingKind {
        self.kind
    }

    pub(crate) fn workspace_root(&self) -> &WorkspacePathFact {
        &self.workspace_root
    }

    pub(crate) fn repository_root(&self) -> Option<&WorkspacePathFact> {
        self.repository_root.as_ref()
    }

    pub(crate) fn common_git_dir(&self) -> Option<&WorkspacePathFact> {
        self.common_git_dir.as_ref()
    }

    pub(crate) fn admin_dir(&self) -> Option<&WorkspacePathFact> {
        self.admin_dir.as_ref()
    }

    pub(crate) fn marker(&self) -> Option<&WorkspacePathFact> {
        self.marker.as_ref()
    }

    pub(crate) fn commondir(&self) -> Option<&WorkspacePathFact> {
        self.commondir.as_ref()
    }

    pub(crate) fn gitdir(&self) -> Option<&WorkspacePathFact> {
        self.gitdir.as_ref()
    }

    pub(crate) fn head(&self) -> Option<&WorkspacePathFact> {
        self.head.as_ref()
    }

    pub(crate) fn branch(&self) -> Option<&str> {
        self.branch.as_deref()
    }

    pub(crate) fn binding_fingerprint(&self) -> &RepositoryFingerprint {
        &self.binding_fingerprint
    }

    /// Compare the immutable filesystem relationship of two host bindings.
    /// Git legitimately replaces `HEAD` while a task is open (branch changes,
    /// detach/reattach, and some worktree operations). Its presence is part of
    /// the repository shape, but its file identity, content, and derived branch
    /// are mutable runtime state rather than repository identity.
    pub(crate) fn same_runtime_identity(&self, other: &Self) -> bool {
        fn same_fact(left: &WorkspacePathFact, right: &WorkspacePathFact) -> bool {
            left.opaque_id == right.opaque_id
                && left.content_length == right.content_length
                && left.content_fingerprint == right.content_fingerprint
        }
        fn same_optional(
            left: Option<&WorkspacePathFact>,
            right: Option<&WorkspacePathFact>,
        ) -> bool {
            match (left, right) {
                (Some(left), Some(right)) => same_fact(left, right),
                (None, None) => true,
                _ => false,
            }
        }
        fn same_head(left: Option<&WorkspacePathFact>, right: Option<&WorkspacePathFact>) -> bool {
            match (left, right) {
                (Some(_), Some(_)) => true,
                (None, None) => true,
                _ => false,
            }
        }

        self.kind == other.kind
            && same_fact(&self.project_root, &other.project_root)
            && same_fact(&self.workspace_root, &other.workspace_root)
            && same_optional(self.repository_root(), other.repository_root())
            && same_optional(self.common_git_dir(), other.common_git_dir())
            && same_optional(self.admin_dir(), other.admin_dir())
            && same_optional(self.marker(), other.marker())
            && same_optional(self.commondir(), other.commondir())
            && same_optional(self.gitdir(), other.gitdir())
            && same_head(self.head(), other.head())
    }

    pub(crate) fn validate(&self) -> Result<(), TaskValidationError> {
        self.project_root.validate()?;
        self.workspace_root.validate()?;
        for fact in [
            self.repository_root.as_ref(),
            self.common_git_dir.as_ref(),
            self.admin_dir.as_ref(),
            self.marker.as_ref(),
            self.commondir.as_ref(),
            self.gitdir.as_ref(),
            self.head.as_ref(),
        ]
        .into_iter()
        .flatten()
        {
            fact.validate()?;
        }
        if let Some(branch) = &self.branch {
            if branch.is_empty() || branch.len() > MAX_DURABLE_BRANCH_BYTES {
                return Err(TaskValidationError::EmptyBranch);
            }
            if !canonical::is_canonical(branch) {
                return Err(TaskValidationError::EmptyBranch);
            }
        }
        match self.kind {
            WorkspaceBindingKind::Main => {
                if self.repository_root.is_none()
                    || self.common_git_dir.is_none()
                    || self.marker.is_none()
                    || self.head.is_none()
                    || self.admin_dir.is_some()
                    || self.commondir.is_some()
                    || self.gitdir.is_some()
                    || self.branch.is_some()
                {
                    return Err(TaskValidationError::InvalidRepositoryFingerprint);
                }
            }
            WorkspaceBindingKind::Worktree => {
                if self.repository_root.is_none()
                    || self.common_git_dir.is_none()
                    || self.admin_dir.is_none()
                    || self.marker.is_none()
                    || self.commondir.is_none()
                    || self.gitdir.is_none()
                    || self.head.is_none()
                    || self.branch.is_none()
                {
                    return Err(TaskValidationError::InvalidRepositoryFingerprint);
                }
            }
            WorkspaceBindingKind::External => {
                let has_repository = self.repository_root.is_some()
                    || self.common_git_dir.is_some()
                    || self.marker.is_some()
                    || self.head.is_some();
                if self.admin_dir.is_some()
                    || self.commondir.is_some()
                    || self.gitdir.is_some()
                    || self.branch.is_some()
                    || (has_repository
                        && (self.repository_root.is_none()
                            || self.common_git_dir.is_none()
                            || self.marker.is_none()
                            || self.head.is_none()))
                {
                    return Err(TaskValidationError::InvalidRepositoryFingerprint);
                }
            }
        }
        if self.compute_fingerprint()? != self.binding_fingerprint {
            return Err(TaskValidationError::InvalidRepositoryFingerprint);
        }
        Ok(())
    }

    fn compute_fingerprint(&self) -> Result<RepositoryFingerprint, TaskValidationError> {
        let mut hasher = Sha256::new();
        hasher.update(b"devmanager-workspace-binding-v1\0");
        hasher.update(match self.kind {
            WorkspaceBindingKind::Main => b"main".as_slice(),
            WorkspaceBindingKind::Worktree => b"worktree".as_slice(),
            WorkspaceBindingKind::External => b"external".as_slice(),
        });
        hasher.update([0]);
        for (label, fact) in [
            ("project", Some(&self.project_root)),
            ("workspace", Some(&self.workspace_root)),
            ("repository", self.repository_root.as_ref()),
            ("common_git", self.common_git_dir.as_ref()),
            ("admin", self.admin_dir.as_ref()),
            ("marker", self.marker.as_ref()),
            ("commondir", self.commondir.as_ref()),
            ("gitdir", self.gitdir.as_ref()),
            ("head", self.head.as_ref()),
        ] {
            hasher.update(label.as_bytes());
            hasher.update([0]);
            match fact {
                Some(fact) => {
                    // Only the host-issued opaque ID and bounded safe
                    // metadata participate in the durable fingerprint.  The
                    // live path/identity is intentionally never projected.
                    hasher.update(fact.opaque_id().as_bytes());
                    hasher.update([0]);
                    if let Some(length) = fact.content_length {
                        hasher.update(length.to_le_bytes());
                        hasher.update([1]);
                        hasher.update(
                            fact.content_fingerprint
                                .as_ref()
                                .expect("content fingerprint accompanies length")
                                .as_str()
                                .as_bytes(),
                        );
                    } else {
                        hasher.update([0]);
                    }
                }
                None => {
                    hasher.update([0xff]);
                }
            }
            hasher.update([0xff]);
        }
        if let Some(branch) = &self.branch {
            hasher.update(branch.as_bytes());
        }
        let digest = hasher.finalize();
        RepositoryFingerprint::from_host_token(format!(
            "sha256:{}",
            digest
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>()
        ))
    }
}

#[derive(Clone, PartialEq, Eq)]
pub enum WorkspaceRef {
    Main,
    Worktree {
        path: PathBuf,
        branch: String,
    },
    External {
        path: PathBuf,
    },
    MainWithFingerprint {
        repository_fingerprint: RepositoryFingerprint,
    },
    WorktreeWithFingerprint {
        path: PathBuf,
        branch: String,
        repository_fingerprint: RepositoryFingerprint,
    },
    HostBound {
        binding: WorkspaceBindingFact,
    },
    ExternalWithFingerprint {
        path: PathBuf,
        binding: WorkspaceBindingFact,
    },
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
enum WorkspaceRefWire {
    Main,
    Worktree {
        path: PathBuf,
        branch: String,
    },
    External {
        path: PathBuf,
    },
    ExternalBound {
        binding: WorkspaceBindingFact,
    },
    MainWithFingerprint {
        repository_fingerprint: RepositoryFingerprint,
    },
    WorktreeWithFingerprint {
        path: PathBuf,
        branch: String,
        repository_fingerprint: RepositoryFingerprint,
    },
    HostBound {
        binding: WorkspaceBindingFact,
    },
}

impl Serialize for WorkspaceRef {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let wire = match self {
            Self::Main => WorkspaceRefWire::Main,
            Self::Worktree { .. } | Self::External { .. } => {
                return Err(S::Error::custom(
                    "unresolved workspace path cannot cross a durable boundary",
                ));
            }
            Self::MainWithFingerprint {
                repository_fingerprint,
            } => WorkspaceRefWire::MainWithFingerprint {
                repository_fingerprint: repository_fingerprint.clone(),
            },
            Self::WorktreeWithFingerprint { .. } => {
                return Err(S::Error::custom(
                    "workspace path cannot cross a durable boundary",
                ));
            }
            Self::HostBound { binding } => WorkspaceRefWire::HostBound {
                binding: binding.clone(),
            },
            Self::ExternalWithFingerprint { binding, .. } => WorkspaceRefWire::ExternalBound {
                binding: binding.clone(),
            },
        };
        wire.serialize(serializer)
    }
}

impl fmt::Debug for WorkspaceRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let variant = match self {
            Self::Main => "Main",
            Self::Worktree { .. } => "Worktree",
            Self::External { .. } => "External",
            Self::MainWithFingerprint { .. } => "MainWithFingerprint",
            Self::WorktreeWithFingerprint { .. } => "WorktreeWithFingerprint",
            Self::HostBound { .. } => "HostBound",
            Self::ExternalWithFingerprint { .. } => "ExternalWithFingerprint",
        };
        write!(f, "WorkspaceRef::{variant}(REDACTED)")
    }
}

impl WorkspaceRef {
    pub fn worktree(
        path: impl Into<PathBuf>,
        branch: impl Into<String>,
    ) -> Result<Self, TaskValidationError> {
        let path = validate_path(path.into())?;
        let branch = canonicalize_branch(branch.into())?;
        Ok(Self::Worktree { path, branch })
    }

    pub fn external(path: impl Into<PathBuf>) -> Result<Self, TaskValidationError> {
        let path = validate_path(path.into())?;
        Ok(Self::External { path })
    }

    pub(crate) fn worktree_with_fingerprint(
        path: impl Into<PathBuf>,
        branch: impl Into<String>,
        repository_fingerprint: RepositoryFingerprint,
    ) -> Result<Self, TaskValidationError> {
        if repository_fingerprint.as_str().trim().is_empty() {
            return Err(TaskValidationError::EmptyRepositoryFingerprint);
        }
        let path = validate_path(path.into())?;
        let branch = canonicalize_branch(branch.into())?;
        Ok(Self::WorktreeWithFingerprint {
            path,
            branch,
            repository_fingerprint,
        })
    }

    pub fn repository_fingerprint(&self) -> Option<&RepositoryFingerprint> {
        match self {
            Self::MainWithFingerprint {
                repository_fingerprint,
            }
            | Self::WorktreeWithFingerprint {
                repository_fingerprint,
                ..
            } => Some(repository_fingerprint),
            Self::HostBound { binding } => Some(binding.binding_fingerprint()),
            Self::ExternalWithFingerprint { binding, .. } => Some(binding.binding_fingerprint()),
            Self::Main | Self::Worktree { .. } | Self::External { .. } => None,
        }
    }

    pub(crate) fn host_binding(&self) -> Option<&WorkspaceBindingFact> {
        match self {
            Self::HostBound { binding } | Self::ExternalWithFingerprint { binding, .. } => {
                Some(binding)
            }
            _ => None,
        }
    }

    pub fn validate(&self) -> Result<(), TaskValidationError> {
        match self {
            Self::Main => Ok(()),
            Self::Worktree { path, branch } => {
                check_path(path)?;
                if !canonical::is_canonical(branch) {
                    return Err(TaskValidationError::EmptyBranch);
                }
                Ok(())
            }
            Self::External { path } => check_path(path),
            Self::MainWithFingerprint {
                repository_fingerprint,
            } => {
                if repository_fingerprint.as_str().trim().is_empty() {
                    return Err(TaskValidationError::EmptyRepositoryFingerprint);
                }
                Ok(())
            }
            Self::WorktreeWithFingerprint {
                path,
                branch,
                repository_fingerprint,
            } => {
                check_path(path)?;
                if !canonical::is_canonical(branch) {
                    return Err(TaskValidationError::EmptyBranch);
                }
                if repository_fingerprint.as_str().trim().is_empty() {
                    return Err(TaskValidationError::EmptyRepositoryFingerprint);
                }
                Ok(())
            }
            Self::HostBound { binding } => binding.validate(),
            Self::ExternalWithFingerprint { path, binding } => {
                binding.validate()?;
                if binding.kind() != WorkspaceBindingKind::External {
                    return Err(TaskValidationError::InvalidRepositoryFingerprint);
                }
                if path.as_os_str().is_empty() {
                    if !binding.workspace_root().path().as_os_str().is_empty() {
                        return Err(TaskValidationError::InvalidRepositoryFingerprint);
                    }
                } else if binding.workspace_root().path() != path {
                    return Err(TaskValidationError::InvalidRepositoryFingerprint);
                }
                Ok(())
            }
        }
    }
}

impl<'de> Deserialize<'de> for WorkspaceRef {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        match WorkspaceRefWire::deserialize(deserializer)? {
            WorkspaceRefWire::Main => Ok(Self::Main),
            WorkspaceRefWire::Worktree { .. }
            | WorkspaceRefWire::External { .. }
            | WorkspaceRefWire::WorktreeWithFingerprint { .. } => Err(de::Error::custom(
                "durable workspace wire must use an opaque host binding",
            )),
            WorkspaceRefWire::ExternalBound { binding } => {
                // The path is a host-private locator and is intentionally
                // absent from this durable form.  Rebinding is required
                // before a service can use the reference.
                if binding.kind() != WorkspaceBindingKind::External {
                    return Err(de::Error::custom(
                        "opaque external workspace binding has invalid kind",
                    ));
                }
                binding.validate().map_err(de::Error::custom)?;
                Ok(Self::ExternalWithFingerprint {
                    path: PathBuf::new(),
                    binding,
                })
            }
            WorkspaceRefWire::MainWithFingerprint {
                repository_fingerprint,
            } => {
                let value = Self::MainWithFingerprint {
                    repository_fingerprint,
                };
                value.validate().map_err(de::Error::custom)?;
                Ok(value)
            }
            WorkspaceRefWire::HostBound { binding } => {
                binding.validate().map_err(de::Error::custom)?;
                Ok(Self::HostBound { binding })
            }
        }
    }
}

/// The user's choice at task creation time, before the host resolves a
/// concrete, durable [`WorkspaceRef`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceChoice {
    Main,
    NewWorktree,
    Ask,
    External,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskLifecycle {
    Open,
    Settled,
    Closing,
    Archived,
    /// Permanent user deletion. The durable task journal remains as an
    /// auditable tombstone, but the task is never selectable or reopenable.
    Deleted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskConnectivity {
    Connected,
    Disconnected,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskAttention {
    None,
    NeedsAnswer,
    NeedsApproval,
    UncertainOutcome,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskActivity {
    Idle,
    Working,
    Settling,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewReadiness {
    NotReady,
    Ready,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VisibleTaskStatus {
    Disconnected,
    Failed,
    UncertainOutcome,
    NeedsApproval,
    NeedsAnswer,
    Working,
    Settling,
    ReadyForReview,
    Idle,
}

impl VisibleTaskStatus {
    pub fn derive(
        connectivity: TaskConnectivity,
        attention: TaskAttention,
        activity: TaskActivity,
        review_readiness: ReviewReadiness,
    ) -> Self {
        if connectivity == TaskConnectivity::Disconnected {
            return Self::Disconnected;
        }
        match attention {
            TaskAttention::Failed => return Self::Failed,
            TaskAttention::UncertainOutcome => return Self::UncertainOutcome,
            TaskAttention::NeedsApproval => return Self::NeedsApproval,
            TaskAttention::NeedsAnswer => return Self::NeedsAnswer,
            TaskAttention::None => {}
        }
        match activity {
            TaskActivity::Working => return Self::Working,
            TaskActivity::Settling => return Self::Settling,
            TaskActivity::Idle => {}
        }
        match review_readiness {
            ReviewReadiness::Ready => Self::ReadyForReview,
            ReviewReadiness::NotReady => Self::Idle,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskAssignment {
    LocalOwner,
    ExternalPrincipal { authority: String, subject: String },
}

impl TaskAssignment {
    pub fn external_principal(
        authority: impl Into<String>,
        subject: impl Into<String>,
    ) -> Result<Self, TaskValidationError> {
        let authority = canonicalize_principal(
            authority.into(),
            TaskValidationError::EmptyPrincipalAuthority,
        )?;
        let subject =
            canonicalize_principal(subject.into(), TaskValidationError::EmptyPrincipalSubject)?;
        Ok(Self::ExternalPrincipal { authority, subject })
    }

    pub fn validate(&self) -> Result<(), TaskValidationError> {
        match self {
            Self::LocalOwner => Ok(()),
            Self::ExternalPrincipal { authority, subject } => {
                if !canonical::is_canonical(authority) {
                    return Err(TaskValidationError::EmptyPrincipalAuthority);
                }
                if !canonical::is_canonical(subject) {
                    return Err(TaskValidationError::EmptyPrincipalSubject);
                }
                Ok(())
            }
        }
    }
}

impl<'de> Deserialize<'de> for TaskAssignment {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(rename_all = "snake_case", deny_unknown_fields)]
        enum TaskAssignmentWire {
            LocalOwner,
            ExternalPrincipal { authority: String, subject: String },
        }

        match TaskAssignmentWire::deserialize(deserializer)? {
            TaskAssignmentWire::LocalOwner => Ok(Self::LocalOwner),
            TaskAssignmentWire::ExternalPrincipal { authority, subject } => {
                Self::external_principal(authority, subject).map_err(de::Error::custom)
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TaskFacts {
    pub id: TaskId,
    pub environment_id: EnvironmentId,
    pub title: String,
    pub description: Option<String>,
    pub project_id: ProjectId,
    pub workspace: WorkspaceRef,
    pub assignment: TaskAssignment,
    pub lifecycle: TaskLifecycle,
    pub action_epoch: u64,
    pub revision: u64,
    pub created_at_ms: i64,
}

impl TaskFacts {
    pub fn new(
        environment_id: EnvironmentId,
        title: impl Into<String>,
        description: Option<String>,
        project_id: ProjectId,
        workspace: WorkspaceRef,
        assignment: TaskAssignment,
        created_at_ms: i64,
    ) -> Result<Self, TaskValidationError> {
        let title = Self::canonicalize_title(title)?;
        let description = Self::canonicalize_description(description)?;
        workspace.validate()?;
        assignment.validate()?;

        Ok(Self {
            id: TaskId::new(),
            environment_id,
            title,
            description,
            project_id,
            workspace,
            assignment,
            lifecycle: TaskLifecycle::Open,
            action_epoch: 0,
            revision: 0,
            created_at_ms,
        })
    }

    pub fn canonicalize_title(title: impl Into<String>) -> Result<String, TaskValidationError> {
        canonical::canonicalize(title.into()).ok_or(TaskValidationError::EmptyTitle)
    }

    pub fn canonicalize_description(
        description: Option<String>,
    ) -> Result<Option<String>, TaskValidationError> {
        match description {
            Some(value) => Ok(Some(
                canonical::canonicalize(value).ok_or(TaskValidationError::EmptyDescription)?,
            )),
            None => Ok(None),
        }
    }

    pub fn validate_content(&self) -> Result<(), TaskValidationError> {
        if !canonical::is_canonical(&self.title) {
            return Err(TaskValidationError::EmptyTitle);
        }
        match &self.description {
            Some(value) if !canonical::is_canonical(value) => {
                return Err(TaskValidationError::EmptyDescription);
            }
            Some(_) | None => {}
        }
        self.workspace.validate()?;
        self.assignment.validate()?;
        Ok(())
    }

    pub fn validate_for_create(&self) -> Result<(), TaskValidationError> {
        self.validate_content()?;
        if self.lifecycle != TaskLifecycle::Open || self.action_epoch != 0 || self.revision != 1 {
            return Err(TaskValidationError::InvalidCreateState);
        }
        Ok(())
    }
}

impl<'de> Deserialize<'de> for TaskFacts {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct TaskFactsWire {
            id: TaskId,
            environment_id: EnvironmentId,
            title: String,
            description: Option<String>,
            project_id: ProjectId,
            workspace: WorkspaceRef,
            assignment: TaskAssignment,
            lifecycle: TaskLifecycle,
            action_epoch: u64,
            revision: u64,
            created_at_ms: i64,
        }

        let wire = TaskFactsWire::deserialize(deserializer)?;
        let title = Self::canonicalize_title(wire.title).map_err(de::Error::custom)?;
        let description =
            Self::canonicalize_description(wire.description).map_err(de::Error::custom)?;
        // WorkspaceRef/TaskAssignment deserialize already produce canonical values.
        wire.workspace.validate().map_err(de::Error::custom)?;
        wire.assignment.validate().map_err(de::Error::custom)?;

        // Preserve every persisted identity/lifecycle/revision/timestamp field from the wire.
        Ok(Self {
            id: wire.id,
            environment_id: wire.environment_id,
            title,
            description,
            project_id: wire.project_id,
            workspace: wire.workspace,
            assignment: wire.assignment,
            lifecycle: wire.lifecycle,
            action_epoch: wire.action_epoch,
            revision: wire.revision,
            created_at_ms: wire.created_at_ms,
        })
    }
}

fn canonicalize_branch(value: String) -> Result<String, TaskValidationError> {
    canonical::canonicalize(value).ok_or(TaskValidationError::EmptyBranch)
}

fn canonicalize_principal(
    value: String,
    empty_error: TaskValidationError,
) -> Result<String, TaskValidationError> {
    canonical::canonicalize(value).ok_or(empty_error)
}

fn validate_path(path: PathBuf) -> Result<PathBuf, TaskValidationError> {
    check_path(&path)?;
    Ok(path)
}

fn check_path(path: &Path) -> Result<(), TaskValidationError> {
    if path.as_os_str().is_empty() || path_has_nul(path) {
        return Err(TaskValidationError::EmptyPath);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        RepositoryFingerprint, WorkspaceBindingFact, WorkspaceBindingKind, WorkspacePathFact,
        WorkspaceRef,
    };

    fn fact(path: &str, identity: &str) -> WorkspacePathFact {
        WorkspacePathFact::new(path.into(), identity.into(), None, None)
            .expect("valid host-only path fact")
    }

    fn host_binding_fact() -> WorkspaceBindingFact {
        WorkspaceBindingFact::issue(
            WorkspaceBindingKind::Main,
            fact(
                r"C:\Users\attacker\secret-workspace",
                "windows:volume:user-device",
            ),
            fact(
                r"C:\Users\attacker\secret-workspace",
                "windows:volume:user-device",
            ),
            Some(fact(r"C:\Users\attacker\secret-workspace", "windows:repo")),
            Some(fact(
                r"C:\Users\attacker\secret-workspace\.git",
                "windows:git",
            )),
            None,
            Some(fact(
                r"C:\Users\attacker\secret-workspace\.git",
                "windows:marker",
            )),
            None,
            None,
            Some(fact(
                r"C:\Users\attacker\secret-workspace\.git\HEAD",
                "windows:head",
            )),
            None,
        )
        .expect("valid host binding fact")
    }

    #[test]
    fn durable_workspace_facts_are_opaque_to_paths_identities_and_secrets() {
        let binding = host_binding_fact();
        let durable = WorkspaceRef::HostBound {
            binding: binding.clone(),
        };
        let encoded_path_fact = serde_json::to_string(&binding.project_root()).unwrap();
        let encoded_binding = serde_json::to_string(&binding).unwrap();
        let encoded_ref = serde_json::to_string(&durable).unwrap();

        for encoded in [encoded_path_fact, encoded_binding, encoded_ref] {
            assert!(!encoded.contains(r"C:\Users\attacker\secret-workspace"));
            assert!(!encoded.contains("windows:volume:user-device"));
            assert!(!encoded.contains("TOP_SECRET"));
        }
    }

    #[test]
    fn opaque_repository_fingerprint_rejects_an_untrusted_wire_token() {
        let result = serde_json::from_str::<RepositoryFingerprint>(r#""forged""#);
        assert!(
            result.is_err(),
            "opaque repository identities must not accept arbitrary client tokens"
        );
    }
}

fn path_has_nul(path: &Path) -> bool {
    path.to_string_lossy().contains('\0')
}
