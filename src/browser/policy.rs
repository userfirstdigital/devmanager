use super::BrowserError;
use serde::{Deserialize, Serialize};
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum BrowserRisk {
    Normal,
    Financial,
    Destructive,
    AccountSecurity,
    PermissionChange,
    OutsideWorkspaceFile,
    OsPermission,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct BrowserApprovalPolicy;

impl BrowserApprovalPolicy {
    pub fn trust_project() -> Self {
        Self
    }

    pub fn requires_confirmation(&self, risk: BrowserRisk) -> bool {
        !matches!(risk, BrowserRisk::Normal)
    }
}

pub fn classify_upload_path(
    workspace_root: impl AsRef<Path>,
    candidate: impl AsRef<Path>,
) -> Result<(PathBuf, BrowserRisk), BrowserError> {
    let workspace_root = canonicalize_existing(workspace_root.as_ref())?;
    let candidate = canonicalize_existing(candidate.as_ref())?;
    let risk = if candidate.starts_with(&workspace_root) {
        BrowserRisk::Normal
    } else {
        BrowserRisk::OutsideWorkspaceFile
    };
    Ok((candidate, risk))
}

fn canonicalize_existing(path: &Path) -> Result<PathBuf, BrowserError> {
    path.canonicalize().map_err(|error| {
        if error.kind() == ErrorKind::NotFound {
            BrowserError::MissingFile {
                path: path.to_path_buf(),
            }
        } else {
            BrowserError::Io {
                operation: "canonicalize path".to_string(),
                path: path.to_path_buf(),
                message: error.to_string(),
            }
        }
    })
}
