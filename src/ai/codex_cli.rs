//! Helpers for invoking the Codex CLI: command-line tokenizing and quoting,
//! executable resolution, bounded capability probing, help-text flag
//! detection, and canonical visible-text shaping shared with the remote
//! composer. No process here outlives its call; the Codex TUI itself is
//! launched by the process manager with the user's own command.

use std::path::{Path, PathBuf};
use std::process::Stdio;

const CODEX_PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(12);
const CODEX_PROBE_TREE_KILL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(1);
const CODEX_PROBE_PIPE_DRAIN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(1);
#[cfg(not(windows))]
const CODEX_PROCESS_GROUP_TERM_GRACE: std::time::Duration = std::time::Duration::from_millis(250);
const MAX_PROBE_OUTPUT_BYTES: usize = 256 * 1024;
const MAX_VISIBLE_TEXT_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodexConfigOverride {
    key: String,
    toml_value: String,
}

impl CodexConfigOverride {
    pub fn new(key: impl Into<String>, toml_value: impl Into<String>) -> Result<Self, String> {
        let key = key.into();
        if key.is_empty()
            || !key.chars().all(|character| {
                character.is_ascii_alphanumeric() || matches!(character, '_' | '-' | '.')
            })
        {
            return Err("Codex config override key is invalid".to_string());
        }
        let toml_value = toml_value.into();
        if toml_value.is_empty() || toml_value.contains(['\r', '\n']) {
            return Err("Codex config override value must be nonblank and single-line".to_string());
        }
        Ok(Self { key, toml_value })
    }

    pub fn key(&self) -> &str {
        &self.key
    }

    pub fn toml_value(&self) -> &str {
        &self.toml_value
    }

    pub(crate) fn argument(&self) -> String {
        format!("{}={}", self.key, self.toml_value)
    }
}

pub(crate) fn split_command_line(command: &str) -> Result<Vec<String>, String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut quote = None;
    let mut characters = command.chars().peekable();
    while let Some(character) = characters.next() {
        match quote {
            Some(delimiter) if character == delimiter => quote = None,
            Some('"') if character == '\\' && characters.peek() == Some(&'"') => {
                current.push(characters.next().unwrap_or('"'));
            }
            Some(_) => current.push(character),
            None if matches!(character, '\'' | '"') => quote = Some(character),
            None if character.is_whitespace() => {
                if !current.is_empty() {
                    tokens.push(std::mem::take(&mut current));
                }
            }
            None if matches!(character, '|' | ';' | '&' | '<' | '>' | '\r' | '\n') => {
                return Err(
                    "Custom Codex wrapper or shell operators cannot be adapted safely".to_string(),
                );
            }
            None => current.push(character),
        }
    }
    if quote.is_some() {
        return Err("Codex command contains an unterminated quote".to_string());
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    Ok(tokens)
}

pub(crate) fn quote_command_for_shell(tokens: &[String], shell_program: &str) -> String {
    let shell = Path::new(shell_program)
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or(shell_program)
        .to_ascii_lowercase();
    if shell.contains("powershell") || shell == "pwsh" {
        let command = tokens
            .iter()
            .map(|token| format!("'{}'", token.replace('\'', "''")))
            .collect::<Vec<_>>()
            .join(" ");
        return format!("& {command}");
    }
    if shell == "cmd" {
        return tokens
            .iter()
            .map(|token| format!("\"{}\"", token.replace('"', "\\\"")))
            .collect::<Vec<_>>()
            .join(" ");
    }
    tokens
        .iter()
        .map(|token| format!("'{}'", token.replace('\'', "'\"'\"'")))
        .collect::<Vec<_>>()
        .join(" ")
}

pub(crate) fn resolve_executable(program: &str) -> Result<PathBuf, String> {
    let supplied = PathBuf::from(program);
    if supplied.components().count() > 1 || supplied.is_absolute() {
        return supplied
            .canonicalize()
            .map_err(|error| format!("Cannot resolve Codex executable `{program}`: {error}"));
    }

    let path = std::env::var_os("PATH")
        .ok_or_else(|| "PATH is unavailable while resolving Codex".to_string())?;
    let extensions = std::env::var("PATHEXT").unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".to_string());
    let names = executable_candidate_names(program, cfg!(windows), &extensions);

    for directory in std::env::split_paths(&path) {
        for name in &names {
            let candidate = directory.join(name);
            if candidate.is_file() {
                return candidate.canonicalize().map_err(|error| {
                    format!("Cannot canonicalize `{}`: {error}", candidate.display())
                });
            }
        }
    }
    Err(format!(
        "Codex executable `{program}` was not found on PATH"
    ))
}

