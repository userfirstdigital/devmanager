//! Profile-scoped persistence for unsent task composer input.
//!
//! Drafts are client-local preferences, never host facts. The key includes the
//! exact Task and primary Agent identities so a restarted/rebound provider can
//! never inherit text that belonged to another conversation.

use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::domain::{AgentSessionId, TaskId};
use crate::ui::components::text_field::{MAX_TEXT_FIELD_BYTES, MAX_TEXT_FIELD_SCALARS};
use crate::ui::task_cockpit::composer::{
    ComposerDraftProjection, MAX_COMPOSER_ATTACHMENTS, MAX_PROMPT_ID_SCALARS,
};
use crate::ui::workspace_layout::write_atomically;

const DRAFT_SCHEMA_V1: &str = "devmanager.composer-drafts/v1";
/// Legacy raw-`TaskId` draft schema written by [`ComposerDraftStore::save`].
const DRAFT_SCHEMA: &str = DRAFT_SCHEMA_V1;
/// Host-qualified draft schema written by [`ComposerDraftStore::save_keyed`].
const DRAFT_SCHEMA_V2: &str = "devmanager.composer-drafts/v2";
const DRAFT_FILE_NAME: &str = "composer-drafts.json";
const MAX_DRAFT_FILE_BYTES: u64 = 2 * 1024 * 1024;
pub const MAX_PERSISTED_COMPOSER_DRAFTS: usize = 128;

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(bound(serialize = "K: Serialize", deserialize = "K: Deserialize<'de>"))]
pub struct KeyedComposerDraftKey<K = TaskId> {
    pub task_id: K,
    pub agent_session_id: AgentSessionId,
}

/// Local raw-`TaskId` draft key (existing public API). Remains `Copy`.
pub type ComposerDraftKey = KeyedComposerDraftKey<TaskId>;

impl Copy for KeyedComposerDraftKey<TaskId> {}

impl KeyedComposerDraftKey<TaskId> {
    pub const fn new(task_id: TaskId, agent_session_id: AgentSessionId) -> Self {
        Self {
            task_id,
            agent_session_id,
        }
    }
}

