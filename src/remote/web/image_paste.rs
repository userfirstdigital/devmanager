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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ComposerWriteOrigin {
    UserText,
    Generic,
}

pub(crate) fn handle_web_image_paste(
    process_manager: &ProcessManager,
    session_id: &str,
    attachment: &RemoteImageAttachment,
    authorize: impl FnOnce() -> bool,
) -> Result<(), String> {
    let runtime = process_manager.runtime_state();
    let session = runtime
        .sessions
        .get(session_id)
        .cloned()
        .ok_or_else(|| format!("Unknown terminal session: {session_id}"))?;
    execute_web_image_paste(&session, attachment, authorize, |reference| {
        process_manager.paste_user_text_to_session(session_id, reference)
    })
}

fn execute_web_image_paste(
    session: &SessionRuntimeState,
    attachment: &RemoteImageAttachment,
    authorize: impl FnOnce() -> bool,
    paste_user: impl FnOnce(&str) -> Result<(), String>,
) -> Result<(), String> {
    let staged = stage_web_image_for_session(session, attachment)?;
    if !authorize() {
        rollback_staged_images(std::slice::from_ref(&staged));
        return Err(WEB_COMPOSER_AUTHORITY_CHANGED.to_string());
    }
    let reference = format!("{} ", staged.prompt_reference);
    if let Err(error) = paste_user(&reference) {
        rollback_staged_images(std::slice::from_ref(&staged));
        return Err(error);
    }
    Ok(())
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
    execute_web_composer_batch(
        &session,
        attachments,
        text,
        authorize,
        |origin, prompt| match origin {
            ComposerWriteOrigin::UserText => {
                process_manager.write_user_text_to_session(session_id, prompt)
            }
            ComposerWriteOrigin::Generic => process_manager.write_to_session(session_id, prompt),
        },
    )
}

fn execute_web_composer_batch(
    session: &SessionRuntimeState,
    attachments: &[RemoteImageAttachment],
    text: &str,
    authorize: impl FnOnce() -> bool,
    mut write: impl FnMut(ComposerWriteOrigin, &str) -> Result<(), String>,
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
    let (text, submit) = match text.strip_suffix('\r') {
        Some(text) => (text, Some("\r")),
        None => (text, None),
    };
    let prompt = if references.is_empty() {
        text.to_string()
    } else {
        format!("{references} {text}")
    };
    let type_slash_command = submit.is_some()
        && session.session_kind.is_ai()
        && references.is_empty()
        && text.trim_start().starts_with('/');
    if !authorize() {
        rollback_staged_images(&staged);
        return Err(WEB_COMPOSER_AUTHORITY_CHANGED.to_string());
    }
    if submit.is_some() && matches!(session.session_kind, crate::state::SessionKind::Codex) {
        // Native mode can be restored while the provider TUI still owns a
        // model, status, or other full-screen interaction. Exit that screen
        // before writing the next prompt so the provider cannot discard it.
        if let Err(error) = write(ComposerWriteOrigin::Generic, "\u{1b}") {
            rollback_staged_images(&staged);
            return Err(error);
        }
        std::thread::sleep(Duration::from_millis(180));
    }
    let prompt_result = if type_slash_command {
        write_slash_command_prompt(&prompt, &mut write)
    } else {
        write(ComposerWriteOrigin::UserText, &prompt)
    };
    if let Err(error) = prompt_result {
        rollback_staged_images(&staged);
        return Err(error);
    }
    if let Some(submit) = submit {
        // TUI input parsers treat an Enter key as a distinct event. Sending it
        // in the same PTY write as pasted text can leave the prompt visibly
        // filled but never submitted (observed with Codex on Windows ConPTY).
        // Give cold provider autocomplete enough time to observe a typed slash
        // token. Ordinary prompts can use the short PTY settle interval without
        // adding visible latency.
        std::thread::sleep(ai_prompt_settle_delay(text));
        if type_slash_command {
            // A trailing separator closes autocomplete without choosing or
            // expanding a suggestion. Both providers ignore the whitespace
            // when executing, and the longer settle lets queued ConPTY writes
            // reach the TUI before Enter is delivered.
            write(ComposerWriteOrigin::Generic, " ")?;
            std::thread::sleep(Duration::from_millis(500));
        } else if matches!(session.session_kind, crate::state::SessionKind::Codex) {
            // Codex can keep pasted text in its multiline editor. Escape
            // returns it to the composer before Enter submits the prompt as a
            // distinct key event. Claude instead clears a composed prompt when
            // Escape is sent here, so it omits this post-text key entirely.
            write(ComposerWriteOrigin::Generic, "\u{1b}")?;
            std::thread::sleep(Duration::from_millis(120));
        }
        write(ComposerWriteOrigin::Generic, submit)?;
        if type_slash_command
            && matches!(session.session_kind, crate::state::SessionKind::Claude)
            && slash_command_has_no_arguments(text)
        {
            // Composer writes reach Claude as one queued PTY burst. The first
            // Enter accepts its autocomplete entry; a second distinct Enter
            // executes the now-complete exact command. Argument-bearing
            // commands already dismissed autocomplete and must submit once.
            std::thread::sleep(Duration::from_millis(180));
            write(ComposerWriteOrigin::Generic, submit)?;
        }
    }
    Ok(())
}