fn executable_candidate_names(program: &str, windows: bool, path_ext: &str) -> Vec<String> {
    let mut names = Vec::new();
    if windows && Path::new(program).extension().is_none() {
        names.extend(
            path_ext
                .split(';')
                .filter(|extension| !extension.is_empty())
                .map(|extension| format!("{program}{}", extension.to_ascii_lowercase())),
        );
        names.extend(
            path_ext
                .split(';')
                .filter(|extension| !extension.is_empty())
                .map(|extension| format!("{program}{}", extension.to_ascii_uppercase())),
        );
    }
    names.push(program.to_string());
    names
}

#[derive(Debug, PartialEq, Eq)]
enum SuspendedChildStartup<Job> {
    ClaimFailed(String),
    AfterClaim { error: String, job: Job },
}

fn claim_then_resume_suspended_child<Job, Claim, BeforeResume, Resume>(
    pid: u32,
    claim: Claim,
    before_resume: BeforeResume,
    resume: Resume,
) -> Result<Job, SuspendedChildStartup<Job>>
where
    Claim: FnOnce(u32) -> Result<Job, String>,
    BeforeResume: FnOnce(&Job) -> Result<(), String>,
    Resume: FnOnce(u32) -> Result<(), String>,
{
    let job = match claim(pid) {
        Ok(job) => job,
        Err(error) => return Err(SuspendedChildStartup::ClaimFailed(error)),
    };
    if let Err(error) = before_resume(&job) {
        return Err(SuspendedChildStartup::AfterClaim { error, job });
    }
    match resume(pid) {
        Ok(()) => Ok(job),
        Err(error) => Err(SuspendedChildStartup::AfterClaim { error, job }),
    }
}

pub(crate) fn run_probe(executable: &Path, args: &[String]) -> Result<String, String> {
    let mut command = std::process::Command::new(executable);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(crate::services::platform_service::MANAGED_PROCESS_CREATION_FLAGS);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let mut child = command.spawn().map_err(|error| {
        format!(
            "Failed to probe Codex executable `{}`: {error}",
            executable.display()
        )
    })?;
    let pid = child.id();
    // On Windows, kill-on-close job ownership is the authoritative backstop
    // for a wrapper that exits before its descendants. Unix probes are placed
    // in a dedicated process group above. A failed Windows claim leaves the
    // root suspended, so only its retained Child handle may be used to roll it
    // back; a raw PID/tree fallback would recreate authority we do not own.
    // Resume happens only after a successful claim (and any attestation gate)
    // so identity checks observe a still-suspended child. A resume failure
    // keeps the exact Child/Job pair for terminate_probe_tree.
    let managed_job = match claim_then_resume_suspended_child(
        pid,
        |pid| match crate::services::platform_service::claim_suspended_process(pid) {
            Ok(None) if cfg!(windows) => {
                Err("managed Windows Job was unavailable".to_string())
            }
            Ok(job) => Ok(job),
            Err(error) => Err(error),
        },
        |_| Ok(()),
        {
            #[cfg(windows)]
            {
                crate::services::platform_service::resume_suspended_process
            }
            #[cfg(not(windows))]
            {
                |_pid| Ok(())
            }
        },
    ) {
        Ok(managed_job) => managed_job,
        Err(SuspendedChildStartup::ClaimFailed(error)) => {
            terminate_probe_tree(&mut child, pid, None);
            return Err(format!(
                "Cannot own Codex capability probe process tree: {error}"
            ));
        }
        Err(SuspendedChildStartup::AfterClaim { error, job }) => {
            terminate_probe_tree(&mut child, pid, job);
            return Err(format!(
                "Cannot resume Codex capability probe process: {error}"
            ));
        }
    };
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let mut stdout_reader = spawn_probe_pipe_reader(stdout);
    let mut stderr_reader = spawn_probe_pipe_reader(stderr);
    let started = std::time::Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Ok(status),
            Ok(None) if started.elapsed() < CODEX_PROBE_TIMEOUT => {
                std::thread::sleep(std::time::Duration::from_millis(25));
            }
            Ok(None) => {
                break Err(format!(
                    "Codex capability probe timed out after {} seconds",
                    CODEX_PROBE_TIMEOUT.as_secs()
                ))
            }
            Err(error) => break Err(format!("Codex capability probe failed: {error}")),
        }
    };

    terminate_probe_tree(&mut child, pid, managed_job);
    let pipe_deadline = std::time::Instant::now()
        .checked_add(CODEX_PROBE_PIPE_DRAIN_TIMEOUT)
        .ok_or_else(|| "Codex capability probe pipe deadline overflow".to_string())?;
    let stdout = receive_probe_pipe(&mut stdout_reader, pipe_deadline)?;
    let stderr = receive_probe_pipe(&mut stderr_reader, pipe_deadline)?;
    let status = status?;
    let output = format!(
        "{}{}",
        String::from_utf8_lossy(&stdout),
        String::from_utf8_lossy(&stderr)
    );
    if !status.success() {
        return Err(format!(
            "Codex capability probe exited with {status}: {}",
            truncate_utf8(output.trim(), 2_048)
        ));
    }
    Ok(output)
}

