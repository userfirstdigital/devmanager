use std::path::{Path, PathBuf};

use serde::de::{self, Deserializer};
use serde::{Deserialize, Serialize};

use crate::domain::canonical;
use crate::domain::id::{EnvironmentId, ProjectId, TaskId};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskValidationError {
    EmptyTitle,
    EmptyDescription,
    EmptyPath,
    EmptyBranch,
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceRef {
    Main,
    Worktree { path: PathBuf, branch: String },
    External { path: PathBuf },
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
        }
    }
}

impl<'de> Deserialize<'de> for WorkspaceRef {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(rename_all = "snake_case")]
        enum WorkspaceRefWire {
            Main,
            Worktree { path: PathBuf, branch: String },
            External { path: PathBuf },
        }

        match WorkspaceRefWire::deserialize(deserializer)? {
            WorkspaceRefWire::Main => Ok(Self::Main),
            WorkspaceRefWire::Worktree { path, branch } => {
                Self::worktree(path, branch).map_err(de::Error::custom)
            }
            WorkspaceRefWire::External { path } => Self::external(path).map_err(de::Error::custom),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskLifecycle {
    Open,
    Closing,
    Archived,
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
        #[serde(rename_all = "snake_case")]
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

fn path_has_nul(path: &Path) -> bool {
    path.to_string_lossy().contains('\0')
}