fn ai_prompt_settle_delay(text: &str) -> Duration {
    if text.trim_start().starts_with('/') {
        Duration::from_millis(250)
    } else {
        Duration::from_millis(50)
    }
}

fn slash_command_has_no_arguments(text: &str) -> bool {
    let trimmed = text.trim_start();
    let token_end = trimmed.find(char::is_whitespace).unwrap_or(trimmed.len());
    trimmed[token_end..].trim().is_empty()
}

fn write_slash_command_prompt(
    prompt: &str,
    write: &mut impl FnMut(ComposerWriteOrigin, &str) -> Result<(), String>,
) -> Result<(), String> {
    let trimmed = prompt.trim_start();
    let leading_len = prompt.len() - trimmed.len();
    if leading_len > 0 {
        write(ComposerWriteOrigin::Generic, &prompt[..leading_len])?;
    }
    let token_end = trimmed.find(char::is_whitespace).unwrap_or(trimmed.len());
    for (index, character) in trimmed[..token_end].chars().enumerate() {
        let mut encoded = [0_u8; 4];
        let origin = if index == 0 {
            ComposerWriteOrigin::UserText
        } else {
            ComposerWriteOrigin::Generic
        };
        write(origin, character.encode_utf8(&mut encoded))?;
        // Keep each command-token byte outside the PTY/channel coalescing
        // window so provider TUIs treat it as typing rather than a paste.
        std::thread::sleep(Duration::from_millis(100));
    }
    if token_end < trimmed.len() {
        write(ComposerWriteOrigin::Generic, &trimmed[token_end..])?;
    }
    Ok(())
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
    use crate::browser::{
        BrowserAttachmentBroker, BrowserPromptInput, BrowserWorkspaceKey, BrowserWorkspaceSnapshot,
    };
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
    fn image_paste_preserves_reservation_and_cleans_staging_until_user_write_succeeds() {
        let cwd = temp_test_dir("web-image-paste-transaction");
        let mut session = SessionRuntimeState::new(
            "claude-image",
            cwd.clone(),
            SessionDimensions::default(),
            TerminalBackend::default(),
        );
        session.session_kind = SessionKind::Claude;
        let attachment = RemoteImageAttachment {
            mime_type: "image/png".to_string(),
            file_name: Some("transaction.png".to_string()),
            bytes: vec![1, 2, 3],
        };
        let broker = BrowserAttachmentBroker::default();
        let workspace_key = BrowserWorkspaceKey::new("project", "conversation").unwrap();
        broker.observe_workspace(workspace_key.clone(), &attachment_snapshot("ann-image"));
        broker.bind_session("claude-image", workspace_key.clone());

        let writes = std::sync::atomic::AtomicUsize::new(0);
        let authority_error = execute_web_image_paste(
            &session,
            &attachment,
            || false,
            |_| {
                writes.fetch_add(1, Ordering::SeqCst);
                Ok(())
            },
        )
        .expect_err("lost authority rejects staged image");
        assert_eq!(authority_error, WEB_COMPOSER_AUTHORITY_CHANGED);
        assert_eq!(writes.load(Ordering::SeqCst), 0);
        assert_eq!(
            broker.projection(&workspace_key).pending_annotation_ids,
            ["ann-image"]
        );
        assert_staging_empty(&cwd);

        let write_error = execute_web_image_paste(
            &session,
            &attachment,
            || true,
            |reference| {
                let reservation = broker
                    .reserve_for_input("claude-image", BrowserPromptInput::Paste(reference))
                    .expect("image reference reserves pending annotation");
                assert!(reservation.preamble().contains("ann-image"));
                broker.rollback(reservation).unwrap();
                Err("fixture first user write failed".to_string())
            },
        )
        .expect_err("first user write failure is retryable");
        assert_eq!(write_error, "fixture first user write failed");
        assert_eq!(
            broker.projection(&workspace_key).pending_annotation_ids,
            ["ann-image"]
        );
        assert_staging_empty(&cwd);

        let kept_path = std::sync::Mutex::new(None);
        execute_web_image_paste(
            &session,
            &attachment,
            || true,
            |reference| {
                let reservation = broker
                    .reserve_for_input("claude-image", BrowserPromptInput::Paste(reference))
                    .expect("retry reserves pending annotation");
                assert!(reservation.preamble().contains("ann-image"));
                broker.commit(reservation).unwrap();
                *kept_path.lock().unwrap() =
                    Some(cwd.join(reference.trim().trim_start_matches('@')));
                Ok(())
            },
        )
        .expect("successful image reference write");
        assert!(broker
            .projection(&workspace_key)
            .pending_annotation_ids
            .is_empty());
        assert!(kept_path.lock().unwrap().as_ref().unwrap().is_file());
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
            |_, _| {
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
    fn claude_composer_batch_exits_provider_ui_without_clearing_the_prompt() {
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
        let observed_writes = std::sync::Mutex::new(Vec::new());

        execute_web_composer_batch(
            &session,
            &attachments,
            "hello\r",
            || true,
            |_, prompt| {
                observed_writes.lock().unwrap().push(prompt.to_string());
                Ok(())
            },
        )
        .expect("batch succeeds");

        let observed_writes = observed_writes.lock().unwrap();
        assert_eq!(observed_writes.len(), 2);
        assert_eq!(observed_writes[1], "\r");
        let prompt = &observed_writes[0];
        assert!(!prompt.ends_with('\r'));
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
    fn every_codex_composer_batch_exits_multiline_mode_before_submitting() {
        let cwd = temp_test_dir("web-composer-codex-submit");
        let mut session = SessionRuntimeState::new(
            "codex-batch",
            cwd,
            SessionDimensions::default(),
            TerminalBackend::default(),
        );
        session.session_kind = SessionKind::Codex;
        let observed_writes = std::sync::Mutex::new(Vec::new());

        for prompt in ["first prompt\r", "second prompt\r"] {
            execute_web_composer_batch(
                &session,
                &[],
                prompt,
                || true,
                |origin, text| {
                    observed_writes
                        .lock()
                        .unwrap()
                        .push((origin, text.to_string()));
                    Ok(())
                },
            )
            .expect("Codex batch succeeds");
        }

        assert_eq!(
            observed_writes
                .lock()
                .unwrap()
                .iter()
                .map(|(origin, text)| (*origin, text.as_str()))
                .collect::<Vec<_>>(),
            [
                (ComposerWriteOrigin::Generic, "\u{1b}"),
                (ComposerWriteOrigin::UserText, "first prompt"),
                (ComposerWriteOrigin::Generic, "\u{1b}"),
                (ComposerWriteOrigin::Generic, "\r"),
                (ComposerWriteOrigin::Generic, "\u{1b}"),
                (ComposerWriteOrigin::UserText, "second prompt"),
                (ComposerWriteOrigin::Generic, "\u{1b}"),
                (ComposerWriteOrigin::Generic, "\r"),
            ]
        );
    }

    #[test]
    fn codex_composer_draft_does_not_send_submit_keys() {
        let cwd = temp_test_dir("web-composer-codex-draft");
        let mut session = SessionRuntimeState::new(
            "codex-draft",
            cwd,
            SessionDimensions::default(),
            TerminalBackend::default(),
        );
        session.session_kind = SessionKind::Codex;
        let observed_writes = std::sync::Mutex::new(Vec::new());

        execute_web_composer_batch(
            &session,
            &[],
            "unfinished",
            || true,
            |_, text| {
                observed_writes.lock().unwrap().push(text.to_string());
                Ok(())
            },
        )
        .expect("draft write succeeds");

        assert_eq!(*observed_writes.lock().unwrap(), ["unfinished"]);
    }

    #[test]
    fn slash_commands_get_a_cold_provider_autocomplete_settle_window() {
        assert_eq!(ai_prompt_settle_delay("/model"), Duration::from_millis(250));
        assert_eq!(
            ai_prompt_settle_delay("  /status"),
            Duration::from_millis(250)
        );
        assert_eq!(
            ai_prompt_settle_delay("ordinary prompt"),
            Duration::from_millis(50)
        );
    }

    #[test]
    fn codex_slash_command_dismisses_autocomplete_with_trailing_space() {
        let cwd = temp_test_dir("web-composer-codex-slash");
        let mut session = SessionRuntimeState::new(
            "codex-slash",
            cwd,
            SessionDimensions::default(),
            TerminalBackend::default(),
        );
        session.session_kind = SessionKind::Codex;
        let observed_writes = std::sync::Mutex::new(Vec::new());

        execute_web_composer_batch(
            &session,
            &[],
            "/model\r",
            || true,
            |_, text| {
                observed_writes.lock().unwrap().push(text.to_string());
                Ok(())
            },
        )
        .expect("slash command succeeds");

        assert_eq!(
            *observed_writes.lock().unwrap(),
            ["\u{1b}", "/", "m", "o", "d", "e", "l", " ", "\r"]
        );
    }

    #[test]
    fn claude_unique_slash_command_dismisses_autocomplete_with_trailing_space() {
        let cwd = temp_test_dir("web-composer-claude-slash");
        let mut session = SessionRuntimeState::new(
            "claude-slash",
            cwd,
            SessionDimensions::default(),
            TerminalBackend::default(),
        );
        session.session_kind = SessionKind::Claude;
        let observed_writes = std::sync::Mutex::new(Vec::new());

        execute_web_composer_batch(
            &session,
            &[],
            "/model\r",
            || true,
            |_, text| {
                observed_writes.lock().unwrap().push(text.to_string());
                Ok(())
            },
        )
        .expect("slash command succeeds");

        assert_eq!(
            *observed_writes.lock().unwrap(),
            ["/", "m", "o", "d", "e", "l", " ", "\r", "\r"]
        );
    }

    #[test]
    fn claude_prefix_colliding_slash_command_dismisses_autocomplete_with_trailing_space() {
        let cwd = temp_test_dir("web-composer-claude-status");
        let mut session = SessionRuntimeState::new(
            "claude-status",
            cwd,
            SessionDimensions::default(),
            TerminalBackend::default(),
        );
        session.session_kind = SessionKind::Claude;
        let observed_writes = std::sync::Mutex::new(Vec::new());

        execute_web_composer_batch(
            &session,
            &[],
            "/status\r",
            || true,
            |_, text| {
                observed_writes.lock().unwrap().push(text.to_string());
                Ok(())
            },
        )
        .expect("slash command succeeds");

        assert_eq!(
            *observed_writes.lock().unwrap(),
            ["/", "s", "t", "a", "t", "u", "s", " ", "\r", "\r"]
        );
    }

    #[test]
    fn claude_slash_command_with_arguments_gets_a_trailing_separator() {
        let cwd = temp_test_dir("web-composer-claude-arguments");
        let mut session = SessionRuntimeState::new(
            "claude-arguments",
            cwd,
            SessionDimensions::default(),
            TerminalBackend::default(),
        );
        session.session_kind = SessionKind::Claude;
        let observed_writes = std::sync::Mutex::new(Vec::new());

        execute_web_composer_batch(
            &session,
            &[],
            "/model opus\r",
            || true,
            |_, text| {
                observed_writes.lock().unwrap().push(text.to_string());
                Ok(())
            },
        )
        .expect("slash command succeeds");

        assert_eq!(
            *observed_writes.lock().unwrap(),
            ["/", "m", "o", "d", "e", "l", " opus", " ", "\r"]
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
            |_, _| {
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

    #[test]
    fn slash_composer_consumes_on_first_slash_and_preserves_a_concurrent_annotation() {
        let cwd = temp_test_dir("web-composer-slash-origin");
        let mut session = SessionRuntimeState::new(
            "claude-slash-origin",
            cwd,
            SessionDimensions::default(),
            TerminalBackend::default(),
        );
        session.session_kind = SessionKind::Claude;
        let broker = BrowserAttachmentBroker::default();
        let workspace_key = BrowserWorkspaceKey::new("project", "conversation").unwrap();
        broker.observe_workspace(workspace_key.clone(), &attachment_snapshot("ann-first"));
        broker.bind_session("claude-slash-origin", workspace_key.clone());
        let observed = std::sync::Mutex::new(Vec::new());

        execute_web_composer_batch(
            &session,
            &[],
            "  /model\r",
            || true,
            |origin, text| {
                let mut prefix = String::new();
                if origin == ComposerWriteOrigin::UserText {
                    let reservation = broker
                        .reserve_for_input("claude-slash-origin", BrowserPromptInput::Text(text))
                        .expect("first slash reserves current pending annotation");
                    prefix = reservation.preamble().to_string();
                    broker.observe_workspace(
                        workspace_key.clone(),
                        &attachment_snapshot("ann-later"),
                    );
                    broker.commit(reservation).unwrap();
                }
                observed
                    .lock()
                    .unwrap()
                    .push((origin, text.to_string(), prefix));
                Ok(())
            },
        )
        .expect("slash command writes");

        let observed = observed.lock().unwrap();
        let user_writes = observed
            .iter()
            .filter(|(origin, _, _)| *origin == ComposerWriteOrigin::UserText)
            .collect::<Vec<_>>();
        assert_eq!(user_writes.len(), 1);
        assert_eq!(user_writes[0].1, "/");
        assert!(user_writes[0].2.contains("ann-first"));
        assert!(observed
            .iter()
            .any(|(origin, text, _)| { *origin == ComposerWriteOrigin::Generic && text == "  " }));
        assert!(observed
            .iter()
            .filter(|(origin, _, _)| *origin == ComposerWriteOrigin::Generic)
            .all(|(_, _, prefix)| prefix.is_empty()));
        assert_eq!(
            broker.projection(&workspace_key).pending_annotation_ids,
            ["ann-later"]
        );
    }

    #[test]
    fn ordinary_composer_retries_first_user_write_but_keeps_commit_after_enter_failure() {
        let cwd = temp_test_dir("web-composer-ordinary-transaction");
        let mut session = SessionRuntimeState::new(
            "claude-ordinary-origin",
            cwd.clone(),
            SessionDimensions::default(),
            TerminalBackend::default(),
        );
        session.session_kind = SessionKind::Claude;
        let attachments = [RemoteImageAttachment {
            mime_type: "image/png".to_string(),
            file_name: Some("ordinary.png".to_string()),
            bytes: vec![1, 2, 3],
        }];
        let broker = BrowserAttachmentBroker::default();
        let workspace_key = BrowserWorkspaceKey::new("project", "ordinary").unwrap();
        broker.observe_workspace(workspace_key.clone(), &attachment_snapshot("ann-ordinary"));
        broker.bind_session("claude-ordinary-origin", workspace_key.clone());

        let first_error = execute_web_composer_batch(
            &session,
            &attachments,
            "first try\r",
            || true,
            |origin, text| {
                assert_eq!(origin, ComposerWriteOrigin::UserText);
                let reservation = broker
                    .reserve_for_input("claude-ordinary-origin", BrowserPromptInput::Text(text))
                    .expect("first prompt reserves annotation");
                broker.rollback(reservation).unwrap();
                Err("fixture first prompt write failed".to_string())
            },
        )
        .expect_err("first user write fails");
        assert_eq!(first_error, "fixture first prompt write failed");
        assert_eq!(
            broker.projection(&workspace_key).pending_annotation_ids,
            ["ann-ordinary"]
        );
        assert_staging_empty(&cwd);

        let kept_path = std::sync::Mutex::new(None);
        let later_error = execute_web_composer_batch(
            &session,
            &attachments,
            "retry\r",
            || true,
            |origin, text| match origin {
                ComposerWriteOrigin::UserText => {
                    let reservation = broker
                        .reserve_for_input("claude-ordinary-origin", BrowserPromptInput::Text(text))
                        .expect("new submission retries annotation reservation");
                    assert!(reservation.preamble().contains("ann-ordinary"));
                    let reference = text
                        .split_whitespace()
                        .find(|part| part.starts_with('@'))
                        .expect("staged attachment reference");
                    *kept_path.lock().unwrap() = Some(cwd.join(reference.trim_start_matches('@')));
                    broker.commit(reservation).unwrap();
                    Ok(())
                }
                ComposerWriteOrigin::Generic if text == "\r" => {
                    Err("fixture later Enter failed".to_string())
                }
                ComposerWriteOrigin::Generic => Ok(()),
            },
        )
        .expect_err("later Enter failure surfaces");
        assert_eq!(later_error, "fixture later Enter failed");
        assert!(broker
            .projection(&workspace_key)
            .pending_annotation_ids
            .is_empty());
        assert!(kept_path.lock().unwrap().as_ref().unwrap().is_file());
    }

    fn temp_test_dir(label: &str) -> PathBuf {
        let path =
            std::env::temp_dir().join(format!("devmanager-tests-{label}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        path
    }

    fn attachment_snapshot(annotation_id: &str) -> BrowserWorkspaceSnapshot {
        serde_json::from_value(serde_json::json!({
            "annotations": [{
                "id": annotation_id,
                "kind": "element",
                "tabId": "page",
                "anchorRevision": 1,
                "comment": "Review image",
                "url": "https://example.test/page",
                "locator": {},
                "bounds": { "x": 1, "y": 2, "width": 30, "height": 40 },
                "viewport": {},
                "screenshotResource": "shot-image",
                "computedStyles": {},
                "resolved": false
            }],
            "pendingAnnotationRevision": 1,
            "pendingAnnotationIds": [annotation_id]
        }))
        .expect("valid attachment snapshot")
    }

    fn assert_staging_empty(cwd: &Path) {
        let staging = cwd.join(".devmanager").join("pasted-images");
        assert!(
            !staging.exists() || fs::read_dir(staging).unwrap().next().is_none(),
            "failed image transaction left staged files behind"
        );
    }
}
