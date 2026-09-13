//! Linux process-tree custody. One dedicated tracer thread owns all wait and
//! ptrace calls for a launch. The first exec stop precedes any provider code;
//! fork, vfork and clone descendants remain traced even after setsid.
//!
//! This is lifecycle ownership, not a sandbox. No syscall-by-syscall tracing
//! is used. Only this tracer thread's children are waited, never global PIDs.

use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::process::{Child, Command, ExitStatus};
use std::sync::{mpsc, Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use super::identity::{ManagedProcessId, ManagedProcessIdentity};

const MAX_TRACEES: usize = 4096;
const POLL: Duration = Duration::from_millis(5);
const CLEANUP: Duration = Duration::from_secs(5);
const OPTIONS: libc::c_ulong = (libc::PTRACE_O_TRACEFORK
    | libc::PTRACE_O_TRACEVFORK
    | libc::PTRACE_O_TRACECLONE
    | libc::PTRACE_O_TRACEEXEC
    | libc::PTRACE_O_EXITKILL) as libc::c_ulong;

#[derive(Debug)]
struct Member {
    identity: ManagedProcessIdentity,
    process: Arc<OwnedFd>,
    initial_stop: bool,
}

#[derive(Debug, Default)]
struct State {
    members: BTreeMap<i32, Member>,
    pending_fork_notifications: BTreeSet<i32>,
    resume_requested: bool,
    resumed: bool,
    stopping: bool,
    finished: bool,
    root_status: Option<ExitStatus>,
    failure: Option<String>,
}

#[derive(Debug, Default)]
struct Shared {
    state: Mutex<State>,
    changed: Condvar,
}

/// Read/control handle. It does not own the tracer join; the registry's tree
/// owner must remain alive until every process and the tracer have exited.
#[derive(Clone, Debug)]
pub(crate) struct LinuxProcessControl(Arc<Shared>);

impl LinuxProcessControl {
    pub(crate) fn resume(&self, deadline: Instant) -> Result<(), String> {
        let mut state = self
            .0
            .state
            .lock()
            .map_err(|_| "Linux process state poisoned")?;
        if state.resume_requested || state.stopping || state.finished {
            return Err("Linux launch is no longer at its one-way exec barrier".into());
        }
        state.resume_requested = true;
        self.0.changed.notify_all();
        while !state.resumed && !state.finished && state.failure.is_none() {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                state.stopping = true;
                self.0.changed.notify_all();
                return Err("Linux process resume deadline expired".into());
            }
            state = self
                .0
                .changed
                .wait_timeout(state, remaining)
                .map_err(|_| "Linux process state poisoned")?
                .0;
        }
        if let Some(error) = &state.failure {
            return Err(error.clone());
        }
        if !state.resumed {
            return Err("Linux process exited before resume".into());
        }
        Ok(())
    }

    pub(crate) fn terminate(&self) -> Result<(), String> {
        let mut state = self
            .0
            .state
            .lock()
            .map_err(|_| "Linux process state poisoned")?;
        state.stopping = true;
        self.0.changed.notify_all();
        Ok(())
    }

    pub(crate) fn root_status(&self) -> io::Result<Option<ExitStatus>> {
        let state = self
            .0
            .state
            .lock()
            .map_err(|_| io::Error::other("Linux process state poisoned"))?;
        if let Some(status) = state.root_status {
            return Ok(Some(status));
        }
        if let Some(error) = &state.failure {
            return Err(io::Error::other(error.clone()));
        }
        Ok(None)
    }

    pub(crate) fn members(&self, deadline: Instant) -> Result<Vec<ManagedProcessIdentity>, String> {
        if Instant::now() >= deadline {
            return Err("Linux membership deadline expired".into());
        }
        let state = self
            .0
            .state
            .lock()
            .map_err(|_| "Linux process state poisoned")?;
        if let Some(error) = &state.failure {
            return Err(error.clone());
        }
        let mut processes = BTreeMap::new();
        for member in state.members.values() {
            processes.insert(member.identity.id().pid(), member.identity.clone());
        }
        if Instant::now() >= deadline {
            return Err("Linux membership deadline expired".into());
        }
        Ok(processes.into_values().collect())
    }

    pub(crate) fn wait_empty(&self, deadline: Instant) -> Result<bool, String> {
        let mut state = self
            .0
            .state
            .lock()
            .map_err(|_| "Linux process state poisoned")?;
        loop {
            if state.finished {
                if let Some(error) = &state.failure {
                    return Err(error.clone());
                }
                return Ok(state.members.is_empty());
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Ok(false);
            }
            state = self
                .0
                .changed
                .wait_timeout(state, remaining)
                .map_err(|_| "Linux process state poisoned")?
                .0;
        }
    }
}

