//! Profile-scoped persistence for images attached to sent conversation turns.
//!
//! Providers replace the local image path with their own marker in the
//! canonical transcript. This small client-local index is therefore required
//! to keep the picture visible after the desktop app restarts.

use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::client::HostTaskKey;
use crate::domain::provider_input::{
    MAX_PROVIDER_IMAGE_ATTACHMENTS, MAX_PROVIDER_IMAGE_PATH_BYTES,
};
use crate::ui::components::text_field::MAX_TEXT_FIELD_BYTES;
use crate::ui::conversation::rows::SentMessageImages;
use crate::ui::workspace_layout::write_atomically;

const SCHEMA: &str = "devmanager.sent-message-images/v1";
const FILE_NAME: &str = "sent-message-images.json";
const MAX_FILE_BYTES: u64 = 2 * 1024 * 1024;
const MAX_TASKS: usize = 128;
const MAX_MESSAGES_PER_TASK: usize = 32;

#[derive(Debug, Serialize, Deserialize)]
struct SentImageFile {
    schema: String,
    tasks: Vec<StoredTaskImages>,
}

#[derive(Debug, Serialize, Deserialize)]
struct StoredTaskImages {
    task: HostTaskKey,
    messages: Vec<SentMessageImages>,
}

#[derive(Clone, Debug)]
pub struct SentImageStore {
    path: PathBuf,
}

impl SentImageStore {
    pub fn at_path(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn at_profile_root(root: impl AsRef<Path>) -> Self {
        Self::at_path(root.as_ref().join(FILE_NAME))
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn load(&self) -> BTreeMap<HostTaskKey, Vec<SentMessageImages>> {
        let Some(bytes) = read_bounded(&self.path) else {
            return BTreeMap::new();
        };
        let Ok(file) = serde_json::from_slice::<SentImageFile>(&bytes) else {
            return BTreeMap::new();
        };
        if file.schema != SCHEMA || file.tasks.len() > MAX_TASKS {
            return BTreeMap::new();
        }
        let mut tasks = BTreeMap::new();
        for task in file.tasks {
            if !valid_messages(&task.messages) || tasks.insert(task.task, task.messages).is_some() {
                return BTreeMap::new();
            }
        }
        tasks
    }

    pub fn save(&self, tasks: &BTreeMap<HostTaskKey, Vec<SentMessageImages>>) -> io::Result<()> {
        if tasks.len() > MAX_TASKS || tasks.values().any(|messages| !valid_messages(messages)) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "sent conversation images are outside their bounded contract",
            ));
        }
        let file = SentImageFile {
            schema: SCHEMA.to_string(),
            tasks: tasks
                .iter()
                .map(|(task, messages)| StoredTaskImages {
                    task: task.clone(),
                    messages: messages.clone(),
                })
                .collect(),
        };
        let bytes = serde_json::to_vec_pretty(&file)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        if bytes.len() as u64 > MAX_FILE_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "sent conversation images exceed their storage limit",
            ));
        }
        write_atomically(&self.path, &bytes)
    }
}

fn read_bounded(path: &Path) -> Option<Vec<u8>> {
    let mut bytes = Vec::new();
    fs::File::open(path)
        .ok()?
        .take(MAX_FILE_BYTES + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    (bytes.len() as u64 <= MAX_FILE_BYTES).then_some(bytes)
}

fn valid_messages(messages: &[SentMessageImages]) -> bool {
    messages.len() <= MAX_MESSAGES_PER_TASK && messages.iter().all(valid_message)
}

fn valid_message(message: &SentMessageImages) -> bool {
    !message.text.trim().is_empty()
        && message.text.len() <= MAX_TEXT_FIELD_BYTES
        && !message.paths.is_empty()
        && message.paths.len() <= MAX_PROVIDER_IMAGE_ATTACHMENTS
        && message.paths.iter().all(|path| {
            path.len() <= MAX_PROVIDER_IMAGE_PATH_BYTES
                && Path::new(path).is_absolute()
                && normalized_pasted_image_path(path)
        })
}

fn normalized_pasted_image_path(path: &str) -> bool {
    let normalized = path.replace('\\', "/");
    let lower = normalized.to_ascii_lowercase();
    !normalized
        .split('/')
        .any(|component| matches!(component, "." | ".."))
        && normalized.contains("/.devmanager/pasted-images/")
        && (lower.ends_with(".png") || lower.ends_with(".jpg") || lower.ends_with(".jpeg"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::HostId;
    use crate::domain::TaskId;

    fn key() -> HostTaskKey {
        HostTaskKey::new(HostId::local_profile("store-test").unwrap(), TaskId::new())
    }

    fn message(root: &Path) -> SentMessageImages {
        SentMessageImages {
            text: "what colour is this".into(),
            paths: vec![root
                .join(".devmanager/pasted-images/orange.png")
                .to_string_lossy()
                .into_owned()],
        }
    }

    #[test]
    fn sent_images_round_trip_across_a_new_store_instance() {
        let directory = tempfile::tempdir().unwrap();
        let store = SentImageStore::at_profile_root(directory.path());
        let expected = BTreeMap::from([(key(), vec![message(directory.path())])]);
        store.save(&expected).unwrap();

        assert_eq!(SentImageStore::at_path(store.path()).load(), expected);
    }

    #[test]
    fn corrupt_or_untrusted_image_records_fail_closed() {
        let directory = tempfile::tempdir().unwrap();
        let store = SentImageStore::at_profile_root(directory.path());
        let retained = BTreeMap::from([(key(), vec![message(directory.path())])]);
        store.save(&retained).unwrap();
        let before = fs::read(store.path()).unwrap();

        let invalid = BTreeMap::from([(
            key(),
            vec![SentMessageImages {
                text: "show secrets".into(),
                paths: vec![directory
                    .path()
                    .join("secret.png")
                    .to_string_lossy()
                    .into_owned()],
            }],
        )]);
        assert_eq!(
            store.save(&invalid).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        assert_eq!(fs::read(store.path()).unwrap(), before);

        let traversal = BTreeMap::from([(
            key(),
            vec![SentMessageImages {
                text: "show secrets".into(),
                paths: vec![directory
                    .path()
                    .join(".devmanager/pasted-images/../../secret.png")
                    .to_string_lossy()
                    .into_owned()],
            }],
        )]);
        assert_eq!(
            store.save(&traversal).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        assert_eq!(fs::read(store.path()).unwrap(), before);

        fs::write(store.path(), b"{not json").unwrap();
        assert!(store.load().is_empty());
    }
}
