//! Host-side plain shell terminals: real spawn, cwd sampling, exit observation.
//!
//! These drive `ProcessManager` directly (no host executor) because the facts
//! the host pump publishes are only as good as what the manager can observe
//! from a real Windows shell.
#![cfg(windows)]

use devmanager::config::paths::{resolve_app_paths, AppProfile, BuildKind, ResolvedAppPaths};
use devmanager::config::{ConfigCommand, ConfigStore, Project};
use devmanager::domain::cockpit::TaskCockpitQuery;
use devmanager::domain::command::{
    Command, CommandEnvelope, CommandReceipt, CreateTaskRequestIntent,
};
use devmanager::domain::id::{CommandId, EnvironmentId, ProjectId, RequestId};
use devmanager::domain::query::{Query, QueryEnvelope, QueryOutcome, QueryReply, QueryResult};
use devmanager::domain::resource::{ResourceLifecycle, TerminalLaunch};
use devmanager::domain::task::{
    ReviewReadiness, TaskActivity, TaskAssignment, TaskAttention, TaskConnectivity,
};
use devmanager::domain::{ClientId, ResourceId, TaskCockpitResult, TaskId};
use devmanager::host::HostRequestExecutor;
use devmanager::kernel::CommandBus;
use devmanager::protocol::{
    Capability, CapabilitySet, ClientRequest, FrameLimits, NegotiatedParameters, ProtocolVersion,
    ServerMessage,
};
use devmanager::services::{pid_file, ProcessManager};
use devmanager::state::SessionDimensions;
use devmanager::terminal::protocol::{CloseReason, TerminalSessionId, TerminalSize, TerminalSpec};
use devmanager::terminal::service::{AttachedTerminalRuntime, TerminalService};
use devmanager::workspace::{WorkspaceProjectRoots, WorkspaceRequest};
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

/// One host executor with a real configured-service runtime, which is what a
/// plain shell needs: `open_shell_terminal_after_accept` refuses outright
/// without one, so a fixture that skipped this would test nothing.
struct ShellHost {
    _base: tempfile::TempDir,
    paths: ResolvedAppPaths,
    requests: devmanager::host::HostRequestHandle,
    executor: devmanager::host::SupervisedHostExecutor,
    negotiated: NegotiatedParameters,
    client_id: ClientId,
    task_id: TaskId,
}

impl ShellHost {
    async fn start(label: &str) -> Self {
        let base = tempfile::tempdir().expect("fixture base");
        let profile = format!("shellterm{}{label}", std::process::id());
        let paths = resolve_app_paths(
            base.path(),
            AppProfile::named(&profile).expect("named profile"),
            BuildKind::Debug,
        )
        .expect("isolated debug paths");
        std::fs::create_dir_all(&paths.root).expect("profile root");

        let configured_id = ProjectId::new().to_string();
        let mut store = ConfigStore::open_host(&paths).expect("host config");
        store
            .execute(
                store.snapshot().revision,
                ConfigCommand::CreateProject {
                    project: Project {
                        id: configured_id.clone(),
                        name: "Shell terminal fixture".to_string(),
                        root_path: paths.root.to_string_lossy().into_owned(),
                        created_at: "now".to_string(),
                        updated_at: "now".to_string(),
                        ..Project::default()
                    },
                },
            )
            .expect("persist project");
        let revision = store.snapshot().revision;
        let roots = WorkspaceProjectRoots::from_host_config_store(&mut store, revision, 1, 1)
            .expect("issue project roots");
        let project_id = roots
            .project_id_for_config_id(&configured_id)
            .expect("opaque project id");

        let bus = CommandBus::open(&paths.database).expect("command store");
        let (requests, executor) =
            HostRequestExecutor::start_supervised_with_config_store(bus, store, &paths.root)
                .expect("configured host executor");

        let client_id = ClientId::new();
        let negotiated = NegotiatedParameters {
            version: ProtocolVersion::current(),
            client_id,
            capabilities: CapabilitySet::from_capabilities([Capability::TaskCockpit]),
            limits: FrameLimits::v1_default(),
        };
        let task_id = TaskId::new();
        let created = requests
            .execute(
                negotiated.clone(),
                ClientRequest::Command(CommandEnvelope {
                    command_id: CommandId::new(),
                    client_id,
                    task_id: None,
                    issued_at_ms: 1_725_000_000_000,
                    expected_task_revision: None,
                    command: Command::CreateTaskV2(CreateTaskRequestIntent {
                        id: task_id,
                        environment_id: EnvironmentId::new(),
                        title: "Shell terminal fixture".into(),
                        description: None,
                        project_id,
                        workspace: WorkspaceRequest::confirmed_external(&paths.root),
                        // No provider: a plain shell must not need one, and this
                        // keeps a provider launch out of the fixture entirely.
                        primary_provider: None,
                        defer_primary_provider_start: true,
                        assignment: TaskAssignment::LocalOwner,
                        created_at_ms: 1_725_000_000_000,
                        connectivity: TaskConnectivity::Connected,
                        attention: TaskAttention::None,
                        activity: TaskActivity::Idle,
                        review_readiness: ReviewReadiness::NotReady,
                    }),
                }),
            )
            .await
            .expect("create task");
        assert!(
            matches!(
                created,
                ServerMessage::CommandReceipt(CommandReceipt::Accepted { .. })
            ),
            "fixture task must be created: {created:?}"
        );

        Self {
            _base: base,
            paths,
            requests,
            executor,
            negotiated,
            client_id,
            task_id,
        }
    }

