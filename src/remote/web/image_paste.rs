use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::remote::RemoteImageAttachment;
use crate::services::ProcessManager;
use crate::state::SessionRuntimeState;

pub(crate) const WEB_PASTE_IMAGE_MAX_BYTES: usize = 5 * 1024 * 1024;
const STAGING_DIR: [&str; 2] = [".devmanager", "pasted-images"];
const STAGED_IMAGE_TTL: Duration = Duration::from_secs(60 * 60 * 24);

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
    let file_name = format!("{file_stem}-{unique}.{extension}");
    let path = staging_dir.join(file_name);
    fs::write(&path, &attachment.bytes)
        .map_err(|error| format!("Failed to save pasted image on the host: {error}"))?;

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

    fn temp_test_dir(label: &str) -> PathBuf {
        let path =
            std::env::temp_dir().join(format!("devmanager-tests-{label}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        path
    }
}