#[derive(Debug)]
pub(crate) struct LinuxProcessTree {
    control: LinuxProcessControl,
    thread: Option<JoinHandle<()>>,
}

impl LinuxProcessTree {
    pub(crate) fn spawn(mut command: Command, deadline: Instant) -> Result<(Child, Self), String> {
        // Do not raise SIGSTOP here: std::Command::spawn waits for the exec
        // error pipe to close. TRACEME alone produces the post-exec SIGTRAP
        // after the pipe closes, before the new program executes user code.
        let parent_pid = std::process::id() as libc::pid_t;
        unsafe {
            command.pre_exec(move || {
                // EXITKILL is installed at the first exec stop. Cover the
                // earlier fork-to-exec interval too, including a parent exit
                // racing prctl itself. This thread remains the lifetime owner.
                if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) < 0 {
                    return Err(io::Error::last_os_error());
                }
                if libc::getppid() != parent_pid {
                    return Err(io::Error::from_raw_os_error(libc::ESRCH));
                }
                if libc::ptrace(libc::PTRACE_TRACEME, 0, 0, 0) < 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let shared = Arc::new(Shared::default());
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let mut owner = Self {
            control: LinuxProcessControl(shared.clone()),
            thread: None,
        };
        owner.thread = Some(
            std::thread::Builder::new()
                .name("linux-process-owner".into())
                .spawn(move || run(command, deadline, shared, ready_tx))
                .map_err(|error| format!("start Linux process owner: {error}"))?,
        );
        let child = ready_rx
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .map_err(|_| "Linux process did not reach its exec barrier before the deadline")??;
        Ok((child, owner))
    }

    pub(crate) fn control(&self) -> LinuxProcessControl {
        self.control.clone()
    }

    pub(crate) fn join(&mut self, deadline: Instant) -> Result<(), String> {
        if !self.control.wait_empty(deadline)? {
            return Err("Linux process tree remains live".into());
        }
        if let Some(thread) = self.thread.take() {
            thread.join().map_err(|_| "Linux process owner panicked")?;
        }
        Ok(())
    }
}

impl Drop for LinuxProcessTree {
    fn drop(&mut self) {
        let _ = self.control.terminate();
        // The owner never acquires a ProcessManager/registry/terminal lock.
        // Joining it while those locks are held cannot deadlock the waiter.
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn ptrace(request: libc::c_uint, pid: i32, data: usize) -> io::Result<()> {
    if unsafe { libc::ptrace(request, pid, 0, data) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn event_message(pid: i32) -> io::Result<i32> {
    let mut value: libc::c_ulong = 0;
    ptrace(libc::PTRACE_GETEVENTMSG, pid, &mut value as *mut _ as usize)?;
    i32::try_from(value).map_err(|_| io::Error::other("invalid ptrace event PID"))
}

fn inspect(tid: i32) -> io::Result<Member> {
    let status = std::fs::read_to_string(format!("/proc/{tid}/status"))?;
    let pid: u32 = status
        .lines()
        .find_map(|line| {
            line.strip_prefix("Tgid:")
                .and_then(|value| value.trim().parse().ok())
        })
        .ok_or_else(|| io::Error::other("missing traced process group identity"))?;
    let raw = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
    if raw < 0 {
        return Err(io::Error::last_os_error());
    }
    let process = unsafe { OwnedFd::from_raw_fd(raw as i32) };
    let start = crate::services::platform_service::capture_process_creation_time_100ns(pid)
        .ok_or_else(|| io::Error::other("missing traced process creation identity"))?;
    let executable = std::fs::read_link(format!("/proc/{pid}/exe"))?;
    let id = ManagedProcessId::new(pid, start).map_err(io::Error::other)?;
    let identity = ManagedProcessIdentity::new(id, executable).map_err(io::Error::other)?;
    Ok(Member {
        identity,
        process: Arc::new(process),
        initial_stop: false,
    })
}

fn kill_member(member: &Member) -> io::Result<()> {
    let result = unsafe {
        libc::syscall(
            libc::SYS_pidfd_send_signal,
            member.process.as_raw_fd(),
            libc::SIGKILL,
            std::ptr::null::<libc::siginfo_t>(),
            0,
        )
    };
    if result < 0 {
        let error = io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::ESRCH) {
            return Err(error);
        }
    }
    Ok(())
}

fn run(
    command: Command,
    deadline: Instant,
    shared: Arc<Shared>,
    ready: mpsc::SyncSender<Result<Child, String>>,
) {
    let mut command = command;
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            let _ = ready.send(Err(format!("spawn Linux process: {error}")));
            shared.state.lock().unwrap().finished = true;
            shared.changed.notify_all();
            return;
        }
    };
    let root = child.id() as i32;
    let mut root_reaped = false;
    let initialized = (|| -> io::Result<Member> {
        loop {
            let mut status = 0;
            let waited = unsafe {
                libc::waitpid(
                    root,
                    &mut status,
                    libc::WNOHANG | libc::__WALL | libc::__WNOTHREAD,
                )
            };
            if waited < 0 {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(error);
            }
            if waited == root {
                root_reaped = libc::WIFEXITED(status) || libc::WIFSIGNALED(status);
                if !libc::WIFSTOPPED(status) || libc::WSTOPSIG(status) != libc::SIGTRAP {
                    return Err(io::Error::other(
                        "Linux launch did not reach its exact exec stop",
                    ));
                }
                ptrace(libc::PTRACE_SETOPTIONS, root, OPTIONS as usize)?;
                return inspect(root);
            }
            if Instant::now() >= deadline {
                return Err(io::Error::other("Linux exec barrier deadline expired"));
            }
            std::thread::sleep(POLL);
        }
    })();
    match initialized {
        Ok(member) => {
            shared.state.lock().unwrap().members.insert(root, member);
        }
        Err(error) => {
            // No provider instruction has been resumed, so there can be no
            // descendants. Child still owns this unreaped root's identity.
            if !root_reaped {
                let _ = child.kill();
                let _ = child.wait();
            }
            let _ = ready.send(Err(format!("Linux exec barrier: {error}")));
            shared.state.lock().unwrap().finished = true;
            shared.changed.notify_all();
            return;
        }
    }
    if ready.send(Ok(child)).is_err() {
        shared.state.lock().unwrap().stopping = true;
    }
    let mut stop_deadline = None;
    loop {
        let mut state = shared.state.lock().unwrap();
        if state.stopping {
            let end = *stop_deadline.get_or_insert_with(|| Instant::now() + CLEANUP);
            for member in state.members.values() {
                let _ = kill_member(member);
            }
            if Instant::now() >= end {
                state
                    .failure
                    .get_or_insert("Linux process cleanup deadline expired".into());
                // EXITKILL remains installed; tracer exit never detaches a
                // remaining live process. The non-empty/error state prevents
                // callers from publishing a successful resource release.
                state.finished = true;
                shared.changed.notify_all();
                return;
            }
        } else if state.resume_requested && !state.resumed {
            match ptrace(libc::PTRACE_CONT, root, 0) {
                Ok(()) => state.resumed = true,
                Err(error) => {
                    state.failure = Some(format!("resume Linux process: {error}"));
                    state.stopping = true;
                }
            }
            shared.changed.notify_all();
        }
        drop(state);
        let mut progressed = false;
        let mut children_gone = false;
        for _ in 0..256 {
            let mut status = 0;
            // WNOTHREAD is essential: waitpid(-1) without it can steal a
            // different host thread's child, probe, compiler, or Git result.
            let tid = unsafe {
                libc::waitpid(
                    -1,
                    &mut status,
                    libc::WNOHANG | libc::__WALL | libc::__WNOTHREAD,
                )
            };
            if tid == 0 {
                break;
            }
            if tid < 0 {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                if error.raw_os_error() == Some(libc::ECHILD) {
                    children_gone = true;
                } else {
                    fail(&shared, format!("wait Linux process tree: {error}"));
                }
                break;
            }
            progressed = true;
            if let Err(error) = observe(&shared, root, tid, status) {
                fail(&shared, format!("observe Linux process tree: {error}"));
            }
        }
        let mut state = shared.state.lock().unwrap();
        if children_gone {
            if !state.members.is_empty() {
                state
                    .failure
                    .get_or_insert("Linux wait lineage ended with unreconciled members".into());
            }
            state.finished = true;
            shared.changed.notify_all();
            return;
        }
        if !progressed {
            let _ = shared.changed.wait_timeout(state, POLL).unwrap();
        }
    }
}

fn fail(shared: &Shared, message: String) {
    let mut state = shared.state.lock().unwrap();
    state.failure.get_or_insert(message);
    state.stopping = true;
    shared.changed.notify_all();
}

fn observe(shared: &Shared, root: i32, tid: i32, status: i32) -> io::Result<()> {
    let mut state = shared.state.lock().unwrap();
    if libc::WIFEXITED(status) || libc::WIFSIGNALED(status) {
        state.members.remove(&tid);
        if tid == root {
            state.root_status = Some(ExitStatus::from_raw(status));
        }
        shared.changed.notify_all();
        return Ok(());
    }
    if !libc::WIFSTOPPED(status) {
        return Err(io::Error::other("unexpected traced wait state"));
    }
    let event = status >> 16;
    let signal = libc::WSTOPSIG(status);
    let new_child = !state.members.contains_key(&tid);
    if new_child {
        // Auto-attached children's initial stop can precede their parent's
        // fork notification. The dedicated tracer's wait lineage owns both.
        let member = inspect(tid)?;
        if state.members.len() >= MAX_TRACEES {
            kill_member(&member)?;
            return Err(io::Error::other("Linux process tree exceeds member bound"));
        }
        state.members.insert(tid, member);
        state.pending_fork_notifications.insert(tid);
    }
    let initial_stop = new_child
        || state
            .members
            .get_mut(&tid)
            .is_some_and(|member| std::mem::take(&mut member.initial_stop));
    match event {
        libc::PTRACE_EVENT_FORK | libc::PTRACE_EVENT_VFORK | libc::PTRACE_EVENT_CLONE => {
            // The new child is already kernel-traced and stopped. Its initial
            // wait event admits its pidfd before any continuation.
            let child = event_message(tid)?;
            if child <= 0 {
                return Err(io::Error::other("invalid fork event identity"));
            }
            let already_observed = state.pending_fork_notifications.remove(&child);
            if !already_observed && !state.members.contains_key(&child) {
                let mut member = inspect(child)?;
                if state.members.len() >= MAX_TRACEES {
                    kill_member(&member)?;
                    return Err(io::Error::other("Linux process tree exceeds member bound"));
                }
                member.initial_stop = true;
                state.members.insert(child, member);
            }
        }
        libc::PTRACE_EVENT_EXEC => {
            let former_tid = event_message(tid)?;
            let member = inspect(tid)?;
            let pid = member.identity.id().pid();
            state
                .members
                .retain(|old_tid, old| *old_tid == tid || old.identity.id().pid() != pid);
            if former_tid != tid {
                state.members.remove(&former_tid);
            }
            state.members.insert(tid, member);
        }
        0 => {}
        _ => return Err(io::Error::other("unexpected ptrace event")),
    }
    if state.stopping {
        if let Some(member) = state.members.get(&tid) {
            kill_member(member)?;
        }
        return Ok(());
    }
    // Only kernel-generated event/initial-child stops suppress delivery.
    // Ordinary application signals keep their normal process semantics.
    let deliver = if event != 0 || (initial_stop && signal == libc::SIGSTOP) {
        0
    } else {
        signal
    };
    ptrace(libc::PTRACE_CONT, tid, deliver as usize)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};

    fn helper() -> PathBuf {
        let exe = std::env::current_exe().unwrap();
        let path = exe
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join("devmanager-process-test-helper");
        assert!(
            path.is_file(),
            "build the isolated process helper before this test"
        );
        path
    }

    fn wait_marker(path: &Path) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !path.exists() {
            assert!(
                Instant::now() < deadline,
                "marker did not appear: {}",
                path.display()
            );
            std::thread::sleep(POLL);
        }
    }

    #[test]
    fn linux_process_tree_gates_exec_and_reaps_detached_children_after_root_exit() {
        for early_root_exit in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let started = root.path().join("root");
            let descendant = root.path().join("descendant");
            let pid_file = root.path().join("pid");
            let mut command = Command::new(helper());
            command
                .arg("linux-detached-tree")
                .arg(&started)
                .arg(&descendant)
                .arg(&pid_file);
            if early_root_exit {
                command.arg("exit-root");
            }
            let (child, mut tree) =
                LinuxProcessTree::spawn(command, Instant::now() + CLEANUP).unwrap();
            let pid = child.id();
            let control = tree.control();
            std::thread::sleep(Duration::from_millis(50));
            assert!(
                !started.exists(),
                "provider instructions ran before registration/resume"
            );
            assert_eq!(
                control.members(Instant::now() + CLEANUP).unwrap()[0]
                    .id()
                    .pid(),
                pid
            );
            control.resume(Instant::now() + CLEANUP).unwrap();
            assert!(
                control.resume(Instant::now() + CLEANUP).is_err(),
                "resume must be one-way"
            );
            wait_marker(&descendant);
            wait_marker(&pid_file);
            let descendant_pid: u32 = std::fs::read_to_string(&pid_file).unwrap().parse().unwrap();
            let members = control.members(Instant::now() + CLEANUP).unwrap();
            assert!(members
                .iter()
                .any(|member| member.id().pid() == descendant_pid));
            if early_root_exit {
                let deadline = Instant::now() + CLEANUP;
                while control.root_status().unwrap().is_none() {
                    assert!(Instant::now() < deadline);
                    std::thread::sleep(POLL);
                }
                assert!(!control
                    .wait_empty(Instant::now() + Duration::from_millis(20))
                    .unwrap());
            }
            control.terminate().unwrap();
            tree.join(Instant::now() + CLEANUP).unwrap();
            assert!(control
                .members(Instant::now() + CLEANUP)
                .unwrap()
                .is_empty());
            assert!(!PathBuf::from(format!("/proc/{pid}")).exists());
            // An orphan may briefly be a PID 1-owned zombie after its ptrace
            // exit is reaped. It must never remain a live detached process.
            assert!(!crate::services::platform_service::is_pid_running(
                descendant_pid
            ));
        }
    }