fn spawn_probe_pipe_reader<R: std::io::Read + Send + 'static>(pipe: Option<R>) -> ProbePipeReader {
    let (sender, receiver) = std::sync::mpsc::sync_channel(1);
    let thread = std::thread::spawn(move || {
        let _ = sender.send(capture_probe_pipe(pipe));
    });
    ProbePipeReader {
        receiver,
        thread: Some(thread),
    }
}

struct ProbePipeReader {
    receiver: std::sync::mpsc::Receiver<Vec<u8>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

fn receive_probe_pipe(
    reader: &mut ProbePipeReader,
    deadline: std::time::Instant,
) -> Result<Vec<u8>, String> {
    let received = reader
        .receiver
        .recv_timeout(deadline.saturating_duration_since(std::time::Instant::now()));
    match received {
        Ok(bytes) => {
            join_probe_pipe_thread(reader, deadline, "Codex probe pipe reader")?;
            Ok(bytes)
        }
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
            cancel_probe_pipe_read(reader);
            // Cancellation is an ownership-settlement operation. Give the
            // exact retained thread a small, bounded acknowledgement window;
            // returning while it still owns the pipe would detach an actor.
            let cancellation_deadline = std::time::Instant::now()
                .checked_add(std::time::Duration::from_millis(250))
                .unwrap_or_else(|| std::process::abort());
            join_probe_pipe_thread(
                reader,
                cancellation_deadline,
                "timed-out Codex probe pipe reader",
            )?;
            Err("Codex capability probe pipe drain exceeded its deadline".to_string())
        }
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
            join_probe_pipe_thread(reader, deadline, "failed Codex probe pipe reader")?;
            Err("Codex capability probe pipe reader disconnected".to_string())
        }
    }
}

fn cancel_probe_pipe_read(reader: &ProbePipeReader) {
    #[cfg(windows)]
    if let Some(thread) = reader.thread.as_ref() {
        use std::ffi::c_void;
        use std::os::windows::io::AsRawHandle;

        #[link(name = "kernel32")]
        extern "system" {
            fn CancelSynchronousIo(thread: *mut c_void) -> i32;
        }

        // SAFETY: the JoinHandle retains the exact reader-thread handle until
        // `join_probe_pipe_thread` consumes it below.
        unsafe {
            let _ = CancelSynchronousIo(thread.as_raw_handle());
        }
    }
}

fn join_probe_pipe_thread(
    reader: &mut ProbePipeReader,
    deadline: std::time::Instant,
    context: &str,
) -> Result<(), String> {
    let Some(thread) = reader.thread.take() else {
        return Ok(());
    };
    while !thread.is_finished() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    if !thread.is_finished() {
        // There is no safe way to drop a JoinHandle without detaching its
        // thread. A probe reader that ignores both tree closure and explicit
        // cancellation violates the no-orphan host invariant, so fail closed.
        std::process::abort();
    }
    thread.join().map_err(|_| format!("{context} panicked"))
}

