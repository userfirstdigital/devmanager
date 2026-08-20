//! Profile-scoped persistence for unsent task composer input.
//!
//! Drafts are client-local preferences, never host facts. The key includes the
//! exact Task and primary Agent identities so a restarted/rebound provider can
//! never inherit text that belonged to another conversation.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::domain::{AgentSessionId, TaskId};
use crate::ui::components::text_field::{MAX_TEXT_FIELD_BYTES, MAX_TEXT_FIELD_SCALARS};
use crate::ui::task_cockpit::composer::{
    ComposerDraftProjection, MAX_COMPOSER_ATTACHMENTS, MAX_PROMPT_ID_SCALARS,
};
use crate::ui::workspace_layout::write_atomically;

const DRAFT_SCHEMA: &str = "devmanager.composer-drafts/v1";
const DRAFT_FILE_NAME: &str = "composer-drafts.json";
pub const MAX_PERSISTED_COMPOSER_DRAFTS: usize = 128;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
pub struct ComposerDraftKey {
    pub task_id: TaskId,
    pub agent_session_id: AgentSessionId,
}

impl ComposerDraftKey {
    pub const fn new(task_id: TaskId, agent_session_id: AgentSessionId) -> Self {
        Self {
            task_id,
            agent_session_id,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
struct StoredComposerDraft {
    key: ComposerDraftKey,
    draft: ComposerDraftProjection,
}

#[derive(Debug, Serialize, Deserialize)]
struct ComposerDraftFile {
    schema: String,
    drafts: Vec<StoredComposerDraft>,
}

#[derive(Clone, Debug)]
pub struct ComposerDraftStore {
    path: PathBuf,
}

impl ComposerDraftStore {
    pub fn at_path(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn at_profile_root(root: impl AsRef<Path>) -> Self {
        Self::at_path(root.as_ref().join(DRAFT_FILE_NAME))
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn load(&self) -> BTreeMap<ComposerDraftKey, ComposerDraftProjection> {
        let Ok(bytes) = fs::read(&self.path) else {
            return BTreeMap::new();
        };
        let Ok(file) = serde_json::from_slice::<ComposerDraftFile>(&bytes) else {
            return BTreeMap::new();
        };
        if file.schema != DRAFT_SCHEMA || file.drafts.len() > MAX_PERSISTED_COMPOSER_DRAFTS {
            return BTreeMap::new();
        }
        let mut drafts = BTreeMap::new();
        for stored in file.drafts {
            if !valid_draft(&stored.draft) || drafts.insert(stored.key, stored.draft).is_some() {
                return BTreeMap::new();
            }
        }
        drafts
    }

    pub fn save(
        &self,
        drafts: &BTreeMap<ComposerDraftKey, ComposerDraftProjection>,
    ) -> io::Result<()> {
        if drafts.len() > MAX_PERSISTED_COMPOSER_DRAFTS
            || drafts.values().any(|draft| !valid_draft(draft))
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "composer draft preference is outside its bounded contract",
            ));
        }
        let file = ComposerDraftFile {
            schema: DRAFT_SCHEMA.to_string(),
            drafts: drafts
                .iter()
                .map(|(key, draft)| StoredComposerDraft {
                    key: *key,
                    draft: draft.clone(),
                })
                .collect(),
        };
        let bytes = serde_json::to_vec_pretty(&file)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        write_atomically(&self.path, &bytes)
    }
}

fn valid_draft(draft: &ComposerDraftProjection) -> bool {
    draft.text.chars().count() <= MAX_TEXT_FIELD_SCALARS
        && draft.text.len() <= MAX_TEXT_FIELD_BYTES
        && draft.attachments.len() <= MAX_COMPOSER_ATTACHMENTS
        && draft.attachments.iter().all(|attachment| {
            attachment.label.chars().count() <= MAX_TEXT_FIELD_SCALARS
                && attachment.label.len() <= MAX_TEXT_FIELD_BYTES
        })
        && draft.prompt.as_ref().is_none_or(|prompt| {
            prompt.prompt_id.chars().count() <= MAX_PROMPT_ID_SCALARS
                && prompt.body.chars().count() <= MAX_TEXT_FIELD_SCALARS
                && prompt.body.len() <= MAX_TEXT_FIELD_BYTES
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::task_cockpit::composer::{AttachmentKind, ComposerAttachmentProjection};

    #[test]
    fn profile_store_round_trips_exact_task_agent_drafts() {
        let directory = tempfile::tempdir().expect("temp dir");
        let store = ComposerDraftStore::at_profile_root(directory.path());
        let key = ComposerDraftKey::new(TaskId::new(), AgentSessionId::new());
        let draft = ComposerDraftProjection {
            text: "keep this through restart".to_string(),
            attachments: vec![ComposerAttachmentProjection {
                artifact_id: crate::domain::ArtifactId::new(),
                kind: AttachmentKind::File,
                label: "plan.md".to_string(),
            }],
            prompt: None,
        };
        let drafts = BTreeMap::from([(key, draft.clone())]);

        store.save(&drafts).expect("save drafts");

        assert_eq!(store.load(), BTreeMap::from([(key, draft)]));
    }

    #[test]
    fn corrupt_or_oversized_draft_storage_fails_closed() {
        let directory = tempfile::tempdir().expect("temp dir");
        let store = ComposerDraftStore::at_profile_root(directory.path());
        fs::write(store.path(), b"{not json").expect("write corrupt store");
        assert!(store.load().is_empty());

        let key = ComposerDraftKey::new(TaskId::new(), AgentSessionId::new());
        let oversized = ComposerDraftProjection {
            text: "x".repeat(MAX_TEXT_FIELD_SCALARS + 1),
            attachments: Vec::new(),
            prompt: None,
        };
        assert_eq!(
            store
                .save(&BTreeMap::from([(key, oversized)]))
                .expect_err("oversized draft")
                .kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn duplicate_draft_identity_fails_closed_instead_of_selecting_a_winner() {
        let directory = tempfile::tempdir().expect("temp dir");
        let store = ComposerDraftStore::at_profile_root(directory.path());
        let key = ComposerDraftKey::new(TaskId::new(), AgentSessionId::new());
        let encoded = serde_json::json!({
            "schema": DRAFT_SCHEMA,
            "drafts": [
                {
                    "key": key,
                    "draft": { "text": "first", "attachments": [], "prompt": null }
                },
                {
                    "key": key,
                    "draft": { "text": "second", "attachments": [], "prompt": null }
                }
            ]
        });
        fs::write(
            store.path(),
            serde_json::to_vec_pretty(&encoded).expect("encode duplicate store"),
        )
        .expect("write duplicate store");

        assert!(store.load().is_empty());
    }
}
