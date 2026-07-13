use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::remote::RemoteImageAttachment;
use crate::services::ProcessManager;
use crate::state::SessionRuntimeState;

pub(crate) const WEB_PASTE_IMAGE_MAX_BYTES: usize = 5 * 1024 * 1024;
pub(crate) const WEB_COMPOSER_AUTHORITY_CHANGED: &str =
    "The writer lease changed before the prompt reached the terminal.";
const STAGING_DIR: [&str; 2] = [".devmanager", "pasted-images"];
const STAGED_IMAGE_TTL: Duration = Duration::from_secs(60 * 60 * 24);
static NEXT_STAGED_IMAGE_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StagedImageAttachment {
    pub path: PathBuf,
    pub prompt_reference: String,
}

pub(crate) fn handle_web_image_paste(
    process_manager: &ProcessManager,
    session_id: &str,
    attachment: &RemoteImageAttachment,
) -> Result<(), String> {
    let runtime = process_manager.runtime_state();
    let session = runtime
        .sessions
        .get(session_id)
        .cloned()
        .ok_or_else(|| format!("Unknown terminal session: {session_id}"))?;
    let staged = stage_web_image_for_session(&session, attachment)?;
    process_manager.paste_to_session(session_id, &format!("{} ", staged.prompt_reference))
}

pub(crate) fn handle_web_composer_batch(
    process_manager: &ProcessManager,
    session_id: &str,
    attachments: &[RemoteImageAttachment],
    text: &str,
    authorize: impl FnOnce() -> bool,
) -> Result<(), String> {
    let runtime = process_manager.runtime_state();
    let session = runtime
        .sessions
        .get(session_id)
        .cloned()
        .ok_or_else(|| format!("Unknown terminal session: {session_id}"))?;
    execute_web_composer_batch(&session, attachments, text, authorize, |prompt| {
        process_manager.write_to_session(session_id, prompt)
    })
}

fn execute_web_composer_batch(
    session: &SessionRuntimeState,
    attachments: &[RemoteImageAttachment],
    text: &str,
    authorize: impl FnOnce() -> bool,
    write: impl FnOnce(&str) -> Result<(), String>,
) -> Result<(), String> {
    let mut staged = Vec::with_capacity(attachments.len());
    for attachment in attachments {
        match stage_web_image_for_session(session, attachment) {
            Ok(attachment) => staged.push(attachment),
            Err(error) => {
                rollback_staged_images(&staged);
                return Err(error);
            }
        }
    }
    let references = staged
        .iter()
        .map(|attachment| attachment.prompt_reference.as_str())
        .collect::<Vec<_>>()
        .join(" ");
    let prompt = if references.is_empty() {
        text.to_string()
    } else {
        format!("{references} {text}")
    };
    if !authorize() {
        rollback_staged_images(&staged);
        return Err(WEB_COMPOSER_AUTHORITY_CHANGED.to_string());
    }
    match write(&prompt) {
        Ok(()) => Ok(()),
        Err(error) => {
            rollback_staged_images(&staged);
            Err(error)
        }
    }
}

fn rollback_staged_images(staged: &[StagedImageAttachment]) {
    for attachment in staged {
        let _ = fs::remove_file(&attachment.path);
    }
}

pub(crate) fn stage_web_image_for_session(
    session: &SessionRuntimeState,
    attachment: &RemoteImageAttachment,
) -> Result<StagedImageAttachment, String> {
    if !session.session_kind.is_ai() {
        return Err("Image paste is only supported in Claude and Codex terminals.".to_string());
    }

    let extension = validate_image_attachment(attachment)?;
    let staging_dir = staging_dir_for_session(&session.cwd);
    let _ = cleanup_staged_images(&staging_dir);
    fs::create_dir_all(&staging_dir)
        .map_err(|error| format!("Failed to prepare pasted image staging: {error}"))?;

    let file_stem = sanitize_file_stem(attachment.file_name.as_deref());
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let ordinal = NEXT_STAGED_IMAGE_ID.fetch_add(1, Ordering::Relaxed);
    let file_name = format!("{file_stem}-{unique}-{ordinal}.{extension}");
    let path = staging_dir.join(file_name);
    if let Err(error) = fs::write(&path, &attachment.bytes) {
        let _ = fs::remove_file(&path);
        return Err(format!("Failed to save pasted image on the host: {error}"));
    }

    Ok(StagedImageAttachment {
        prompt_reference: format!("@{}", prompt_path(&path, &session.cwd)),
        path,
    })
}

