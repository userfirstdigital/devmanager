use std::path::{Path, PathBuf};

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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TaskLifecycle {
    Open,
    Closing,
    Archived,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
