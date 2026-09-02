//! Host-side plain shell terminals: real spawn, cwd sampling, exit observation.
//!
//! These drive `ProcessManager` directly (no host executor) because the facts
//! the host pump publishes are only as good as what the manager can observe
//! from a real Windows shell.
#![cfg(windows)]

use devmanager::domain::resource::TerminalLaunch;
use devmanager::domain::{ResourceId, TaskId};
use devmanager::services::{pid_file, ProcessManager};
use devmanager::state::SessionDimensions;
use devmanager::terminal::protocol::TerminalSessionId;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const CMD_EXE: &str = r"C:\Windows\System32\cmd.exe";

fn use_isolated_pid_file(label: &str) -> pid_file::TestPidFileGuard {
    let path = std::env::temp_dir().join(format!(
        "devmanager-task-shell-{label}-{}.json",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&path);
    pid_file::use_test_pid_file(path)
}

fn cmd_launch(cwd: PathBuf) -> TerminalLaunch {
    TerminalLaunch {
        cwd,
        program: PathBuf::from(CMD_EXE),
        args: vec!["/Q".to_string()],
    }
}

fn dimensions() -> SessionDimensions {
    SessionDimensions {
        cols: 100,
        rows: 30,
        cell_width: 8,
        cell_height: 16,
    }
}

/// Every spawned shell is a real process. Close it even when an assertion
/// unwinds, or the run leaves orphaned `cmd.exe` behind.
struct ShellGuard<'a> {
    manager: &'a ProcessManager,
    session_id: TerminalSessionId,
}

impl Drop for ShellGuard<'_> {
    fn drop(&mut self) {
        let _ = self.manager.close_task_shell_session(self.session_id);
    }
}

fn same_directory(left: &Path, right: &Path) -> bool {
    match (left.canonicalize(), right.canonicalize()) {
        (Ok(left), Ok(right)) => left == right,
        _ => left == right,
    }
}

#[test]
fn task_shell_spawns_reports_cwd_and_exit() {
    let _pid_guard = use_isolated_pid_file("spawn-cwd-exit");
    let manager = ProcessManager::new();
    let workdir = tempfile::tempdir().expect("workdir");
    let task_id = TaskId::new();
    let resource_id = ResourceId::new();
    let session_id = manager
        .spawn_task_shell_session(
            task_id,
            resource_id,
            1,
            1,
            &cmd_launch(workdir.path().to_path_buf()),
            dimensions(),
        )
        .expect("spawn");
    let _guard = ShellGuard {
        manager: &manager,
        session_id,
    };

    let runtime = manager.task_shell_runtime(session_id).expect("runtime");

    // The attachment fence the runtime presents is what TerminalService
    // verifies in `attach_plain_shell`; a mismatch there is a silent refusal.
    let fence = runtime
        .current_attachment_fence()
        .expect("attachment fence is published at spawn");
    assert_eq!(fence, (resource_id, 1));

    let sub = workdir.path().join("sub");
    std::fs::create_dir(&sub).expect("mkdir");
    runtime.write_bytes(b"cd sub\r\n").expect("cd");

    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if manager
            .shell_session_cwd(session_id)
            .is_some_and(|observed| same_directory(&observed, &sub))
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "cwd never reported: {:?}",
            manager.shell_session_cwd(session_id)
        );
        std::thread::sleep(Duration::from_millis(100));
    }

    runtime.write_bytes(b"exit 3\r\n").expect("exit");
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if let Some((code, summary)) = manager.shell_session_exit(session_id) {
            assert_eq!(code, Some(3), "summary: {summary}");
            break;
        }
        assert!(Instant::now() < deadline, "exit never observed");
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[test]
fn task_shell_reports_its_launch_directory_before_any_command() {
    let _pid_guard = use_isolated_pid_file("launch-cwd");
    let manager = ProcessManager::new();
    let workdir = tempfile::tempdir().expect("workdir");
    let session_id = manager
        .spawn_task_shell_session(
            TaskId::new(),
            ResourceId::new(),
            1,
            1,
            &cmd_launch(workdir.path().to_path_buf()),
            dimensions(),
        )
        .expect("spawn");
    let _guard = ShellGuard {
        manager: &manager,
        session_id,
    };

    let observed = manager.shell_session_cwd(session_id).expect("launch cwd");
    assert!(observed.is_absolute(), "cwd facts must be absolute");
    assert!(
        same_directory(&observed, workdir.path()),
        "{observed:?} is not the launch directory {:?}",
        workdir.path()
    );
}

#[test]
fn closing_a_task_shell_drops_the_session_and_its_runtime() {
    let _pid_guard = use_isolated_pid_file("close");
    let manager = ProcessManager::new();
    let workdir = tempfile::tempdir().expect("workdir");
    let session_id = manager
        .spawn_task_shell_session(
            TaskId::new(),
            ResourceId::new(),
            1,
            1,
            &cmd_launch(workdir.path().to_path_buf()),
            dimensions(),
        )
        .expect("spawn");
    assert!(manager.task_shell_runtime(session_id).is_ok());

    manager
        .close_task_shell_session(session_id)
        .expect("close the shell");

    assert!(
        manager.task_shell_runtime(session_id).is_err(),
        "a closed shell must not remain addressable"
    );
    // Closing twice is how the host reacts to an UnknownTerminal outcome after
    // the resource was released under it; it must not error.
    manager
        .close_task_shell_session(session_id)
        .expect("closing an already-closed shell is a no-op");
}