fn validate_image_attachment(attachment: &RemoteImageAttachment) -> Result<&'static str, String> {
    if attachment.bytes.is_empty() {
        return Err("Pasted image is empty.".to_string());
    }
    if attachment.bytes.len() > WEB_PASTE_IMAGE_MAX_BYTES {
        return Err("Pasted image is too large. Max size is 5 MiB.".to_string());
    }
    match attachment.mime_type.as_str() {
        "image/png" => Ok("png"),
        "image/jpeg" => Ok("jpg"),
        _ => Err("Unsupported pasted image type. Try PNG or JPEG.".to_string()),
    }
}

fn staging_dir_for_session(cwd: &Path) -> PathBuf {
    if cwd.is_dir() {
        cwd.join(STAGING_DIR[0]).join(STAGING_DIR[1])
    } else {
        std::env::temp_dir().join("devmanager").join(STAGING_DIR[1])
    }
}

fn sanitize_file_stem(file_name: Option<&str>) -> String {
    let raw_stem = file_name
        .and_then(|name| Path::new(name).file_stem())
        .and_then(|stem| stem.to_str())
        .unwrap_or("clipboard-image");
    let cleaned = raw_stem
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() {
                ch.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect::<String>()
        .trim_matches('-')
        .to_string();
    if cleaned.is_empty() {
        "clipboard-image".to_string()
    } else {
        cleaned
    }
}

fn prompt_path(path: &Path, cwd: &Path) -> String {
    let relative_or_absolute = path.strip_prefix(cwd).unwrap_or(path);
    relative_or_absolute.to_string_lossy().replace('\\', "/")
}

fn cleanup_staged_images(dir: &Path) -> Result<(), String> {
    let now = SystemTime::now();
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(_) => return Ok(()),
    };
    for entry in entries.flatten() {
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        if !metadata.is_file() {
            continue;
        }
        let Ok(modified_at) = metadata.modified() else {
            continue;
        };
        let Ok(age) = now.duration_since(modified_at) else {
            continue;
        };
        if age >= STAGED_IMAGE_TTL {
            let _ = fs::remove_file(entry.path());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{SessionDimensions, SessionKind};
    use crate::terminal::session::TerminalBackend;

    #[test]
    fn stage_web_image_for_ai_session_writes_hidden_workspace_file() {
        let cwd = temp_test_dir("web-image-paste-ai");
        let mut session = SessionRuntimeState::new(
            "claude-1",
            cwd.clone(),
            SessionDimensions::default(),
            TerminalBackend::default(),
        );
        session.session_kind = SessionKind::Claude;
        let attachment = RemoteImageAttachment {
            mime_type: "image/png".to_string(),
            file_name: Some("Screen Shot.png".to_string()),
            bytes: vec![1, 2, 3],
        };

        let staged = stage_web_image_for_session(&session, &attachment).expect("stage image");

        assert!(staged
            .path
            .starts_with(cwd.join(".devmanager").join("pasted-images")));
        assert_eq!(fs::read(&staged.path).expect("saved bytes"), vec![1, 2, 3]);
        assert!(staged
            .prompt_reference
            .starts_with("@.devmanager/pasted-images/"));
        assert!(staged.prompt_reference.ends_with(".png"));
    }

    #[test]
    fn stage_web_image_for_non_ai_session_rejects() {
        let cwd = temp_test_dir("web-image-paste-server");
        let mut session = SessionRuntimeState::new(
            "server-1",
            cwd,
            SessionDimensions::default(),
            TerminalBackend::default(),
        );
        session.session_kind = SessionKind::Server;
        let attachment = RemoteImageAttachment {
            mime_type: "image/png".to_string(),
            file_name: Some("clip.png".to_string()),
            bytes: vec![1, 2, 3],
        };

        let error = stage_web_image_for_session(&session, &attachment).unwrap_err();

        assert_eq!(
            error,
            "Image paste is only supported in Claude and Codex terminals."
        );
    }

    #[test]
    fn composer_batch_rolls_back_when_second_attachment_fails_before_any_pty_write() {
        let cwd = temp_test_dir("web-composer-batch-rollback");
        let mut session = SessionRuntimeState::new(
            "claude-batch",
            cwd.clone(),
            SessionDimensions::default(),
            TerminalBackend::default(),
        );
        session.session_kind = SessionKind::Claude;
        let attachments = vec![
            RemoteImageAttachment {
                mime_type: "image/png".to_string(),
                file_name: Some("first.png".to_string()),
                bytes: vec![1, 2, 3],
            },
            RemoteImageAttachment {
                mime_type: "image/gif".to_string(),
                file_name: Some("second.gif".to_string()),
                bytes: vec![4, 5, 6],
            },
        ];
        let writes = std::sync::atomic::AtomicUsize::new(0);

        let result = execute_web_composer_batch(
            &session,
            &attachments,
            "hello\r",
            || true,
            |_| {
                writes.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(())
            },
        );

        assert!(result.is_err());
        assert_eq!(writes.load(std::sync::atomic::Ordering::SeqCst), 0);
        let staging = cwd.join(".devmanager").join("pasted-images");
        assert!(
            !staging.exists() || fs::read_dir(staging).unwrap().next().is_none(),
            "a rejected batch left staged files behind"
        );
    }

    #[test]
    fn composer_batch_stages_distinct_files_and_writes_the_pty_once() {
        let cwd = temp_test_dir("web-composer-batch-success");
        let mut session = SessionRuntimeState::new(
            "claude-batch",
            cwd.clone(),
            SessionDimensions::default(),
            TerminalBackend::default(),
        );
        session.session_kind = SessionKind::Claude;
        let attachments = vec![
            RemoteImageAttachment {
                mime_type: "image/png".to_string(),
                file_name: Some("clipboard.png".to_string()),
                bytes: vec![1, 2, 3],
            },
            RemoteImageAttachment {
                mime_type: "image/png".to_string(),
                file_name: Some("clipboard.png".to_string()),
                bytes: vec![4, 5, 6],
            },
        ];
        let writes = std::sync::atomic::AtomicUsize::new(0);
        let observed_prompt = std::sync::Mutex::new(None);

        execute_web_composer_batch(
            &session,
            &attachments,
            "hello\r",
            || true,
            |prompt| {
                writes.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                *observed_prompt.lock().unwrap() = Some(prompt.to_string());
                Ok(())
            },
        )
        .expect("batch succeeds");

        assert_eq!(writes.load(std::sync::atomic::Ordering::SeqCst), 1);
        let prompt = observed_prompt.lock().unwrap().clone().unwrap();
        let references = prompt
            .split_whitespace()
            .filter(|part| part.starts_with('@'))
            .collect::<Vec<_>>();
        assert_eq!(references.len(), 2);
        assert_ne!(references[0], references[1]);
        assert_eq!(
            fs::read(cwd.join(references[0].trim_start_matches('@'))).unwrap(),
            vec![1, 2, 3]
        );
        assert_eq!(
            fs::read(cwd.join(references[1].trim_start_matches('@'))).unwrap(),
            vec![4, 5, 6]
        );
    }

    #[test]
    fn composer_batch_revalidates_authority_after_staging_and_rolls_back() {
        let cwd = temp_test_dir("web-composer-authority-rollback");
        let mut session = SessionRuntimeState::new(
            "claude-authority",
            cwd.clone(),
            SessionDimensions::default(),
            TerminalBackend::default(),
        );
        session.session_kind = SessionKind::Claude;
        let attachments = vec![RemoteImageAttachment {
            mime_type: "image/png".to_string(),
            file_name: Some("authority.png".to_string()),
            bytes: vec![1, 2, 3],
        }];
        let writes = std::sync::atomic::AtomicUsize::new(0);

        let result = execute_web_composer_batch(
            &session,
            &attachments,
            "hello\r",
            || false,
            |_| {
                writes.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(())
            },
        );

        assert_eq!(result.unwrap_err(), WEB_COMPOSER_AUTHORITY_CHANGED);
        assert_eq!(writes.load(std::sync::atomic::Ordering::SeqCst), 0);
        let staging = cwd.join(".devmanager").join("pasted-images");
        assert!(
            !staging.exists() || fs::read_dir(staging).unwrap().next().is_none(),
            "authority loss left staged files behind"
        );
    }

    fn temp_test_dir(label: &str) -> PathBuf {
        let path =
            std::env::temp_dir().join(format!("devmanager-tests-{label}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        path
    }
}
