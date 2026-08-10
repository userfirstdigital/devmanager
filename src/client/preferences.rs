//! Isolated client-local preferences for the native-next client.
//!
//! The host owns task truth and `session.json` owns legacy window/tab state.
//! This store is deliberately a separate file so client presentation state can
//! be restored without making the legacy desktop session a second task
//! authority.  Callers must provide the profile-specific root; there is no
//! implicit production path or environment fallback.

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

const PREFERENCES_SCHEMA: &str = "devmanager.client-preferences/v1";
const PREFERENCES_FILE_NAME: &str = "inbox-preferences.json";
const MAX_CURSOR_BYTES: usize = 1_048_576;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClientPreferenceError {
    Io(String),
    Encode(String),
    Decode(String),
    UnsupportedSchema(String),
    CursorTooLarge(usize),
}

impl std::fmt::Display for ClientPreferenceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(message) => write!(f, "client preference I/O failed: {message}"),
            Self::Encode(message) => write!(f, "client preference encode failed: {message}"),
            Self::Decode(message) => write!(f, "client preference decode failed: {message}"),
            Self::UnsupportedSchema(schema) => {
                write!(f, "unsupported client preference schema: {schema}")
            }
            Self::CursorTooLarge(bytes) => {
                write!(f, "inbox cursor exceeds {MAX_CURSOR_BYTES} bytes: {bytes}")
            }
        }
    }
}

impl std::error::Error for ClientPreferenceError {}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PreferencesFile {
    schema: String,
    #[serde(default)]
    inbox_unread_cursor: Option<Vec<u8>>,
}

/// The only durable client-local file currently owned by the Inbox surface.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InboxPreferenceStore {
    path: PathBuf,
}

impl InboxPreferenceStore {
    /// Construct a store at an explicit file path.  Native-next passes its
    /// isolated profile root; tests pass a temporary directory.  The method
    /// intentionally does not inspect `DEVMANAGER_PROFILE` or app globals.
    pub fn at_path(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// Construct the dedicated preference path beneath an already-resolved
    /// client profile root.  This does not resolve or touch the root itself.
    pub fn at_profile_root(root: impl AsRef<Path>) -> Self {
        Self::at_path(root.as_ref().join(PREFERENCES_FILE_NAME))
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Missing files and missing fields are the backwards-compatible default.
    pub fn load_unread_cursor(&self) -> Result<Option<Vec<u8>>, ClientPreferenceError> {
        let bytes = match fs::read(&self.path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(ClientPreferenceError::Io(error.to_string())),
        };
        let file: PreferencesFile = serde_json::from_slice(&bytes)
            .map_err(|error| ClientPreferenceError::Decode(error.to_string()))?;
        if file.schema != PREFERENCES_SCHEMA {
            return Err(ClientPreferenceError::UnsupportedSchema(file.schema));
        }
        if let Some(cursor) = &file.inbox_unread_cursor {
            if cursor.len() > MAX_CURSOR_BYTES {
                return Err(ClientPreferenceError::CursorTooLarge(cursor.len()));
            }
        }
        Ok(file.inbox_unread_cursor)
    }

    /// Encode and publish the complete preference file through one atomic
    /// replace.  A null cursor is a valid reset and is never represented by a
    /// partially written file.
    pub fn save_unread_cursor(&self, cursor: Option<&[u8]>) -> Result<(), ClientPreferenceError> {
        if let Some(cursor) = cursor {
            if cursor.len() > MAX_CURSOR_BYTES {
                return Err(ClientPreferenceError::CursorTooLarge(cursor.len()));
            }
        }
        let file = PreferencesFile {
            schema: PREFERENCES_SCHEMA.to_string(),
            inbox_unread_cursor: cursor.map(<[u8]>::to_vec),
        };
        let bytes = serde_json::to_vec(&file)
            .map_err(|error| ClientPreferenceError::Encode(error.to_string()))?;
        write_atomically(&self.path, &bytes)
            .map_err(|error| ClientPreferenceError::Io(format!("{}: {error}", self.path.display())))
    }
}

fn write_atomically(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("preferences.json");
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    let pid = std::process::id();
    let mut temporary = None;
    for attempt in 0..10_000u32 {
        let suffix = if attempt == 0 {
            format!("{file_name}.devmanager-tmp-{pid}-{stamp}")
        } else {
            format!("{file_name}.devmanager-tmp-{pid}-{stamp}-{attempt}")
        };
        let candidate = parent.join(suffix);
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate)
        {
            Ok(file) => {
                temporary = Some((candidate, file));
                break;
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
    }
    let Some((temporary, mut file)) = temporary else {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "exhausted temporary preference file names",
        ));
    };
    let result = (|| {
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        replace_file(&temporary, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

#[cfg(windows)]
fn replace_file(temporary: &Path, destination: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows::core::PCWSTR;
    use windows::Win32::Storage::FileSystem::{
        MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
    };

    let temporary: Vec<u16> = temporary.as_os_str().encode_wide().chain(Some(0)).collect();
    let destination: Vec<u16> = destination
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect();
    unsafe {
        MoveFileExW(
            PCWSTR(temporary.as_ptr()),
            PCWSTR(destination.as_ptr()),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
        .map_err(io::Error::from)
    }
}

#[cfg(not(windows))]
fn replace_file(temporary: &Path, destination: &Path) -> io::Result<()> {
    fs::rename(temporary, destination)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_and_legacy_preference_files_restore_to_default() {
        let directory = tempfile::tempdir().expect("temp directory");
        let store = InboxPreferenceStore::at_profile_root(directory.path());
        assert_eq!(store.load_unread_cursor().expect("missing is valid"), None);
        fs::write(
            store.path(),
            br#"{"schema":"devmanager.client-preferences/v1"}"#,
        )
        .expect("legacy preference file");
        assert_eq!(
            store.load_unread_cursor().expect("missing field is valid"),
            None
        );
    }

    #[test]
    fn cursor_round_trip_replaces_one_complete_file() {
        let directory = tempfile::tempdir().expect("temp directory");
        let store = InboxPreferenceStore::at_profile_root(directory.path());
        store
            .save_unread_cursor(Some(&[1, 2, 3]))
            .expect("save cursor");
        assert_eq!(
            store.load_unread_cursor().expect("load cursor"),
            Some(vec![1, 2, 3])
        );
        store.save_unread_cursor(None).expect("clear cursor");
        assert_eq!(store.load_unread_cursor().expect("load clear"), None);
        assert!(fs::read_dir(directory.path())
            .expect("read preference directory")
            .all(|entry| !entry
                .expect("directory entry")
                .file_name()
                .to_string_lossy()
                .contains("devmanager-tmp")));
    }

    #[test]
    fn invalid_schema_and_oversized_cursor_fail_closed() {
        let directory = tempfile::tempdir().expect("temp directory");
        let store = InboxPreferenceStore::at_profile_root(directory.path());
        fs::write(
            store.path(),
            br#"{"schema":"devmanager.client-preferences/v2","inboxUnreadCursor":null}"#,
        )
        .expect("unsupported schema file");
        assert!(matches!(
            store.load_unread_cursor(),
            Err(ClientPreferenceError::UnsupportedSchema(schema)) if schema.ends_with("/v2")
        ));
        assert!(matches!(
            store.save_unread_cursor(Some(&vec![0u8; MAX_CURSOR_BYTES + 1])),
            Err(ClientPreferenceError::CursorTooLarge(bytes)) if bytes == MAX_CURSOR_BYTES + 1
        ));
    }
}