impl<K> KeyedComposerDraftKey<K> {
    pub fn from_parts(task_id: K, agent_session_id: AgentSessionId) -> Self {
        Self {
            task_id,
            agent_session_id,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(bound(serialize = "K: Serialize", deserialize = "K: Deserialize<'de>"))]
struct StoredComposerDraft<K = TaskId> {
    key: KeyedComposerDraftKey<K>,
    draft: ComposerDraftProjection,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(bound(serialize = "K: Serialize", deserialize = "K: Deserialize<'de>"))]
struct ComposerDraftFile<K = TaskId> {
    schema: String,
    drafts: Vec<StoredComposerDraft<K>>,
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
        let Some(bytes) = read_bounded(&self.path) else {
            return BTreeMap::new();
        };
        let Ok(file) = serde_json::from_slice::<ComposerDraftFile<TaskId>>(&bytes) else {
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

    /// Load host-qualified drafts.
    ///
    /// - `v1` maps each legacy raw task through the caller-supplied local owner.
    /// - `v2` deserializes full `K` directly and never reinterprets a foreign
    ///   owner as local.
    ///
    /// Duplicate mapped keys fail closed (empty map) without modifying disk.
    pub fn load_keyed<K, F>(
        &self,
        mut map_legacy_task: F,
    ) -> BTreeMap<KeyedComposerDraftKey<K>, ComposerDraftProjection>
    where
        K: Clone + Ord + Eq + Serialize + DeserializeOwned,
        F: FnMut(TaskId) -> K,
    {
        let Some(bytes) = read_bounded(&self.path) else {
            return BTreeMap::new();
        };
        let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
            return BTreeMap::new();
        };
        let Some(schema) = value.get("schema").and_then(|schema| schema.as_str()) else {
            return BTreeMap::new();
        };
        match schema {
            DRAFT_SCHEMA_V2 => {
                let Ok(file) = serde_json::from_value::<ComposerDraftFile<K>>(value) else {
                    return BTreeMap::new();
                };
                collect_drafts(file)
            }
            DRAFT_SCHEMA_V1 => {
                let Ok(file) = serde_json::from_value::<ComposerDraftFile<TaskId>>(value) else {
                    return BTreeMap::new();
                };
                if file.schema != DRAFT_SCHEMA_V1 && file.schema != DRAFT_SCHEMA {
                    return BTreeMap::new();
                }
                if file.drafts.len() > MAX_PERSISTED_COMPOSER_DRAFTS {
                    return BTreeMap::new();
                }
                let mut drafts = BTreeMap::new();
                for stored in file.drafts {
                    if !valid_draft(&stored.draft) {
                        return BTreeMap::new();
                    }
                    let key = KeyedComposerDraftKey::from_parts(
                        map_legacy_task(stored.key.task_id),
                        stored.key.agent_session_id,
                    );
                    if drafts.insert(key, stored.draft).is_some() {
                        return BTreeMap::new();
                    }
                }
                drafts
            }
            _ => BTreeMap::new(),
        }
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
        self.write_bounded(&bytes)
    }

    /// Persist host-qualified drafts as schema `v2`.
    pub fn save_keyed<K>(
        &self,
        drafts: &BTreeMap<KeyedComposerDraftKey<K>, ComposerDraftProjection>,
    ) -> io::Result<()>
    where
        K: Clone + Ord + Eq + Serialize,
    {
        if drafts.len() > MAX_PERSISTED_COMPOSER_DRAFTS
            || drafts.values().any(|draft| !valid_draft(draft))
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "composer draft preference is outside its bounded contract",
            ));
        }
        let file = ComposerDraftFile {
            schema: DRAFT_SCHEMA_V2.to_string(),
            drafts: drafts
                .iter()
                .map(|(key, draft)| StoredComposerDraft {
                    key: key.clone(),
                    draft: draft.clone(),
                })
                .collect(),
        };
        let bytes = serde_json::to_vec_pretty(&file)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        self.write_bounded(&bytes)
    }

    fn write_bounded(&self, bytes: &[u8]) -> io::Result<()> {
        if bytes.len() as u64 > MAX_DRAFT_FILE_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "composer drafts exceed their storage limit",
            ));
        }
        write_atomically(&self.path, bytes)
    }
}

