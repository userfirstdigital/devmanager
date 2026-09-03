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
use devmanager::terminal::protocol::{CloseReason, TerminalSessionId, TerminalSize, TerminalSpec};
use devmanager::terminal::service::{AttachedTerminalRuntime, TerminalService};
use std::path::{Path, PathBuf};
use std::sync::Arc;
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
    let guard = ShellGuard {
        manager: &manager,
        session_id,
    };
    assert!(manager.task_shell_runtime(session_id).is_ok());

    manager
        .close_task_shell_session(session_id)
        .expect("close the shell");
    drop(guard);

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

/// Closing a shell must retire both halves, in the order the host uses.
///
/// The hosted view is closed first so clients see the Exit delta, and the
/// manager session is closed after. `close_managed_process_exact` sets
/// `retired` instead of clearing the teardown slot, so the manager still finds
/// the fence it requires and takes its idempotent path -- neither half is left
/// `Failed`, `reap_incomplete`, or in its owner's map.
#[test]
fn closing_a_shell_retires_the_hosted_view_then_the_manager_session() {
    let _pid_guard = use_isolated_pid_file("retire-both-halves");
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
    let guard = ShellGuard {
        manager: &manager,
        session_id,
    };

    let service = TerminalService::new();
    let runtime: Arc<dyn AttachedTerminalRuntime> =
        manager.task_shell_runtime(session_id).expect("runtime");
    let spec =
        TerminalSpec::new(session_id, TerminalSize::new(100, 30).expect("size")).expect("spec");
    service
        .attach_plain_shell(task_id, resource_id, 1, spec, runtime)
        .expect("attach the plain shell");
    let terminal_id = service
        .shell_terminal_id(resource_id)
        .expect("lookup")
        .expect("an attached shell is addressable by its resource");

    // The activity gate reads this sequence. Prove it can move at all before
    // trusting a gate built on it: a real shell prints a prompt, so a sequence
    // that never advances would mean the gate could never fire.
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let sequence = service
            .shell_output_sequence(resource_id)
            .expect("sequence for a live shell");
        if sequence > 0 {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the hosted delta sequence never advanced for a real shell"
        );
        std::thread::sleep(Duration::from_millis(100));
    }

    service
        .close(terminal_id, CloseReason::ExplicitServiceClose)
        .expect("the hosted view closes through its verified fence");
    assert!(
        service
            .shell_terminal_id(resource_id)
            .expect("lookup")
            .is_none(),
        "a closed view must not still be addressable"
    );

    manager
        .close_task_shell_session(session_id)
        .expect("the manager close must stay idempotent after the view teardown");
    assert!(
        manager.task_shell_runtime(session_id).is_err(),
        "the manager must drop the session it no longer owns"
    );
    let key = format!("shell-{session_id}");
    if let Some(state) = manager.runtime_state().sessions.get(&key) {
        assert!(
            !state.reap_incomplete,
            "the second teardown must not report an incomplete reap: {:?}",
            state.exit
        );
        assert_ne!(
            state.status,
            devmanager::state::SessionStatus::Failed,
            "neither half may leave the session Failed: {:?}",
            state.exit
        );
    }
    drop(guard);
}

/// The PowerShell prompt hook moves `shell_session_cwd` for a real managed shell.
///
/// Everything the live-cwd ladder claims about the default Windows shell rests
/// on this. Neither rung can be trusted from reading: PowerShell moves its own
/// location on `Set-Location` and leaves its Win32 current directory at the
/// launch directory (measured on PowerShell 7.6.5, 2026-09-02), so a shell with
/// no hook reports the directory it started in forever.
///
/// The unhooked control is the point of the test. Without it a green here could
/// be satisfied by the launch directory happening to equal the target, or by a
/// future pwsh that updates its own PEB, and the hook could then be deleted
/// without this file noticing.
///
/// The gate is whether an UNHOOKED `pwsh` starts under the managed launcher,
/// not whether `pwsh` exists: on a machine where it cannot start at all there
/// is nothing here to measure, but a machine where the plain one starts and the
/// hooked one does not is the hook breaking the launch, which must be red.
/// Measured 2026-09-02 on this developer machine: MSIX `pwsh` 7.6.5 cannot be
/// started through the managed launcher by either spelling on PATH, so this
/// test SKIPS here and the hook is proved instead by
/// `terminal::session::tests::a_live_pwsh_prompt_hook_reports_the_directory_it_moved_to`,
/// which runs the same hook in a real `pwsh` without the managed launcher.
#[test]
fn a_pwsh_task_shell_reports_its_cwd_only_with_the_prompt_hook() {
    let Some(program) = devmanager::diagnostics::resolve::resolve_all("pwsh")
        .into_iter()
        .next()
    else {
        println!(
            "SKIPPED a_pwsh_task_shell_reports_its_cwd_only_with_the_prompt_hook: \
             pwsh is not resolvable on PATH"
        );
        return;
    };
    let _pid_guard = use_isolated_pid_file("pwsh-prompt-hook");
    let manager = ProcessManager::new();
    let workdir = tempfile::tempdir().expect("workdir");
    let sub = workdir.path().join("sub");
    std::fs::create_dir(&sub).expect("mkdir");

    let spawn = |args: Vec<String>| {
        let launch = TerminalLaunch {
            cwd: workdir.path().to_path_buf(),
            program: program.clone(),
            args,
        };
        manager.spawn_task_shell_session(
            TaskId::new(),
            ResourceId::new(),
            1,
            1,
            &launch,
            dimensions(),
        )
    };

    let plain = match spawn(devmanager::terminal::session::pwsh_shell_args(false)) {
        Ok(session_id) => session_id,
        Err(error) => {
            println!(
                "SKIPPED a_pwsh_task_shell_reports_its_cwd_only_with_the_prompt_hook: \
                 an unhooked pwsh cannot start under the managed launcher on this machine \
                 ({}): {error}",
                program.display()
            );
            return;
        }
    };
    let _plain_guard = ShellGuard {
        manager: &manager,
        session_id: plain,
    };
    // The plain one started, so a refusal here is the hook's argument, not the
    // machine. That is a failure, never a skip.
    let hooked = spawn(devmanager::terminal::session::pwsh_shell_args(true))
        .expect("the prompt hook must not stop pwsh from starting");
    let _hooked_guard = ShellGuard {
        manager: &manager,
        session_id: hooked,
    };

    for session_id in [hooked, plain] {
        manager
            .task_shell_runtime(session_id)
            .expect("runtime")
            .write_bytes(b"Set-Location sub\r\n")
            .expect("Set-Location");
    }

    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        if manager
            .shell_session_cwd(hooked)
            .is_some_and(|observed| same_directory(&observed, &sub))
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the hooked pwsh never reported {sub:?}; observed {:?}",
            manager.shell_session_cwd(hooked)
        );
        std::thread::sleep(Duration::from_millis(200));
    }

    // The hooked shell has reported, so the unhooked one has had at least as
    // long to do the same. It cannot, and that is why the hook exists.
    let unhooked = manager
        .shell_session_cwd(plain)
        .expect("a live shell always answers with some directory");
    assert!(
        !same_directory(&unhooked, &sub),
        "an unhooked pwsh reported {unhooked:?} after Set-Location, so this test \
         would pass with the prompt hook removed"
    );
    assert!(
        same_directory(&unhooked, workdir.path()),
        "an unhooked pwsh must still report its launch directory, not nothing: {unhooked:?}"
    );
}
