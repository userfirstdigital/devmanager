use std::path::{Path, PathBuf};

use serde::de::{self, Deserializer};
use serde::{Deserialize, Serialize};

use crate::domain::id::{EnvironmentId, ProjectId, TaskId};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskValidationError {
    EmptyTitle,
    EmptyDescription,
    EmptyPath,
    EmptyBranch,
    EmptyPrincipalAuthority,
    EmptyPrincipalSubject,
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
        let branch = validate_non_empty(branch.into(), TaskValidationError::EmptyBranch)?;
        Ok(Self::Worktree { path, branch })
    }

    pub fn external(path: impl Into<PathBuf>) -> Result<Self, TaskValidationError> {
        let path = validate_path(path.into())?;
        Ok(Self::External { path })
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
        let authority = validate_non_empty(
            authority.into(),
            TaskValidationError::EmptyPrincipalAuthority,
        )?;
        let subject =
            validate_non_empty(subject.into(), TaskValidationError::EmptyPrincipalSubject)?;
        Ok(Self::ExternalPrincipal { authority, subject })
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
        let title = validate_non_empty(title.into(), TaskValidationError::EmptyTitle)?;
        let description = match description {
            Some(value) => Some(validate_non_empty(
                value,
                TaskValidationError::EmptyDescription,
            )?),
            None => None,
        };

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
        let title = validate_non_empty(wire.title, TaskValidationError::EmptyTitle)
            .map_err(de::Error::custom)?;
        let description = match wire.description {
            Some(value) => Some(
                validate_non_empty(value, TaskValidationError::EmptyDescription)
                    .map_err(de::Error::custom)?,
            ),
            None => None,
        };

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

fn validate_non_empty(
    value: String,
    empty_error: TaskValidationError,
) -> Result<String, TaskValidationError> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(empty_error);
    }
    Ok(trimmed.to_string())
}

fn validate_path(path: PathBuf) -> Result<PathBuf, TaskValidationError> {
    if path.as_os_str().is_empty() || path_has_nul(&path) {
        return Err(TaskValidationError::EmptyPath);
    }
    Ok(path)
}

fn path_has_nul(path: &Path) -> bool {
    path.to_string_lossy().contains('\0')
}