impl Drop for ProbePipeReader {
    fn drop(&mut self) {
        if self.thread.is_none() {
            return;
        }
        cancel_probe_pipe_read(self);
        let deadline = std::time::Instant::now()
            .checked_add(std::time::Duration::from_millis(250))
            .unwrap_or_else(|| std::process::abort());
        if join_probe_pipe_thread(self, deadline, "Codex probe pipe reader drop").is_err() {
            std::process::abort();
        }
    }
}

fn terminate_probe_tree(
    child: &mut std::process::Child,
    _pid: u32,
    managed_job: Option<crate::services::platform_service::ManagedProcessJob>,
) {
    if let Some(managed_job) = managed_job {
        // Normal Windows probes are enrolled before execution. Closing this
        // identity-bound job is safer than snapshotting and killing raw PIDs.
        drop(managed_job);
    } else {
        // Unix probes explicitly created their own process group before any
        // code ran, so that group is a sealed pre-authority rollback
        // capability. On Windows this branch is reachable only when the
        // suspended root could not be enrolled; the Child handle below is the
        // sole rollback authority and no descendant could have run.
        #[cfg(not(windows))]
        {
            let _ = crate::services::platform_service::terminate_owned_process_group(
                _pid,
                CODEX_PROCESS_GROUP_TERM_GRACE,
            );
        }
    }

    let _ = child.kill();
    let reap_deadline = std::time::Instant::now()
        .checked_add(CODEX_PROBE_TREE_KILL_TIMEOUT)
        .unwrap_or_else(|| std::process::abort());
    while std::time::Instant::now() < reap_deadline {
        match child.try_wait() {
            Ok(Some(_)) | Err(_) => break,
            Ok(None) => std::thread::sleep(std::time::Duration::from_millis(10)),
        }
    }
}

fn capture_probe_pipe<R: std::io::Read>(pipe: Option<R>) -> Vec<u8> {
    let Some(mut pipe) = pipe else {
        return Vec::new();
    };
    let mut captured = Vec::new();
    let mut buffer = [0_u8; 8 * 1024];
    loop {
        let Ok(read) = pipe.read(&mut buffer) else {
            break;
        };
        if read == 0 {
            break;
        }
        let remaining = MAX_PROBE_OUTPUT_BYTES.saturating_sub(captured.len());
        captured.extend_from_slice(&buffer[..read.min(remaining)]);
    }
    captured
}

pub(crate) fn help_advertises_flag(help: &str, flag: &str) -> bool {
    if flag.is_empty() {
        return false;
    }
    let help = strip_ansi_csi(help);
    help.match_indices(flag).any(|(offset, _)| {
        let before = help[..offset].chars().next_back();
        let after = help[offset + flag.len()..].chars().next();
        before.is_none_or(|character| !is_flag_name_character(character))
            && after.is_none_or(|character| !is_flag_name_character(character))
    })
}

fn strip_ansi_csi(text: &str) -> String {
    let mut stripped = String::with_capacity(text.len());
    let mut characters = text.chars().peekable();
    while let Some(character) = characters.next() {
        if character == '\u{1b}' && characters.peek() == Some(&'[') {
            characters.next();
            for control in characters.by_ref() {
                if ('@'..='~').contains(&control) {
                    break;
                }
            }
        } else {
            stripped.push(character);
        }
    }
    stripped
}

fn is_flag_name_character(character: char) -> bool {
    character.is_ascii_alphanumeric() || matches!(character, '-' | '_')
}

pub(crate) fn canonical_codex_composer_prompt(text: &str, image_count: usize) -> String {
    let mut parts = Vec::with_capacity(image_count + usize::from(!text.is_empty()));
    parts.extend(std::iter::repeat_n("[Image]", image_count));
    if !text.is_empty() {
        parts.push(text);
    }
    canonical_codex_visible_text(&parts.join("\n"), MAX_VISIBLE_TEXT_BYTES)
}