    #[test]
    fn linux_process_tree_follows_nonleader_thread_exec_and_drop_reaps_root() {
        let root = tempfile::tempdir().unwrap();
        let marker = root.path().join("thread-exec");
        let mut command = Command::new(helper());
        command.arg("linux-thread-exec").arg(&marker);
        let (child, tree) = LinuxProcessTree::spawn(command, Instant::now() + CLEANUP).unwrap();
        let pid = child.id();
        tree.control().resume(Instant::now() + CLEANUP).unwrap();
        wait_marker(&marker);
        let members = tree.control().members(Instant::now() + CLEANUP).unwrap();
        assert_eq!(members.len(), 1);
        assert_eq!(members[0].id().pid(), pid);
        drop(tree);
        assert!(!PathBuf::from(format!("/proc/{pid}")).exists());
    }

    #[test]
    fn linux_process_tree_owner_death_kills_gated_and_running_roots() {
        const CHILD_ROOT: &str = "DEVMANAGER_TEST_PROBE_OWNER_DEATH_ROOT";
        if let Some(root) = std::env::var_os(CHILD_ROOT) {
            let root = PathBuf::from(root);
            let mut command = Command::new(helper());
            command.arg("linux-death-signal").arg(root.join("started"));
            let (child, tree) = LinuxProcessTree::spawn(command, Instant::now() + CLEANUP).unwrap();
            std::fs::write(root.join("pid"), child.id().to_string()).unwrap();
            if root.join("resume").exists() {
                tree.control().resume(Instant::now() + CLEANUP).unwrap();
            }
            loop {
                std::thread::park();
            }
        }
        struct Owner(Child);
        impl Drop for Owner {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        for resume in [false, true] {
            let root = tempfile::tempdir().unwrap();
            if resume {
                std::fs::write(root.path().join("resume"), b"").unwrap();
            }
            let mut owner = Owner(Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "process::linux::tests::linux_process_tree_owner_death_kills_gated_and_running_roots", "--test-threads=1"])
                .env(CHILD_ROOT, root.path())
                .stdout(std::process::Stdio::null())
                .spawn().unwrap());
            wait_marker(&root.path().join("pid"));
            let pid: i32 = std::fs::read_to_string(root.path().join("pid"))
                .unwrap()
                .parse()
                .unwrap();
            let raw = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
            assert!(raw >= 0);
            let process = unsafe { OwnedFd::from_raw_fd(raw as i32) };
            if resume {
                wait_marker(&root.path().join("started"));
                assert_eq!(
                    std::fs::read_to_string(root.path().join("started")).unwrap(),
                    libc::SIGKILL.to_string()
                );
            } else {
                assert!(!root.path().join("started").exists());
            }
            owner.0.kill().unwrap();
            owner.0.wait().unwrap();
            let mut poll = libc::pollfd {
                fd: process.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            assert_eq!(
                unsafe { libc::poll(&mut poll, 1, 5000) },
                1,
                "provider survived owner death"
            );
            assert_ne!(poll.revents & libc::POLLIN, 0);
            if !resume {
                assert!(!root.path().join("started").exists());
            }
        }
    }

    #[test]
    fn linux_process_tree_does_not_reap_an_unrelated_child_or_execute_dropped_launch() {
        let root = tempfile::tempdir().unwrap();
        let marker = root.path().join("must-not-run");
        let mut unrelated = Command::new("/usr/bin/true").spawn().unwrap();
        let mut command = Command::new(helper());
        command.arg("mark-wait").arg(&marker);
        let (child, tree) = LinuxProcessTree::spawn(command, Instant::now() + CLEANUP).unwrap();
        let pid = child.id();
        drop(tree);
        assert!(!marker.exists());
        assert!(!PathBuf::from(format!("/proc/{pid}")).exists());
        assert!(unrelated.wait().unwrap().success());
    }
}