    /// Kill this host and bring a fresh one up on the same durable store.
    ///
    /// This is the crash: the executor task is aborted mid-flight, so anything
    /// it had begun but not committed is simply gone, and the replacement host
    /// starts with no `shell_sessions` at all. Any `ReleaseResource` row left
    /// behind is one nothing in the process can attribute to a live shell.
    async fn restart(self) -> Self {
        let ShellHost {
            _base,
            paths,
            requests,
            executor,
            negotiated,
            client_id,
            task_id,
        } = self;
        executor.join.abort();
        // Wait for the aborted task to be dropped, not merely cancelled: it owns
        // the previous `ConfigStore`, and the host config refuses a second open
        // while the first handle is alive.
        let _ = executor.join.await;
        drop(requests);
        tokio::task::yield_now().await;

        let store = ConfigStore::open_host(&paths).expect("reopen host config");
        let bus = CommandBus::open(&paths.database).expect("reopen command store");
        let (requests, executor) =
            HostRequestExecutor::start_supervised_with_config_store(bus, store, &paths.root)
                .expect("restarted host executor");
        Self {
            _base,
            paths,
            requests,
            executor,
            negotiated,
            client_id,
            task_id,
        }
    }

    /// The durable facts F10 is about, read through a separate store handle.
    fn shell_release_state(
        &self,
        resource_id: ResourceId,
    ) -> (Option<ResourceLifecycle>, bool, bool) {
        let bus = CommandBus::open(&self.paths.database).expect("reopen store");
        let snapshot = bus
            .task_snapshot(self.task_id)
            .expect("snapshot")
            .expect("task");
        (
            snapshot
                .resources
                .get(&resource_id)
                .map(|resource| resource.lifecycle),
            snapshot.terminal_facts.contains_key(&resource_id),
            snapshot.terminal_strip.order.contains(&resource_id),
        )
    }

    /// Does the Task's own strip still offer a chip for this resource?
    async fn strip_lists(&self, resource_id: ResourceId) -> bool {
        let reply = self
            .requests
            .execute(
                self.negotiated.clone(),
                ClientRequest::Query(QueryEnvelope {
                    request_id: RequestId::new(),
                    client_id: self.client_id,
                    task_id: Some(self.task_id),
                    query: Query::TaskCockpit(TaskCockpitQuery::TaskTerminals),
                }),
            )
            .await
            .expect("task terminals query");
        let ServerMessage::QueryReply(QueryReply { outcome, .. }) = reply else {
            panic!("expected a query reply; got {reply:?}");
        };
        match outcome {
            QueryOutcome::Ok(QueryResult::TaskCockpit(TaskCockpitResult::TaskTerminals(strip))) => {
                strip.order.contains(&resource_id)
                    || strip
                        .terminals
                        .iter()
                        .any(|chip| chip.resource_id == resource_id && !chip.is_provider)
            }
            other => panic!("the strip must be readable on an open task; got {other:?}"),
        }
    }