fn canonical_codex_visible_text(text: &str, max_bytes: usize) -> String {
    truncate_utf8(&sanitize_text(text), max_bytes).to_string()
}

fn sanitize_text(text: &str) -> String {
    text.chars()
        .filter(|character| !character.is_control() || matches!(character, '\n' | '\r' | '\t'))
        .collect()
}

fn truncate_utf8(text: &str, max_bytes: usize) -> &str {
    if text.len() <= max_bytes {
        return text;
    }
    let mut end = max_bytes;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

#[cfg(test)]
mod tests {
    use super::*;

    struct DelayedProbePipe {
        completed: std::sync::Arc<std::sync::atomic::AtomicBool>,
    }

    impl std::io::Read for DelayedProbePipe {
        fn read(&mut self, _buffer: &mut [u8]) -> std::io::Result<usize> {
            std::thread::sleep(std::time::Duration::from_millis(25));
            self.completed
                .store(true, std::sync::atomic::Ordering::Release);
            Ok(0)
        }
    }

    fn read_probe_child_pid(path: &Path, timeout: std::time::Duration) -> Option<u32> {
        let started = std::time::Instant::now();
        while started.elapsed() < timeout {
            if let Ok(pid) = std::fs::read_to_string(path) {
                if let Ok(pid) = pid.trim().parse() {
                    return Some(pid);
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        None
    }

    #[test]
    fn timed_out_probe_pipe_reader_is_cancelled_and_joined_before_return() {
        let completed = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let mut reader = spawn_probe_pipe_reader(Some(DelayedProbePipe {
            completed: std::sync::Arc::clone(&completed),
        }));

        let error = receive_probe_pipe(
            &mut reader,
            std::time::Instant::now() + std::time::Duration::from_millis(1),
        )
        .expect_err("a pipe that misses its drain deadline must fail closed");

        assert!(error.contains("exceeded its deadline"));
        assert!(
            completed.load(std::sync::atomic::Ordering::Acquire),
            "the blocking reader actor must finish before the timeout returns"
        );
        assert!(
            reader.thread.is_none(),
            "the reader JoinHandle must be consumed"
        );
    }

    #[test]
    fn capability_probe_reaps_inherited_stdout_grandchild_without_blocking() {
        let unique = format!(
            "devmanager-codex-probe-tree-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let temp = std::env::temp_dir().join(unique);
        std::fs::create_dir_all(&temp).unwrap();
        let pid_path = temp.join("grandchild.pid");

        #[cfg(windows)]
        let (executable, args) = {
            let script_path = temp.join("probe-wrapper.ps1");
            std::fs::write(
                &script_path,
                r#"param([string]$PidPath)
$child = Start-Process -FilePath (Join-Path $PSHOME 'powershell.exe') -ArgumentList @('-NoProfile', '-NonInteractive', '-Command', 'Start-Sleep -Seconds 60') -PassThru -NoNewWindow
[IO.File]::WriteAllText($PidPath, [string]$child.Id)
[Console]::Out.WriteLine('probe-complete')
"#,
            )
            .unwrap();
            (
                resolve_executable("powershell.exe").unwrap(),
                vec![
                    "-NoProfile".to_string(),
                    "-NonInteractive".to_string(),
                    "-File".to_string(),
                    script_path.to_string_lossy().into_owned(),
                    pid_path.to_string_lossy().into_owned(),
                ],
            )
        };
        #[cfg(not(windows))]
        let (executable, args) = (
            PathBuf::from("/bin/sh"),
            vec![
                "-c".to_string(),
                "sleep 60 & child=$!; printf '%s' \"$child\" > \"$1\"; printf 'probe-complete\\n'"
                    .to_string(),
                "probe-wrapper".to_string(),
                pid_path.to_string_lossy().into_owned(),
            ],
        );

        let (result_tx, result_rx) = std::sync::mpsc::channel();
        let started = std::time::Instant::now();
        std::thread::spawn(move || {
            let _ = result_tx.send(run_probe(&executable, &args));
        });
        let result = match result_rx.recv_timeout(std::time::Duration::from_secs(4)) {
            Ok(result) => result,
            Err(error) => {
                if let Some(pid) =
                    read_probe_child_pid(&pid_path, std::time::Duration::from_secs(1))
                {
                    let _ = crate::services::platform_service::kill_process_tree(pid);
                }
                let _ = result_rx.recv_timeout(std::time::Duration::from_secs(2));
                let _ = std::fs::remove_dir_all(&temp);
                panic!("probe remained blocked on inherited stdout: {error}");
            }
        };
        assert!(
            started.elapsed() < std::time::Duration::from_secs(4),
            "probe completion must remain bounded"
        );
        assert!(result.unwrap().contains("probe-complete"));

        let grandchild_pid = read_probe_child_pid(&pid_path, std::time::Duration::from_secs(1))
            .expect("wrapper must record its inherited-stdout grandchild");
        let reaped_at = std::time::Instant::now();
        while crate::services::platform_service::is_pid_running(grandchild_pid)
            && reaped_at.elapsed() < std::time::Duration::from_secs(2)
        {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let still_running = crate::services::platform_service::is_pid_running(grandchild_pid);
        if still_running {
            let _ = crate::services::platform_service::kill_process_tree(grandchild_pid);
        }
        let _ = std::fs::remove_dir_all(&temp);
        assert!(
            !still_running,
            "probe must not leave its grandchild running"
        );
    }

    #[cfg(not(windows))]
    #[test]
    fn capability_probe_force_kills_sigterm_ignoring_grandchild_and_drains_pipes() {
        let unique = format!(
            "devmanager-codex-probe-stubborn-tree-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let temp = std::env::temp_dir().join(unique);
        std::fs::create_dir_all(&temp).unwrap();
        let pid_path = temp.join("grandchild.pid");
        let ready_path = temp.join("grandchild.ready");
        let args = vec![
            "-c".to_string(),
            r#"/bin/sh -c 'trap "" TERM; printf ready > "$1"; while :; do sleep 60; done' probe-child "$2" & child=$!; while [ ! -s "$2" ]; do sleep 1; done; printf '%s' "$child" > "$1"; printf 'probe-complete\n'"#.to_string(),
            "probe-wrapper".to_string(),
            pid_path.to_string_lossy().into_owned(),
            ready_path.to_string_lossy().into_owned(),
        ];

        let started = std::time::Instant::now();
        let result = run_probe(Path::new("/bin/sh"), &args);
        assert!(
            started.elapsed() < std::time::Duration::from_secs(4),
            "stubborn descendants and inherited pipes must remain bounded"
        );
        assert!(result.unwrap().contains("probe-complete"));

        let grandchild_pid = read_probe_child_pid(&pid_path, std::time::Duration::from_secs(1))
            .expect("wrapper must record its SIGTERM-ignoring grandchild");
        let reaped_at = std::time::Instant::now();
        while crate::services::platform_service::is_pid_running(grandchild_pid)
            && reaped_at.elapsed() < std::time::Duration::from_secs(2)
        {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let still_running = crate::services::platform_service::is_pid_running(grandchild_pid);
        if still_running {
            let _ = crate::services::platform_service::kill_process_tree(grandchild_pid);
        }
        let _ = std::fs::remove_dir_all(&temp);
        assert!(
            !still_running,
            "probe must SIGKILL a process-group descendant that ignored SIGTERM"
        );
    }

    #[test]
    fn help_flag_detection_ignores_ansi_csi_styling() {
        assert!(help_advertises_flag(
            "  \u{1b}[1;36m--remote\u{1b}[0m <URL>\n  \u{1b}[32m--dangerously-bypass-hook-trust\u{1b}[0m",
            "--remote"
        ));
        assert!(help_advertises_flag(
            "  \u{1b}[1;36m--remote\u{1b}[0m <URL>\n  \u{1b}[32m--dangerously-bypass-hook-trust\u{1b}[0m",
            "--dangerously-bypass-hook-trust"
        ));
    }

    #[test]
    fn help_flag_detection_accepts_adjacent_punctuation_and_arguments() {
        assert!(help_advertises_flag(
            "Options: [--remote=<URL>], (--listen <URL>).",
            "--remote"
        ));
        assert!(help_advertises_flag(
            "Options: [--remote=<URL>], (--listen <URL>).",
            "--listen"
        ));
    }

    #[test]
    fn help_flag_detection_rejects_near_matches() {
        assert!(!help_advertises_flag(
            "--remote-mode prefix--remote --remote_auth --remote-auth-token-environment",
            "--remote"
        ));
        assert!(!help_advertises_flag(
            "--remote-auth-token-environment",
            "--remote-auth-token-env"
        ));
    }

    #[test]
    fn windows_executable_resolution_prefers_pathext_wrappers_over_shell_shims() {
        let candidates = executable_candidate_names("npx", true, ".EXE;.CMD");

        assert_eq!(candidates.first().map(String::as_str), Some("npx.exe"));
        assert_eq!(candidates.get(1).map(String::as_str), Some("npx.cmd"));
        assert_eq!(candidates.last().map(String::as_str), Some("npx"));
    }

    #[test]
    fn config_override_rejects_unsafe_keys_and_values() {
        assert!(CodexConfigOverride::new("model", "\"o3\"").is_ok());
        assert!(CodexConfigOverride::new("", "\"o3\"").is_err());
        assert!(CodexConfigOverride::new("model; rm", "\"o3\"").is_err());
        assert!(CodexConfigOverride::new("model", "").is_err());
        assert!(CodexConfigOverride::new("model", "a\nb").is_err());
    }

    #[test]
    fn composer_prompt_prefixes_images_and_sanitizes_control_characters() {
        assert_eq!(
            canonical_codex_composer_prompt("fix\u{7}it", 2),
            "[Image]\n[Image]\nfixit"
        );
        assert_eq!(canonical_codex_composer_prompt("", 1), "[Image]");
    }

    #[test]
    fn capability_probe_claims_job_before_explicit_resume() {
        let events = std::cell::RefCell::new(Vec::new());
        let job = claim_then_resume_suspended_child(
            42,
            |pid| {
                events.borrow_mut().push(format!("claim:{pid}"));
                Ok("job")
            },
            |job| {
                events.borrow_mut().push(format!("gate:{job}"));
                Ok(())
            },
            |pid| {
                events.borrow_mut().push(format!("resume:{pid}"));
                Ok(())
            },
        )
        .expect("successful claim must resume");

        assert_eq!(job, "job");
        assert_eq!(
            events.into_inner(),
            ["claim:42", "gate:job", "resume:42"]
        );
    }

    #[test]
    fn capability_probe_claim_failure_does_not_resume() {
        let gated = std::cell::Cell::new(false);
        let resumed = std::cell::Cell::new(false);
        let result = claim_then_resume_suspended_child(
            42,
            |_: u32| -> Result<(), String> { Err("claim failed".to_string()) },
            |_| {
                gated.set(true);
                Ok(())
            },
            |_| {
                resumed.set(true);
                Ok(())
            },
        );

        assert_eq!(
            result,
            Err(SuspendedChildStartup::ClaimFailed("claim failed".to_string()))
        );
        assert!(!gated.get(), "claim failure must not reach the resume gate");
        assert!(!resumed.get(), "claim failure must not resume the child");
    }

    #[test]
    fn capability_probe_resume_failure_retains_job_for_cleanup() {
        let cleaned = std::cell::RefCell::new(None);
        let result = match claim_then_resume_suspended_child(
            42,
            |_| Ok("job"),
            |_| Ok(()),
            |_| Err("resume failed".to_string()),
        ) {
            Ok(job) => Ok(job),
            Err(SuspendedChildStartup::ClaimFailed(error)) => {
                cleaned.replace(None);
                Err(format!(
                    "Cannot own Codex capability probe process tree: {error}"
                ))
            }
            Err(SuspendedChildStartup::AfterClaim { error, job }) => {
                cleaned.replace(Some(job));
                Err(format!(
                    "Cannot resume Codex capability probe process: {error}"
                ))
            }
        };

        assert_eq!(
            result,
            Err("Cannot resume Codex capability probe process: resume failed".to_string())
        );
        assert_eq!(
            cleaned.into_inner(),
            Some("job"),
            "resume failure must pass the exact job into probe-tree cleanup"
        );
    }
}
