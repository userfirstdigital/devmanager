//! Bounded Git Desktop requests. Repository paths remain host-owned.

use super::git_service::{GitBranch, GitDiffResult, GitLogEntry, GitStatusResult};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum DesktopGitAction {
    Status,
    History {
        limit: u32,
        skip: u32,
    },
    FileDiff {
        relative_path: String,
        staged: bool,
    },
    CommitDiff {
        hash: String,
    },
    Branches,
    StagedDiff,
    Stage {
        paths: Vec<String>,
    },
    Unstage {
        paths: Vec<String>,
    },
    StageAll,
    UnstageAll,
    Commit {
        summary: String,
        description: Option<String>,
    },
    Fetch,
    Pull,
    Push,
    Sync,
    Publish {
        branch: String,
    },
    SwitchBranch {
        name: String,
    },
    CreateBranch {
        name: String,
    },
    DeleteBranch {
        name: String,
    },
}

impl DesktopGitAction {
    pub fn is_mutation(&self) -> bool {
        !matches!(
            self,
            Self::Status
                | Self::History { .. }
                | Self::FileDiff { .. }
                | Self::CommitDiff { .. }
                | Self::Branches
                | Self::StagedDiff
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum DesktopGitPayload {
    Status(GitStatusResult),
    History(Vec<GitLogEntry>),
    Diff(GitDiffResult),
    Branches(Vec<GitBranch>),
    StagedDiff(String),
    Commit(String),
    Done(String),
    Error(String),
}

/// An opaque, identity-bound configured repository selector. Paths stay on the host.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DesktopRepositoryEntry {
    pub id: String,
    pub label: String,
}