    async fn close_terminal(&self, resource_id: ResourceId) {
        let receipt = self
            .requests
            .execute(
                self.negotiated.clone(),
                ClientRequest::Command(CommandEnvelope {
                    command_id: CommandId::new(),
                    client_id: self.client_id,
                    task_id: Some(self.task_id),
                    issued_at_ms: 1_725_000_002_000,
                    expected_task_revision: Some(self.task_revision()),
                    command: Command::CloseTerminal { resource_id },
                }),
            )
            .await
            .expect("close the terminal");
        assert!(
            matches!(
                receipt,
                ServerMessage::CommandReceipt(CommandReceipt::Accepted { .. })
            ),
            "CloseTerminal must be accepted: {receipt:?}"
        );
    }

    fn task_revision(&self) -> u64 {
        let bus = CommandBus::open(&self.paths.database).expect("reopen store");
        bus.task_snapshot(self.task_id)
            .expect("snapshot")
            .expect("task")
            .task
            .revision
    }

    /// Open one plain shell and answer with its durable resource id.
    ///
    /// A refusal is returned rather than asserted so the caller can skip on a
    /// machine with no resolvable shell instead of reporting a red test.
    async fn open_shell(&self) -> Result<ResourceId, String> {
        let request_id = RequestId::new();
        let reply = self
            .requests
            .execute(
                self.negotiated.clone(),
                ClientRequest::Query(QueryEnvelope {
                    request_id,
                    client_id: self.client_id,
                    task_id: Some(self.task_id),
                    query: Query::TaskCockpit(TaskCockpitQuery::OpenShellTerminal {
                        cwd: None,
                        expected_task_revision: self.task_revision(),
                    }),
                }),
            )
            .await
            .expect("open shell query");
        let ServerMessage::QueryReply(QueryReply { outcome, .. }) = reply else {
            panic!("expected a query reply; got {reply:?}");
        };
        match outcome {
            QueryOutcome::Ok(QueryResult::TaskCockpit(TaskCockpitResult::TaskTerminals(strip))) => {
                let resource_id = *strip
                    .order
                    .first()
                    .expect("an opened shell is the strip's first entry");
                Ok(resource_id)
            }
            other => Err(format!("{other:?}")),
        }
    }
}

/// The pids this host has spawned, under the isolated pid file.
///
/// Only ever used to FIND the shell, never to decide it has gone: archiving
/// untracks the session from the ledger, and after that this answers `[]` for
/// a shell that is still running. Measured 2026-09-02 against a host stripped
/// of both halves of this fix -- the record vanished within 200 ms while the
/// process lived on -- which is exactly the "an empty result means it is gone"
/// conflation. Liveness comes from [`shell_is_running`] instead.
fn live_shell_pids() -> Vec<u32> {
    pid_file::active_tracked_processes()
        .into_iter()
        .filter(|record| record.session_id.starts_with("shell-"))
        .map(|record| record.pid)
        .collect()
}

/// Is this exact process still alive, according to the operating system?
fn shell_is_running(pid: u32) -> bool {
    devmanager::services::platform_service::is_pid_running(pid)
}

