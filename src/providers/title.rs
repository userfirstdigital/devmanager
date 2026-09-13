//! One short model call that names a task.
//!
//! A task used to be titled with the first words of the first message, which
//! reads as a truncated paragraph rather than a name. This asks a small model
//! for a title instead.
//!
//! # A deliberate exception to the `exec` ban
//!
//! [`crate::providers::codex`] forbids `exec` (and `app-server`, `--last`,
//! `--remote`) on the launch path, and `reject_forbidden_launch` enforces it.
//! That rule is about *sessions*: a task's provider must be an interactive
//! process this app owns, supervises and can stop -- never a detached
//! non-interactive run pretending to be one.
//!
//! This module is not a session. It is one bounded, read-only question whose
//! only output is a handful of words that become a title, and it never touches
//! a task's provider process or its journal. The exception is kept narrow on
//! purpose:
//!
//! * exactly one prompt shape, built here, never caller-supplied;
//! * `--sandbox read-only`, so the run cannot write to the project;
//! * `--ephemeral`, so it leaves no session behind to be resumed;
//! * stdin closed, a hard deadline, and a bounded read of stdout;
//! * the result is treated as untrusted text: one line, length-capped,
//!   control characters refused.
//!
//! Anything beyond a title belongs on the launch path, under its ban.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

/// The model that names tasks: small, cheap and quick. Titling must never cost
/// what the work itself costs.
pub const TITLE_MODEL_SLUG: &str = "gpt-5.6-luna";
/// Low effort: this is a naming task, not a reasoning one.
pub const TITLE_REASONING_EFFORT: &str = "low";
/// Past this the title is not worth waiting for; the caller keeps what it has.
pub const TITLE_DEADLINE: Duration = Duration::from_secs(45);
/// A title is a few words. Anything longer is a model that misunderstood.
pub const TITLE_MAX_CHARS: usize = 64;
/// Enough stdout to find the answer, not enough for a runaway to matter.
const TITLE_MAX_OUTPUT_BYTES: usize = 64 * 1024;

/// What the model is asked. One shape, built here, so no caller can turn this
/// seam into a general way to run a provider.
pub fn title_prompt(seed: &str) -> String {
    let seed = bounded_seed(seed);
    format!(
        "Write a short title for this task, at most 6 words, in sentence case. \
         Reply with the title only: no quotes, no trailing punctuation, no \
         explanation.\n\nTask:\n{seed}"
    )
}

/// The first message can be enormous; the title only needs its opening.
fn bounded_seed(seed: &str) -> String {
    const SEED_MAX_CHARS: usize = 2000;
    let trimmed = seed.trim();
    if trimmed.chars().count() <= SEED_MAX_CHARS {
        return trimmed.to_string();
    }
    trimmed.chars().take(SEED_MAX_CHARS).collect()
}

/// The exact argument list. Separated from the run so a test can assert the
/// safety flags are present without executing anything.
pub fn title_arguments(seed: &str) -> Vec<String> {
    vec![
        "exec".to_string(),
        "--ephemeral".to_string(),
        "--skip-git-repo-check".to_string(),
        "--sandbox".to_string(),
        "read-only".to_string(),
        "--model".to_string(),
        TITLE_MODEL_SLUG.to_string(),
        "--config".to_string(),
        format!("model_reasoning_effort=\"{TITLE_REASONING_EFFORT}\""),
        title_prompt(seed),
    ]
}

/// Turn the run's stdout into a title, or nothing.
///
/// The output is a transcript: banner, the prompt echoed back, the answer, a
/// token count, then the answer again. The last usable line is the answer, and
/// it is treated as untrusted -- a model that returns prose, control characters
/// or nothing at all must leave the existing title alone.
pub fn title_from_output(stdout: &str) -> Option<String> {
    let candidate = stdout.lines().rev().map(str::trim).find(|line| {
        !line.is_empty()
            && !line.starts_with("tokens used")
            && !line.chars().any(|ch| ch.is_control())
    })?;
    let candidate = candidate
        .trim_matches(|ch: char| ch == '"' || ch == '\'' || ch == '`')
        .trim_end_matches(['.', '!', ',', ':', ';'])
        .trim();
    if candidate.is_empty() || candidate.chars().count() > TITLE_MAX_CHARS {
        return None;
    }
    // A title is one line of words. Anything with a path separator or a brace
    // is the model narrating, not naming.
    if candidate.contains(['{', '}', '\\']) {
        return None;
    }
    Some(candidate.to_string())
}