fn collect_drafts<K: Clone + Ord + Eq>(
    file: ComposerDraftFile<K>,
) -> BTreeMap<KeyedComposerDraftKey<K>, ComposerDraftProjection> {
    if file.schema != DRAFT_SCHEMA_V2 || file.drafts.len() > MAX_PERSISTED_COMPOSER_DRAFTS {
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

fn read_bounded(path: &Path) -> Option<Vec<u8>> {
    let mut bytes = Vec::new();
    fs::File::open(path)
        .ok()?
        .take(MAX_DRAFT_FILE_BYTES + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    (bytes.len() as u64 <= MAX_DRAFT_FILE_BYTES).then_some(bytes)
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
    fn oversized_keyed_draft_file_preserves_previous_disk_content() {
        let directory = tempfile::tempdir().unwrap();
        let store = ComposerDraftStore::at_profile_root(directory.path());
        store.save(&BTreeMap::new()).unwrap();
        let before = fs::read(store.path()).unwrap();
        let drafts = BTreeMap::from([(
            KeyedComposerDraftKey::from_parts(
                "x".repeat(MAX_DRAFT_FILE_BYTES as usize),
                AgentSessionId::new(),
            ),
            ComposerDraftProjection {
                text: "retained".into(),
                attachments: Vec::new(),
                prompt: None,
            },
        )]);
        assert_eq!(
            store.save_keyed(&drafts).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        assert_eq!(fs::read(store.path()).unwrap(), before);
    }

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

    #[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
    struct TestOwnerKey {
        host: String,
        task: TaskId,
    }

    fn local_owner(task: TaskId) -> TestOwnerKey {
        TestOwnerKey {
            host: "local-profile".into(),
            task,
        }
    }

    #[test]
    fn keyed_drafts_keep_same_raw_task_independent_per_host() {
        let directory = tempfile::tempdir().expect("temp dir");
        let store = ComposerDraftStore::at_profile_root(directory.path());
        let shared = TaskId::new();
        let agent = AgentSessionId::new();
        let local = KeyedComposerDraftKey::from_parts(local_owner(shared), agent);
        let remote = KeyedComposerDraftKey::from_parts(
            TestOwnerKey {
                host: "remote-host".into(),
                task: shared,
            },
            agent,
        );
        let local_draft = ComposerDraftProjection {
            text: "local draft".into(),
            attachments: Vec::new(),
            prompt: None,
        };
        let remote_draft = ComposerDraftProjection {
            text: "remote draft".into(),
            attachments: Vec::new(),
            prompt: None,
        };
        store
            .save_keyed(&BTreeMap::from([
                (local.clone(), local_draft.clone()),
                (remote.clone(), remote_draft.clone()),
            ]))
            .expect("save keyed");

        let loaded = store.load_keyed(local_owner);
        assert_eq!(loaded.get(&local), Some(&local_draft));
        assert_eq!(loaded.get(&remote), Some(&remote_draft));
        let bytes = fs::read(store.path()).expect("read");
        let saved: serde_json::Value = serde_json::from_slice(&bytes).expect("parse");
        assert_eq!(saved["schema"], DRAFT_SCHEMA_V2);
    }

    #[test]
    fn draft_v1_migrates_with_explicit_local_owner() {
        let directory = tempfile::tempdir().expect("temp dir");
        let store = ComposerDraftStore::at_profile_root(directory.path());
        let task = TaskId::new();
        let agent = AgentSessionId::new();
        let key = ComposerDraftKey::new(task, agent);
        let draft = ComposerDraftProjection {
            text: "legacy".into(),
            attachments: Vec::new(),
            prompt: None,
        };
        store
            .save(&BTreeMap::from([(key, draft.clone())]))
            .expect("save v1");

        let loaded = store.load_keyed(local_owner);
        let expected = KeyedComposerDraftKey::from_parts(local_owner(task), agent);
        assert_eq!(loaded, BTreeMap::from([(expected, draft)]));
        let saved: serde_json::Value =
            serde_json::from_slice(&fs::read(store.path()).unwrap()).unwrap();
        assert_eq!(
            saved["schema"], DRAFT_SCHEMA_V1,
            "load_keyed must not rewrite legacy disk"
        );
    }

    #[test]
    fn draft_mapping_collision_fails_closed_without_modifying_disk() {
        let directory = tempfile::tempdir().expect("temp dir");
        let store = ComposerDraftStore::at_profile_root(directory.path());
        let first = ComposerDraftKey::new(TaskId::new(), AgentSessionId::new());
        let second = ComposerDraftKey::new(TaskId::new(), first.agent_session_id);
        let draft = ComposerDraftProjection {
            text: "text".into(),
            attachments: Vec::new(),
            prompt: None,
        };
        store
            .save(&BTreeMap::from([(first, draft.clone()), (second, draft)]))
            .expect("save");
        let before = fs::read(store.path()).expect("before");
        let collided = local_owner(TaskId::new());
        assert!(store.load_keyed(|_| collided.clone()).is_empty());
        assert_eq!(fs::read(store.path()).expect("after"), before);
    }

    #[test]
    fn corrupt_or_foreign_draft_v2_fails_closed() {
        let directory = tempfile::tempdir().expect("temp dir");
        let store = ComposerDraftStore::at_profile_root(directory.path());
        fs::write(store.path(), b"{not json").unwrap();
        assert!(store.load_keyed(local_owner).is_empty());

        let foreign = serde_json::json!({
            "schema": "devmanager.composer-drafts/v2",
            "drafts": [{
                "key": {
                    "task_id": { "host": 1, "task": "bad" },
                    "agent_session_id": AgentSessionId::new()
                },
                "draft": { "text": "x", "attachments": [], "prompt": null }
            }]
        });
        fs::write(store.path(), serde_json::to_vec_pretty(&foreign).unwrap()).unwrap();
        assert!(store.load_keyed(local_owner).is_empty());
    }
}
