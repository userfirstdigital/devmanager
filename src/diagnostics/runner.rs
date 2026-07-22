use std::collections::BTreeMap;
use std::ffi::OsString;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::time::Duration;

const MAX_CAPTURE_BYTES: usize = 16 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandSpec {
    pub program: PathBuf,
    pub args: Vec<OsString>,
    pub timeout: Duration,
    pub env: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandOutput {
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub stdout: String,
    pub stderr: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandFailure {
    pub message: String,
}

pub type CommandRunnerFuture<'a> =
    Pin<Box<dyn Future<Output = Result<CommandOutput, CommandFailure>> + Send + 'a>>;

pub trait CommandRunner: Send + Sync {
    fn run<'a>(&'a self, spec: &'a CommandSpec) -> CommandRunnerFuture<'a>;
}

pub struct TokioCommandRunner;

impl CommandRunner for TokioCommandRunner {
    fn run<'a>(&'a self, spec: &'a CommandSpec) -> CommandRunnerFuture<'a> {
        Box::pin(async move { run_tokio(spec).await })
    }
}

async fn run_tokio(spec: &CommandSpec) -> Result<CommandOutput, CommandFailure> {
    use tokio::process::Command;

    let deadline = tokio::time::Instant::now() + spec.timeout;

    let mut command = Command::new(&spec.program);
    command.args(&spec.args);
    command.stdout(std::process::Stdio::piped());
    command.stderr(std::process::Stdio::piped());
    command.kill_on_drop(true);

    // Only apply allowlisted overrides; inherit the rest of the process environment.
    for (key, value) in &spec.env {
        command.env(key, value);
    }

    let mut child = command.spawn().map_err(|err| CommandFailure {
        message: format!("failed to spawn {}: {err}", spec.program.display()),
    })?;

    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let mut stdout_task = tokio::spawn(async move {
        match stdout {
            Some(pipe) => read_bounded_stream(pipe).await,
            None => (Vec::new(), false),
        }
    });
    let mut stderr_task = tokio::spawn(async move {
        match stderr {
            Some(pipe) => read_bounded_stream(pipe).await,
            None => (Vec::new(), false),
        }
    });

    let wait_result = tokio::time::timeout_at(deadline, child.wait()).await;
    match wait_result {
        Ok(Ok(status)) => {
            match collect_reader_output(deadline, &mut stdout_task, &mut stderr_task).await {
                Some(((stdout_bytes, stdout_trunc), (stderr_bytes, stderr_trunc))) => {
                    Ok(CommandOutput {
                        exit_code: status.code(),
                        timed_out: false,
                        stdout: finalize_captured_stream(&stdout_bytes, stdout_trunc),
                        stderr: finalize_captured_stream(&stderr_bytes, stderr_trunc),
                    })
                }
                None => {
                    // Child exited but a pipe stayed open (inherited handle); do not hang.
                    let _ = child.kill().await;
                    let _ = child.wait().await;
                    Ok(timed_out_command_output())
                }
            }
        }
        Ok(Err(err)) => {
            abort_and_join_readers(&mut stdout_task, &mut stderr_task).await;
            let _ = child.kill().await;
            let _ = child.wait().await;
            Err(CommandFailure {
                message: format!("command failed: {err}"),
            })
        }
        Err(_) => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            abort_and_join_readers(&mut stdout_task, &mut stderr_task).await;
            Ok(timed_out_command_output())
        }
    }
}

fn timed_out_command_output() -> CommandOutput {
    CommandOutput {
        exit_code: None,
        timed_out: true,
        stdout: String::new(),
        stderr: "command timed out".to_string(),
    }
}

/// Join both pipe readers before `deadline`. Handles stay abortable via `&mut JoinHandle`.
/// Returns `None` when EOF/join does not finish in time (e.g. inherited pipe handle).
async fn collect_reader_output(
    deadline: tokio::time::Instant,
    stdout_task: &mut tokio::task::JoinHandle<(Vec<u8>, bool)>,
    stderr_task: &mut tokio::task::JoinHandle<(Vec<u8>, bool)>,
) -> Option<((Vec<u8>, bool), (Vec<u8>, bool))> {
    let joined = tokio::time::timeout_at(deadline, async {
        let stdout = (&mut *stdout_task)
            .await
            .unwrap_or_else(|_| (Vec::new(), false));
        let stderr = (&mut *stderr_task)
            .await
            .unwrap_or_else(|_| (Vec::new(), false));
        (stdout, stderr)
    })
    .await;
    match joined {
        Ok(pair) => Some(pair),
        Err(_) => {
            abort_and_join_readers(stdout_task, stderr_task).await;
            None
        }
    }
}

async fn abort_and_join_readers(
    stdout_task: &mut tokio::task::JoinHandle<(Vec<u8>, bool)>,
    stderr_task: &mut tokio::task::JoinHandle<(Vec<u8>, bool)>,
) {
    stdout_task.abort();
    stderr_task.abort();
    // Abort drops the reader futures (and pipe ends). Bound the join so a stuck
    // cancellation path cannot hang the probe either.
    let abort_deadline = tokio::time::Instant::now() + Duration::from_millis(500);
    let _ = tokio::time::timeout_at(abort_deadline, async {
        let _ = (&mut *stdout_task).await;
        let _ = (&mut *stderr_task).await;
    })
    .await;
}

/// Retain at most [`MAX_CAPTURE_BYTES`] while continuing to drain so the pipe cannot block.
async fn read_bounded_stream<R>(mut reader: R) -> (Vec<u8>, bool)
where
    R: tokio::io::AsyncRead + Unpin,
{
    use tokio::io::AsyncReadExt;

    let mut retained = Vec::new();
    let mut truncated = false;
    let mut buf = [0u8; 8 * 1024];
    loop {
        match reader.read(&mut buf).await {
            Ok(0) => break,
            Ok(n) => {
                if retained.len() < MAX_CAPTURE_BYTES {
                    let space = MAX_CAPTURE_BYTES - retained.len();
                    let take = n.min(space);
                    retained.extend_from_slice(&buf[..take]);
                    if take < n {
                        truncated = true;
                    }
                } else {
                    truncated = true;
                }
            }
            Err(_) => break,
        }
    }
    (retained, truncated)
}

fn finalize_captured_stream(bytes: &[u8], truncated_during_capture: bool) -> String {
    let mut text = String::from_utf8_lossy(bytes).into_owned();
    if truncated_during_capture && !text.contains("…[truncated]") {
        if !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str("…[truncated]");
    }
    sanitize_captured(&text)
}

pub fn display_command(spec: &CommandSpec) -> String {
    let mut parts = vec![spec.program.display().to_string()];
    for arg in &spec.args {
        parts.push(arg.to_string_lossy().into_owned());
    }
    // Never include environment values in the display form.
    parts.join(" ")
}

pub fn sanitize_captured(raw: &str) -> String {
    let truncated = truncate_output(raw);
    let redacted = redact_secrets(&truncated);
    elide_home_paths(&redacted)
}

pub fn truncate_output(raw: &str) -> String {
    let bytes = raw.as_bytes();
    if bytes.len() <= MAX_CAPTURE_BYTES {
        return raw.to_string();
    }
    let mut end = MAX_CAPTURE_BYTES;
    while end > 0 && !raw.is_char_boundary(end) {
        end -= 1;
    }
    let mut out = raw[..end].to_string();
    out.push_str("\n…[truncated]");
    out
}

pub fn redact_secrets(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for line in raw.split_inclusive('\n') {
        out.push_str(&redact_line(line));
    }
    out
}

fn redact_line(line: &str) -> String {
    let (body, trailing) = split_trailing_newline(line);
    match redact_body(body) {
        Some(redacted) => format!("{redacted}{trailing}"),
        None => line.to_string(),
    }
}

fn redact_body(body: &str) -> Option<String> {
    let lower = body.to_ascii_lowercase();

    // Multiple sensitive fields (exact or suffixed keys / bearer): redact the whole line
    // so later values cannot leak after the first structured replacement returns.
    if count_secret_key_occurrences(&lower) > 1 {
        return Some("[redacted]".to_string());
    }

    if let Some(idx) = find_bearer_token(&lower) {
        return Some(format!("{}Bearer ***", &body[..idx]));
    }

    for key in ["token", "password", "secret", "bearer"] {
        if let Some(redacted) = try_redact_json_field(body, &lower, key) {
            return Some(redacted);
        }
        if let Some(redacted) = try_redact_assignment_or_header(body, &lower, key) {
            return Some(redacted);
        }
        if let Some(redacted) = try_redact_whitespace_pair(body, &lower, key) {
            return Some(redacted);
        }
    }

    for key in ["token", "password", "secret", "bearer"] {
        if contains_keyword_boundary(&lower, key) {
            // Malformed secret-bearing line: never leak the value.
            return Some("[redacted]".to_string());
        }
    }
    None
}

fn count_secret_key_occurrences(lower: &str) -> usize {
    let mut count = 0;
    for key in ["token", "password", "secret", "bearer"] {
        count += count_keyword_boundary_occurrences(lower, key);
    }
    count
}

/// Count exact keys (`token=`) and suffixed keys (`api_token=`, `access_token`).
fn count_keyword_boundary_occurrences(lower: &str, key: &str) -> usize {
    let mut count = 0;
    let mut start = 0;
    while let Some(rel) = lower[start..].find(key) {
        let idx = start + rel;
        let after = idx + key.len();
        let after_ok = lower
            .as_bytes()
            .get(after)
            .map(|b| !b.is_ascii_alphanumeric() && *b != b'_')
            .unwrap_or(true);
        if after_ok && is_secret_key_prefix_ok(lower, idx) {
            count += 1;
        }
        start = idx + key.len();
    }
    count
}

fn is_secret_key_prefix_ok(lower: &str, idx: usize) -> bool {
    if idx == 0 {
        return true;
    }
    let prev = lower.as_bytes()[idx - 1];
    // Exact word boundary, or suffix after `_` / `-` (api_token, access-token).
    (!prev.is_ascii_alphanumeric() && prev != b'_') || prev == b'_' || prev == b'-'
}

fn find_bearer_token(lower: &str) -> Option<usize> {
    let mut start = 0;
    while let Some(rel) = lower[start..].find("bearer ") {
        let idx = start + rel;
        let before_ok = idx == 0
            || lower.as_bytes()[idx - 1].is_ascii_whitespace()
            || matches!(lower.as_bytes()[idx - 1], b':' | b'=' | b'"' | b'\'');
        if before_ok {
            return Some(idx);
        }
        start = idx + 7;
    }
    None
}

fn contains_keyword_boundary(lower: &str, key: &str) -> bool {
    count_keyword_boundary_occurrences(lower, key) > 0
}

fn try_redact_json_field(body: &str, lower: &str, key: &str) -> Option<String> {
    for quote in [b'"', b'\''] {
        let quote_ch = quote as char;
        // Match exact `"token"` / suffixed `"access_token"` via trailing `token"`.
        let needle = format!("{key}{quote_ch}");
        let mut start = 0;
        while let Some(rel) = lower[start..].find(&needle) {
            let key_start = start + rel;
            if !is_secret_key_prefix_ok(lower, key_start) {
                start = key_start + 1;
                continue;
            }
            let mut i = key_start + key.len() + 1; // after closing quote of the key
            while i < body.len() && body.as_bytes()[i].is_ascii_whitespace() {
                i += 1;
            }
            if i >= body.len() || body.as_bytes()[i] != b':' {
                start = key_start + 1;
                continue;
            }
            i += 1;
            while i < body.len() && body.as_bytes()[i].is_ascii_whitespace() {
                i += 1;
            }
            let value_start = i;
            let value_end = scan_json_value_end(body, value_start);
            let replacement = match body.as_bytes().get(value_start).copied() {
                Some(b'"') => "\"***\"",
                Some(b'\'') => "'***'",
                _ => "***",
            };
            return Some(format!(
                "{}{}{}",
                &body[..value_start],
                replacement,
                &body[value_end..]
            ));
        }
    }
    None
}

fn scan_json_value_end(body: &str, value_start: usize) -> usize {
    let bytes = body.as_bytes();
    if value_start >= bytes.len() {
        return value_start;
    }
    match bytes[value_start] {
        q @ (b'"' | b'\'') => {
            let mut i = value_start + 1;
            while i < bytes.len() {
                if bytes[i] == b'\\' {
                    i = (i + 2).min(bytes.len());
                    continue;
                }
                if bytes[i] == q {
                    return i + 1;
                }
                i += 1;
            }
            bytes.len()
        }
        _ => {
            let mut i = value_start;
            while i < bytes.len() {
                match bytes[i] {
                    b',' | b'}' | b']' => break,
                    _ => i += 1,
                }
            }
            i
        }
    }
}

fn try_redact_assignment_or_header(body: &str, lower: &str, key: &str) -> Option<String> {
    let idx = find_assignment(lower, key)?;
    let mut key_end = idx;
    while key_end < body.len() {
        let b = body.as_bytes()[key_end];
        if b.is_ascii_alphanumeric() || b == b'_' || b == b'-' {
            key_end += 1;
        } else {
            break;
        }
    }
    let after = &body[key_end..];
    let trimmed = after.trim_start();
    let leading_ws = &after[..after.len() - trimmed.len()];

    if let Some(rest) = trimmed.strip_prefix('=') {
        let value = rest.trim_start();
        let pad = &rest[..rest.len() - value.len()];
        let redacted = match value.chars().next() {
            Some(q @ ('"' | '\'')) => format!("{q}***{q}"),
            _ => "***".to_string(),
        };
        return Some(format!(
            "{}{}{}={}{}",
            &body[..idx],
            &body[idx..key_end],
            leading_ws,
            pad,
            redacted
        ));
    }

    if let Some(rest) = trimmed.strip_prefix(':') {
        // Header form: avoid treating JSON `"key":` (handled elsewhere) — if the key
        // was introduced with a quote immediately before idx, skip.
        if idx > 0 && matches!(body.as_bytes()[idx - 1], b'"' | b'\'') {
            return None;
        }
        let value = rest.trim_start();
        let pad = &rest[..rest.len() - value.len()];
        let redacted = match value.chars().next() {
            Some(q @ ('"' | '\'')) => format!("{q}***{q}"),
            _ => "***".to_string(),
        };
        return Some(format!(
            "{}{}{}:{}{}",
            &body[..idx],
            &body[idx..key_end],
            leading_ws,
            pad,
            redacted
        ));
    }

    None
}

fn try_redact_whitespace_pair(body: &str, lower: &str, key: &str) -> Option<String> {
    let idx = find_assignment(lower, key)?;
    let mut key_end = idx;
    while key_end < body.len() {
        let b = body.as_bytes()[key_end];
        if b.is_ascii_alphanumeric() || b == b'_' || b == b'-' {
            key_end += 1;
        } else {
            break;
        }
    }
    let after = &body[key_end..];
    if after.is_empty() || !after.as_bytes()[0].is_ascii_whitespace() {
        return None;
    }
    let trimmed = after.trim_start();
    if trimmed.is_empty() || trimmed.starts_with('=') || trimmed.starts_with(':') {
        return None;
    }
    let leading_ws = &after[..after.len() - trimmed.len()];
    Some(format!(
        "{}{}{}***",
        &body[..idx],
        &body[idx..key_end],
        leading_ws
    ))
}

fn find_assignment(lower: &str, key: &str) -> Option<usize> {
    let mut start = 0;
    while let Some(rel) = lower[start..].find(key) {
        let idx = start + rel;
        let before_ok = idx == 0
            || lower
                .as_bytes()
                .get(idx - 1)
                .map(|b| !b.is_ascii_alphanumeric() && *b != b'_')
                .unwrap_or(true);
        let after = idx + key.len();
        let after_ok = lower
            .as_bytes()
            .get(after)
            .map(|b| *b == b'=' || *b == b':' || b.is_ascii_whitespace())
            .unwrap_or(false);
        if before_ok && after_ok {
            return Some(idx);
        }
        // Also match KEY=value where key is a suffix of an identifier (my_secret=)
        let before_ok_suffix = idx > 0
            && lower
                .as_bytes()
                .get(idx - 1)
                .map(|b| *b == b'_' || *b == b'-')
                .unwrap_or(false);
        if before_ok_suffix && after_ok {
            let mut begin = idx;
            while begin > 0 {
                let b = lower.as_bytes()[begin - 1];
                if b.is_ascii_alphanumeric() || b == b'_' || b == b'-' {
                    begin -= 1;
                } else {
                    break;
                }
            }
            return Some(begin);
        }
        start = idx + key.len();
    }
    None
}

fn split_trailing_newline(value: &str) -> (&str, &str) {
    if let Some(stripped) = value.strip_suffix('\n') {
        if let Some(stripped) = stripped.strip_suffix('\r') {
            (stripped, "\r\n")
        } else {
            (stripped, "\n")
        }
    } else {
        (value, "")
    }
}

pub fn elide_home_paths(raw: &str) -> String {
    let Some(home) = dirs::home_dir() else {
        return raw.to_string();
    };
    elide_home_paths_with(raw, &home)
}

pub fn elide_home_paths_with(raw: &str, home: &Path) -> String {
    elide_home_paths_with_options(raw, home, cfg!(windows))
}

/// Elide `home` prefixes in `raw`. When `ascii_case_insensitive` is true (Windows),
/// matching ignores ASCII case so mixed-case paths still collapse to `~`.
pub fn elide_home_paths_with_options(
    raw: &str,
    home: &Path,
    ascii_case_insensitive: bool,
) -> String {
    let home_str = home.to_string_lossy();
    if home_str.is_empty() {
        return raw.to_string();
    }
    if ascii_case_insensitive {
        replace_ascii_case_insensitive(raw, home_str.as_ref(), "~")
    } else {
        raw.replace(home_str.as_ref(), "~")
    }
}

fn replace_ascii_case_insensitive(haystack: &str, needle: &str, replacement: &str) -> String {
    if needle.is_empty() {
        return haystack.to_string();
    }
    let lower_hay = haystack.to_ascii_lowercase();
    let lower_needle = needle.to_ascii_lowercase();
    let mut out = String::with_capacity(haystack.len());
    let mut i = 0;
    while let Some(rel) = lower_hay[i..].find(&lower_needle) {
        let idx = i + rel;
        out.push_str(&haystack[i..idx]);
        out.push_str(replacement);
        i = idx + needle.len();
    }
    out.push_str(&haystack[i..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncates_output_at_16_kib() {
        let big = "a".repeat(MAX_CAPTURE_BYTES + 50);
        let out = truncate_output(&big);
        assert!(out.ends_with("…[truncated]"));
        assert!(out.len() < big.len());
        assert!(out.as_bytes().len() <= MAX_CAPTURE_BYTES + "\n…[truncated]".len());
    }

    #[test]
    fn redacts_token_password_secret_bearer_case_insensitive() {
        let raw = "TOKEN=abc\nPassword: hunter2\nmy_secret=xyz\nAuthorization: Bearer deadbeef\n";
        let redacted = redact_secrets(raw);
        assert!(redacted.contains("TOKEN="));
        assert!(redacted.contains("***"));
        assert!(redacted.to_ascii_lowercase().contains("password:"));
        assert!(redacted.contains("my_secret="));
        assert!(redacted.contains("Bearer ***"));
        assert!(!redacted.contains("hunter2"));
        assert!(!redacted.contains("deadbeef"));
        assert!(!redacted.contains("abc"));
        assert!(!redacted.contains("xyz"));
    }

    #[test]
    fn redacts_json_whitespace_assignment_header_and_bearer_without_leaking_values() {
        let cases = [
            (r#"{"token":"json-secret-value"}"#, "json-secret-value"),
            (r#"{"password": "spaced-secret"}"#, "spaced-secret"),
            ("password whitespace-secret", "whitespace-secret"),
            ("token = 'assigned-secret'", "assigned-secret"),
            ("token=unquoted-secret", "unquoted-secret"),
            ("X-Api-Key: token=header-secret", "header-secret"),
            (
                "Authorization: Bearer bearer-secret-token",
                "bearer-secret-token",
            ),
            ("Bearer bare-bearer-secret", "bare-bearer-secret"),
        ];
        for (raw, secret) in cases {
            let redacted = redact_secrets(raw);
            assert!(
                !redacted.contains(secret),
                "secret {secret:?} leaked in {redacted:?} from {raw:?}"
            );
            assert!(
                redacted.contains("***") || redacted.contains("[redacted]"),
                "expected redaction marker in {redacted:?} from {raw:?}"
            );
        }
    }

    #[test]
    fn malformed_secret_lines_are_conservatively_redacted() {
        let redacted = redact_secrets("token");
        assert_eq!(redacted, "[redacted]");
        assert!(!redact_secrets("password>>oops-secret").contains("oops-secret"));
    }

    #[test]
    fn multi_secret_json_lines_redact_every_value() {
        let cases = [
            (
                r#"{"token":"alpha-secret","password":"beta-secret"}"#,
                &["alpha-secret", "beta-secret"][..],
            ),
            (
                r#"{"token":"first-token","token":"second-token"}"#,
                &["first-token", "second-token"][..],
            ),
            (
                r#"{"password":"pw-one","secret":"sec-two","bearer":"br-three"}"#,
                &["pw-one", "sec-two", "br-three"][..],
            ),
            (
                r#"{"token": "spaced-a", "password": "spaced-b"}"#,
                &["spaced-a", "spaced-b"][..],
            ),
            ("api_token=alpha password=beta", &["alpha", "beta"][..]),
            (
                r#"{"access_token":"alpha","client_secret":"beta"}"#,
                &["alpha", "beta"][..],
            ),
            (
                r#"api_token=alpha {"password":"beta"}"#,
                &["alpha", "beta"][..],
            ),
        ];
        for (raw, secrets) in cases {
            let redacted = redact_secrets(raw);
            for secret in secrets {
                assert!(
                    !redacted.contains(secret),
                    "secret {secret:?} leaked in {redacted:?} from {raw:?}"
                );
            }
            assert!(
                redacted.contains("[redacted]") || redacted.contains("***"),
                "expected redaction marker in {redacted:?} from {raw:?}"
            );
        }
    }

    #[test]
    fn suffixed_single_secret_keys_are_redacted() {
        for (raw, secret) in [
            ("api_token=only-secret", "only-secret"),
            (r#"{"access_token":"json-only"}"#, "json-only"),
            ("client_secret=plain-secret", "plain-secret"),
        ] {
            let redacted = redact_secrets(raw);
            assert!(
                !redacted.contains(secret),
                "secret {secret:?} leaked in {redacted:?} from {raw:?}"
            );
            assert!(redacted.contains("***") || redacted.contains("[redacted]"));
        }
    }

    #[test]
    fn elides_home_directory_segments() {
        let home = PathBuf::from(if cfg!(windows) {
            r"C:\Users\example"
        } else {
            "/home/example"
        });
        let path = home.join("projects").join("notes.txt");
        let elided = elide_home_paths_with(&path.to_string_lossy(), &home);
        assert!(elided.starts_with('~'));
        assert!(!elided.contains("example"));
    }

    #[test]
    fn elides_windows_home_paths_ascii_case_insensitively() {
        let home = PathBuf::from(r"C:\Users\Example");
        let mixed = r"c:\users\example\projects\notes.txt";
        let elided = elide_home_paths_with_options(mixed, &home, true);
        assert_eq!(elided, r"~\projects\notes.txt");
        assert!(!elided.to_ascii_lowercase().contains("example"));

        let exact_only = elide_home_paths_with_options(mixed, &home, false);
        assert_eq!(
            exact_only, mixed,
            "non-Windows semantics stay case-sensitive"
        );
    }

    #[test]
    fn display_form_never_includes_environment_values() {
        let mut env = BTreeMap::new();
        env.insert("SECRET_TOKEN".to_string(), "super-secret".to_string());
        let spec = CommandSpec {
            program: PathBuf::from("pwsh"),
            args: vec![OsString::from("-NoProfile"), OsString::from("-Version")],
            timeout: Duration::from_secs(5),
            env,
        };
        let display = display_command(&spec);
        assert_eq!(display, "pwsh -NoProfile -Version");
        assert!(!display.contains("super-secret"));
        assert!(!display.contains("SECRET_TOKEN"));
    }

    #[tokio::test]
    async fn bounded_stream_reader_retains_at_most_16kib_while_draining() {
        use tokio::io::AsyncWriteExt;

        let payload_len = MAX_CAPTURE_BYTES + 64 * 1024;
        let (client, mut server) = tokio::io::duplex(8 * 1024);
        let writer = tokio::spawn(async move {
            let chunk = vec![b'x'; 16 * 1024];
            let mut written = 0usize;
            while written < payload_len {
                let n = chunk.len().min(payload_len - written);
                server.write_all(&chunk[..n]).await.unwrap();
                written += n;
            }
        });
        let (captured, truncated) = read_bounded_stream(client).await;
        writer.await.unwrap();
        assert!(truncated);
        assert_eq!(captured.len(), MAX_CAPTURE_BYTES);
        let finalized = finalize_captured_stream(&captured, truncated);
        assert!(finalized.contains("…[truncated]"));
        assert!(finalized.len() < payload_len);
    }

    #[tokio::test]
    async fn pending_readers_after_child_exit_time_out_instead_of_hanging() {
        // Equivalent to: child has exited but another process still holds the
        // stdout/stderr write ends, so readers never see EOF.
        let deadline = tokio::time::Instant::now() + Duration::from_millis(80);
        let mut stdout_task = tokio::spawn(async {
            std::future::pending::<()>().await;
            (Vec::new(), false)
        });
        let mut stderr_task = tokio::spawn(async {
            std::future::pending::<()>().await;
            (Vec::new(), false)
        });

        let started = std::time::Instant::now();
        let collected = collect_reader_output(deadline, &mut stdout_task, &mut stderr_task).await;
        assert!(collected.is_none(), "hanging readers must not succeed");
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "must return timed_out path quickly, elapsed={:?}",
            started.elapsed()
        );
    }

    #[tokio::test]
    async fn run_tokio_times_out_when_pipe_readers_never_eof_after_exit() {
        // End-to-end through run_tokio: child exits immediately, but we simulate the
        // post-wait hang by driving collect_reader_output via a custom path is covered
        // above; here spawn a process that exits while a held pipe would hang.
        // Use a short timeout and a child that exits after writing nothing — readers
        // complete normally. The deterministic hang case is pending_readers_*.
        //
        // Direct regression for run_tokio: inject hanging readers by using an
        // internal-only test hook isn't available, so assert the public timeout
        // budget cannot be exceeded when wait succeeds and readers hang by
        // composing the same deadline logic used in production.
        let overall_timeout = Duration::from_millis(150);
        let deadline = tokio::time::Instant::now() + overall_timeout;
        let mut stdout_task = tokio::spawn(async {
            // Retain forever: models an inherited stdout handle after child exit.
            std::future::pending::<()>().await;
            (vec![b'x'; 8], false)
        });
        let mut stderr_task = tokio::spawn(async {
            std::future::pending::<()>().await;
            (Vec::new(), false)
        });

        // Child "wait" already succeeded (status ignored); only readers remain.
        let started = std::time::Instant::now();
        let output = match collect_reader_output(deadline, &mut stdout_task, &mut stderr_task).await
        {
            Some(((stdout_bytes, stdout_trunc), (stderr_bytes, stderr_trunc))) => CommandOutput {
                exit_code: Some(0),
                timed_out: false,
                stdout: finalize_captured_stream(&stdout_bytes, stdout_trunc),
                stderr: finalize_captured_stream(&stderr_bytes, stderr_trunc),
            },
            None => timed_out_command_output(),
        };
        assert!(output.timed_out);
        assert_eq!(output.exit_code, None);
        assert!(output.stderr.contains("timed out"));
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "post-exit hanging pipes must not block beyond deadline, elapsed={:?}",
            started.elapsed()
        );
    }

    #[tokio::test]
    async fn run_tokio_timeout_kills_child_without_hanging() {
        #[cfg(windows)]
        let (program, args): (PathBuf, Vec<OsString>) = (
            PathBuf::from("ping"),
            vec![
                OsString::from("-n"),
                OsString::from("20"),
                OsString::from("127.0.0.1"),
            ],
        );
        #[cfg(not(windows))]
        let (program, args): (PathBuf, Vec<OsString>) =
            (PathBuf::from("sleep"), vec![OsString::from("20")]);

        let started = std::time::Instant::now();
        let output = run_tokio(&CommandSpec {
            program,
            args,
            timeout: Duration::from_millis(300),
            env: BTreeMap::new(),
        })
        .await
        .unwrap();
        assert!(output.timed_out);
        assert!(started.elapsed() < Duration::from_secs(5));
        assert_eq!(output.exit_code, None);
        assert!(output.stderr.contains("timed out"));
    }

    #[tokio::test]
    async fn run_tokio_bounds_large_stdout_and_stderr_from_real_child() {
        // Direct spawn of pwsh.exe (not a shell wrapper) writing >16KiB per stream.
        let pwsh = [
            r"C:\Program Files\PowerShell\7\pwsh.exe",
            r"C:\Program Files\PowerShell\7-preview\pwsh.exe",
        ]
        .into_iter()
        .map(PathBuf::from)
        .find(|p| p.is_file())
        .or_else(|| {
            std::process::Command::new("where")
                .arg("pwsh")
                .output()
                .ok()
                .and_then(|output| {
                    let text = String::from_utf8_lossy(&output.stdout);
                    text.lines()
                        .next()
                        .map(str::trim)
                        .filter(|line| !line.is_empty())
                        .map(PathBuf::from)
                        .filter(|path| path.is_file())
                })
        });
        let Some(pwsh) = pwsh else {
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("write_large.ps1");
        let oversized = MAX_CAPTURE_BYTES + 50_000;
        std::fs::write(
            &script,
            format!(
                "$o = 'O' * {oversized}; $e = 'E' * {oversized}; [Console]::Out.Write($o); [Console]::Error.Write($e)\n"
            ),
        )
        .unwrap();
        let output = run_tokio(&CommandSpec {
            program: pwsh,
            args: vec![
                OsString::from("-NoProfile"),
                OsString::from("-NoLogo"),
                OsString::from("-File"),
                OsString::from(script.as_os_str()),
            ],
            timeout: Duration::from_secs(15),
            env: BTreeMap::new(),
        })
        .await
        .unwrap();
        assert!(
            !output.timed_out,
            "large-writer child timed out unexpectedly"
        );
        assert!(
            output.stdout.contains("…[truncated]"),
            "stdout should be capture-bounded: len={}",
            output.stdout.len()
        );
        assert!(
            output.stderr.contains("…[truncated]"),
            "stderr should be capture-bounded: len={}",
            output.stderr.len()
        );
        assert!(output.stdout.len() < oversized);
        assert!(output.stderr.len() < oversized);
    }
}