/// Ask for a title. `None` whenever anything is not exactly right: a title is
/// a nicety, and a failed one must never surface as an error or a wrong name.
pub async fn suggest_task_title(
    executable: &Path,
    working_directory: &Path,
    environment: Vec<(std::ffi::OsString, std::ffi::OsString)>,
    seed: &str,
) -> Option<String> {
    if seed.trim().is_empty() {
        return None;
    }
    let mut command = tokio::process::Command::new(executable);
    command
        .args(title_arguments(seed))
        .current_dir(working_directory)
        // Closed, or codex waits on it: `exec` reads a piped stdin as extra
        // prompt input and would hang here forever.
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    command.env_clear();
    for (key, value) in environment {
        command.env(key, value);
    }
    let mut child = command.spawn().ok()?;
    let output = match tokio::time::timeout(TITLE_DEADLINE, async {
        use tokio::io::AsyncReadExt;
        let mut stdout = child.stdout.take()?;
        let mut buffer = Vec::new();
        let mut chunk = [0u8; 8192];
        loop {
            let read = stdout.read(&mut chunk).await.ok()?;
            if read == 0 {
                break;
            }
            buffer.extend_from_slice(&chunk[..read]);
            if buffer.len() >= TITLE_MAX_OUTPUT_BYTES {
                break;
            }
        }
        let _ = child.wait().await;
        String::from_utf8(buffer).ok()
    })
    .await
    {
        Ok(Some(output)) => output,
        // Timed out or unreadable: the child is killed on drop, and the task
        // keeps whatever title it already had.
        _ => return None,
    };
    title_from_output(&output)
}

/// Where the one-shot runs. The task's own folder, so the model sees the
/// project it is naming work in; never a folder outside it.
pub fn title_working_directory(workspace_root: &Path) -> PathBuf {
    workspace_root.to_path_buf()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_one_shot_stays_read_only_ephemeral_and_on_the_small_model() {
        let arguments = title_arguments("fix the failing test");
        assert_eq!(arguments.first().map(String::as_str), Some("exec"));
        for required in [
            "--ephemeral",
            "--skip-git-repo-check",
            "--sandbox",
            "read-only",
            "--model",
            TITLE_MODEL_SLUG,
        ] {
            assert!(
                arguments.iter().any(|argument| argument == required),
                "the titling run must carry {required}; it is the only reason \
                 this exception to the exec ban is narrow enough to allow"
            );
        }
        assert!(arguments
            .iter()
            .any(|argument| argument == "model_reasoning_effort=\"low\""));
        // No way for a caller to smuggle its own arguments in.
        assert_eq!(arguments.len(), 10);
        assert!(arguments.last().unwrap().contains("fix the failing test"));
    }

    #[test]
    fn a_huge_first_message_is_bounded_before_it_is_sent() {
        let seed = "x".repeat(10_000);
        let prompt = title_prompt(&seed);
        assert!(prompt.chars().count() < 2_400, "{}", prompt.chars().count());
    }

    #[test]
    fn the_answer_is_the_last_usable_line_and_untrusted() {
        let transcript = "OpenAI Codex v1\n--------\nworkdir: /tmp\ncodex\n\
             Suggest compact ticket views\ntokens used\n3,420\n\
             Suggest compact ticket views\n";
        assert_eq!(
            title_from_output(transcript).as_deref(),
            Some("Suggest compact ticket views")
        );
        // Quotes and trailing punctuation are the model's, not the title's.
        assert_eq!(
            title_from_output("\"Fix the flaky test.\"").as_deref(),
            Some("Fix the flaky test")
        );
        // Refusals: nothing, prose far too long, or output that is not a name.
        assert_eq!(title_from_output(""), None);
        assert_eq!(title_from_output("   \n  \n"), None);
        assert_eq!(title_from_output(&"word ".repeat(40)), None);
        assert_eq!(title_from_output("{\"error\": \"nope\"}"), None);
    }

    #[test]
    fn an_empty_seed_asks_nothing() {
        // An empty seed is refused before anything spawns, so a current-thread
        // runtime is enough and no provider is ever started.
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let empty = runtime.block_on(suggest_task_title(
            Path::new("/nonexistent-codex"),
            Path::new("/tmp"),
            Vec::new(),
            "   ",
        ));
        assert_eq!(empty, None);
    }
}
