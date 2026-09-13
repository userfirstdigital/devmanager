//! Selectable native text diff of two immutable prompt versions.

use crate::domain::id::PromptVersionId;
use crate::prompts::diff::{diff_versions, DiffStatus, LineChange, LineHunk, TruncationMarker};
use crate::prompts::model::PromptVersion;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VersionDiffView {
    pub old_version_id: PromptVersionId,
    pub new_version_id: PromptVersionId,
    pub old_version: u32,
    pub new_version: u32,
    pub old_hash: [u8; 32],
    pub new_hash: [u8; 32],
    pub hunks: Vec<LineHunk>,
    pub status: DiffStatus,
    pub truncation: Option<TruncationMarker>,
    pub selected_change: Option<usize>,
    pub preserves_original_bodies: bool,
}

impl VersionDiffView {
    pub fn from_versions(old: &PromptVersion, new: &PromptVersion) -> Self {
        let diff = diff_versions(&old.body, &new.body);
        Self {
            old_version_id: old.id,
            new_version_id: new.id,
            old_version: old.version,
            new_version: new.version,
            old_hash: old.body_sha256,
            new_hash: new.body_sha256,
            hunks: diff.hunks().to_vec(),
            status: diff.status(),
            truncation: diff.truncation(),
            selected_change: None,
            preserves_original_bodies: diff.old_body() == old.body.as_str()
                && diff.new_body() == new.body.as_str(),
        }
    }

    pub fn select_change(&mut self, index: usize) -> Option<&LineChange> {
        let mut seen = 0usize;
        for hunk in &self.hunks {
            for change in &hunk.changes {
                if seen == index {
                    self.selected_change = Some(index);
                    return Some(change);
                }
                seen += 1;
            }
        }
        None
    }

    pub fn accessible_status(&self) -> String {
        format!(
            "diff v{} to v{} {:?}",
            self.old_version, self.new_version, self.status
        )
    }
}