/// Poll until a condition has held on `consecutive` successive reads.
///
/// One reading is not evidence when the thing being read can answer wrongly
/// for a moment, and both oracles here can: the pid ledger is a file that is
/// briefly empty while it is rewritten, and a pid can in principle be reused.
/// Requiring the answer to repeat is what keeps a transient from being taken
/// for a result.
async fn wait_until_stable(
    deadline: Duration,
    consecutive: u32,
    mut ready: impl FnMut() -> bool,
) -> bool {
    let end = Instant::now() + deadline;
    let mut streak = 0;
    loop {
        streak = if ready() { streak + 1 } else { 0 };
        if streak >= consecutive {
            return true;
        }
        if Instant::now() >= end {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// Poll for a condition while YIELDING to the executor.
///
/// `std::thread::sleep` here would be silently wrong: these tests run the host
/// on a current-thread runtime, so a blocking sleep starves the executor task
/// and the reaper tick that drives reconciliation never happens. The failure
/// looks exactly like the sweep not working.
async fn wait_until(deadline: Duration, mut ready: impl FnMut() -> bool) -> bool {
    let end = Instant::now() + deadline;
    loop {
        if ready() {
            return true;
        }
        if Instant::now() >= end {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Archiving a task must leave no plain shell of its running.
///
/// `Command::BeginCloseTask` releases every task-owned resource through
/// `ReleaseResource` + `settle_next_resource_release`, which is process-empty
/// for a plain shell by construction: nothing in that path touches a PTY. The
/// release path therefore closes each plain shell itself.
///
/// Read this as an end-to-end regression guard, NOT as an attribution. Three
/// things were measured while writing it, on 2026-09-02, and each one weakens
/// what a green here can be taken to mean:
///
/// 1. A host with neither half of the fix still ends up clean a second or two
///    later. A real shell prints a prompt, the fact pump offers an activity
///    fact, the release has already dropped the durable terminal, and the
///    resulting `UnknownTerminal` outcome closes the shell. That net is real
///    but conditional: it needs the shell to produce output or settle a cwd.
/// 2. The pid ledger is not a liveness oracle -- archiving untracks the
///    session, after which it reports the shell gone whether or not it is --
///    so liveness is asked of the operating system by pid instead.
/// 3. `TaskCockpitQuery::TerminalFor` cannot stand in for "the hosted terminal
///    was retired": on an archived task it answers `Denied(StaleFence)`
///    whether the entry survives or not. There is no wire-visible observable
///    for that half, so it is covered by construction and by
///    `host::connection::tests::only_a_shell_with_no_durable_terminal_left_is_swept`.
///
/// The control is what keeps the assertion able to fail at all: the shell is
/// proved to be a live operating-system process before archiving.
#[test]
fn archiving_a_task_closes_the_plain_shells_it_still_owns() {
    let _pid_guard = use_isolated_pid_file("archive-closes-shells");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime");
    runtime.block_on(async {
        let host = ShellHost::start("archive").await;
        let resource_id = match host.open_shell().await {
            Ok(resource_id) => resource_id,
            Err(refusal) => {
                println!(
                    "SKIPPED archiving_a_task_closes_the_plain_shells_it_still_owns: \
                     no shell could be opened on this machine: {refusal}"
                );
                return;
            }
        };

        assert!(
            wait_until(Duration::from_secs(30), || !live_shell_pids().is_empty()).await,
            "the fixture shell never started, so nothing here could observe it being closed"
        );
        let shell_pid = live_shell_pids()[0];
        assert!(
            shell_is_running(shell_pid),
            "the fixture shell {shell_pid} is not a live process, so the assertion below \
             could not fail"
        );

        let receipt = host
            .requests
            .execute(
                host.negotiated.clone(),
                ClientRequest::Command(CommandEnvelope {
                    command_id: CommandId::new(),
                    client_id: host.client_id,
                    task_id: Some(host.task_id),
                    issued_at_ms: 1_725_000_001_000,
                    expected_task_revision: Some(host.task_revision()),
                    command: Command::BeginCloseTask,
                }),
            )
            .await
            .expect("archive the task");
        assert!(
            matches!(
                receipt,
                ServerMessage::CommandReceipt(CommandReceipt::Accepted { .. })
            ),
            "archive must be accepted: {receipt:?}"
        );

        assert!(
            wait_until_stable(Duration::from_secs(30), 3, || !shell_is_running(shell_pid)).await,
            "archiving task {} left shell {resource_id} running as process {shell_pid}",
            host.task_id
        );
    });
}

/// Closing a shell must settle its release, not leave a `?` chip forever.
///
/// `Command::CloseTerminal` emits only `ResourceReleaseBegun` and enqueues a
/// `ReleaseResource` outbox row. Until F10 the only claimant of that
/// destination class was `settle_next_resource_release`, whose only host caller
/// is the task-close/archive loop -- so a shell closed any other way kept
/// lifecycle `Releasing`, its `terminal_facts` row and its strip entry
/// indefinitely, and the client rendered a muted "?" chip that never went away.
///
/// All four consequences are asserted, because each one is separately visible
/// to a user and a fix that moved only the lifecycle would look right in a
/// debugger and wrong on screen. The pre-close control is what makes them able
/// to fail.
#[test]
fn closing_a_shell_settles_its_release_and_drops_it_from_the_strip() {
    let _pid_guard = use_isolated_pid_file("close-settles-release");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime");
    runtime.block_on(async {
        let host = ShellHost::start("closesettle").await;
        let resource_id = match host.open_shell().await {
            Ok(resource_id) => resource_id,
            Err(refusal) => {
                println!(
                    "SKIPPED closing_a_shell_settles_its_release_and_drops_it_from_the_strip: \
                     no shell could be opened on this machine: {refusal}"
                );
                return;
            }
        };

        // Control: everything the assertions below look at is present first.
        assert_eq!(
            host.shell_release_state(resource_id),
            (Some(ResourceLifecycle::Active), true, true),
            "an open shell must be Active, carry durable facts and sit in the strip"
        );
        assert!(
            host.strip_lists(resource_id).await,
            "an open shell must have a chip, or its disappearance proves nothing"
        );

        host.close_terminal(resource_id).await;

        assert!(
            wait_until(Duration::from_secs(30), || host
                .shell_release_state(resource_id)
                .0
                == Some(ResourceLifecycle::Released))
            .await,
            "closing left the resource at {:?}; nothing else claims a ReleaseResource row \
             outside the archive loop, so it stays there forever",
            host.shell_release_state(resource_id).0
        );
        let (lifecycle, has_facts, in_order) = host.shell_release_state(resource_id);
        assert_eq!(lifecycle, Some(ResourceLifecycle::Released));
        assert!(
            !has_facts,
            "a released shell must not keep its terminal facts"
        );
        assert!(!in_order, "a released shell must not keep its strip slot");
        assert!(
            !host.strip_lists(resource_id).await,
            "a released shell must not still be offered as a chip"
        );
    });
}

/// A host that dies between the teardown and the settle converges on boot.
///
/// The close and the settle are two durable steps and nothing makes them
/// atomic, so the row has to be recoverable by a host that was not there when
/// it was written. The replacement host holds no `shell_sessions` at all, which
/// is exactly the condition that licenses settling: a shell it is still running
/// is one it has not torn down, and settling that release would publish
/// `Released` for a live PTY.
#[test]
fn a_release_left_by_a_dead_host_is_settled_on_the_next_maintenance_tick() {
    let _pid_guard = use_isolated_pid_file("converge-orphan-release");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime");
    runtime.block_on(async {
        let host = ShellHost::start("converge").await;
        let resource_id = match host.open_shell().await {
            Ok(resource_id) => resource_id,
            Err(refusal) => {
                println!(
                    "SKIPPED a_release_left_by_a_dead_host_is_settled_on_the_next_maintenance_tick: \
                     no shell could be opened on this machine: {refusal}"
                );
                return;
            }
        };

        // Begin the release WITHOUT the host performing its close: a second
        // store handle writes the same durable step `CloseTerminal` does, which
        // is the state a crash between the two halves leaves behind.
        {
            let mut bus = CommandBus::open(&host.paths.database).expect("second store handle");
            let revision = bus
                .task_snapshot(host.task_id)
                .expect("snapshot")
                .expect("task")
                .task
                .revision;
            let receipt = bus
                .execute(CommandEnvelope {
                    command_id: CommandId::new(),
                    client_id: host.client_id,
                    task_id: Some(host.task_id),
                    issued_at_ms: 1_725_000_003_000,
                    expected_task_revision: Some(revision),
                    command: Command::CloseTerminal { resource_id },
                })
                .expect("begin the release behind the host's back");
            assert!(
                matches!(receipt, CommandReceipt::Accepted { .. }),
                "the durable close must be accepted: {receipt:?}"
            );
        }
        assert_eq!(
            host.shell_release_state(resource_id).0,
            Some(ResourceLifecycle::Releasing),
            "the fixture must actually leave a pending release, or this proves nothing"
        );

        // The ORIGINAL host still holds a live session for this shell, so it
        // must NOT settle: that is the guard that keeps convergence from
        // publishing Released for a running PTY.
        for _ in 0..4 {
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        assert_eq!(
            host.shell_release_state(resource_id).0,
            Some(ResourceLifecycle::Releasing),
            "a host that still holds the shell session must leave its release alone"
        );

        // Now the crash: the session goes with the host.
        let host = host.restart().await;
        assert!(
            wait_until(Duration::from_secs(30), || host.shell_release_state(resource_id).0
                == Some(ResourceLifecycle::Released))
            .await,
            "a restarted host left the release at {:?}; nothing else ever claims that row",
            host.shell_release_state(resource_id).0
        );
        let (_, has_facts, in_order) = host.shell_release_state(resource_id);
        assert!(!has_facts, "convergence must retire the terminal facts too");
        assert!(!in_order, "convergence must free the strip slot too");
    });
}
