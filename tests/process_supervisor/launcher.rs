//! Suspended managed PTY launch acceptance tests.

#[cfg(windows)]
mod windows {
    use std::collections::BTreeMap;
    use std::ffi::OsString;
    use std::io::{Read, Write};
    use std::path::{Path, PathBuf};
    use std::time::{Duration, Instant};

    use devmanager::domain::id::{ResourceId, TaskId};
    use devmanager::domain::resource::ResourceKind;
    use devmanager::process::identity::ProcessOwner;
    use devmanager::process::job::ManagedProcessJob;
    use devmanager::process::launcher::{prepare_suspended_pty, LaunchIntent, ManagedPtyChild};
    use devmanager::process::registry::{
        ManagedProcessState, ProcessRegistry, ProcessRegistryError,
    };
    use portable_pty::{native_pty_system, PtyPair, PtySize, SlavePty};
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use windows::Win32::Foundation::{HANDLE, WAIT_OBJECT_0};
    use windows::Win32::System::Threading::{
        OpenProcess, WaitForSingleObject, PROCESS_SYNCHRONIZE,
    };

    const MARKER_BOUND: Duration = Duration::from_secs(5);
    const EXIT_BOUND: Duration = Duration::from_secs(3);

    fn fixed_uuid_v7(tail: u8) -> [u8; 16] {
        [
            0x01, 0x9a, 0x11, 0x22, 0x33, 0x44, 0x70, 0x00, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, tail,
        ]
    }

    fn resource_id(tail: u8) -> ResourceId {
        ResourceId::from_bytes(fixed_uuid_v7(tail)).expect("resource id")
    }

    fn helper() -> PathBuf {
        if let Some(path) = std::env::var_os("CARGO_BIN_EXE_devmanager-process-test-helper") {
            return PathBuf::from(path);
        }
        let current = std::env::current_exe().expect("test executable path");
        let target = current
            .parent()
            .and_then(Path::parent)
            .expect("target directory beside test executable");
        let suffix = if cfg!(windows) { ".exe" } else { "" };
        let helper = format!("devmanager-process-test-helper{suffix}");
        let candidate = target.join(&helper);
        if candidate.is_file() {
            return candidate;
        }
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("debug")
            .join(helper)
    }

    fn intent(resource_id: ResourceId, generation: u64, args: Vec<OsString>) -> LaunchIntent {
        LaunchIntent {
            resource_id,
            generation,
            owner: ProcessOwner::Task(TaskId::new()),
            kind: ResourceKind::Terminal,
            executable: helper(),
            args,
            cwd: PathBuf::from(env!("CARGO_MANIFEST_DIR")),
            environment: BTreeMap::new(),
            display_label: "Phase 3 process helper".to_string(),
        }
    }

    fn marker_args(mode: &str, paths: &[&Path]) -> Vec<OsString> {
        std::iter::once(OsString::from(mode))
            .chain(paths.iter().map(|path| path.as_os_str().to_owned()))
            .collect()
    }

    struct TestPty {
        pair: Option<PtyPair>,
        responder: Option<std::thread::JoinHandle<()>>,
    }

    impl TestPty {
        fn open() -> Self {
            let pair = native_pty_system()
                .openpty(PtySize {
                    rows: 24,
                    cols: 80,
                    pixel_width: 0,
                    pixel_height: 0,
                })
                .expect("open ConPTY");
            let mut reader = pair.master.try_clone_reader().expect("clone PTY reader");
            let mut writer = pair.master.take_writer().expect("take PTY writer");
            let responder = std::thread::spawn(move || {
                let mut carry = Vec::new();
                let mut chunk = [0u8; 256];
                loop {
                    let Ok(read) = reader.read(&mut chunk) else {
                        return;
                    };
                    if read == 0 {
                        return;
                    }
                    carry.extend_from_slice(&chunk[..read]);
                    if carry.windows(4).any(|window| window == b"\x1b[6n") {
                        if writer.write_all(b"\x1b[1;1R").is_err() || writer.flush().is_err() {
                            return;
                        }
                        carry.clear();
                    } else if carry.len() > 16 {
                        carry.drain(..carry.len() - 3);
                    }
                }
            });
            Self {
                pair: Some(pair),
                responder: Some(responder),
            }
        }

        fn slave(&self) -> &dyn SlavePty {
            self.pair.as_ref().expect("live test PTY").slave.as_ref()
        }
    }

    impl Drop for TestPty {
        fn drop(&mut self) {
            self.pair.take();
            if let Some(responder) = self.responder.take() {
                responder.join().expect("join PTY responder");
            }
        }
    }

    fn wait_for_file(path: &Path) {
        let deadline = Instant::now() + MARKER_BOUND;
        while !path.exists() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(path.exists(), "marker {} was not written", path.display());
    }

    fn wait_for_file_or_child_exit(path: &Path, child: &mut ManagedPtyChild) {
        let deadline = Instant::now() + MARKER_BOUND;
        while !path.exists() && Instant::now() < deadline {
            if let Some(status) = child.try_wait().expect("query managed child") {
                panic!(
                    "managed child {} exited as {status} before writing {}",
                    child.process_id(),
                    path.display()
                );
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(path.exists(), "marker {} was not written", path.display());
    }

    fn assert_file_absent_for(path: &Path, duration: Duration) {
        let deadline = Instant::now() + duration;
        while Instant::now() < deadline {
            assert!(
                !path.exists(),
                "suspended process unexpectedly wrote {}",
                path.display()
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    struct ProcessWaitHandle(OwnedHandle);

    impl ProcessWaitHandle {
        fn open(pid: u32) -> Self {
            let handle = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, false, pid) }
                .expect("open exact child wait handle");
            Self(unsafe { OwnedHandle::from_raw_handle(handle.0) })
        }

        fn assert_signaled(&self) {
            let result = unsafe {
                WaitForSingleObject(
                    HANDLE(self.0.as_raw_handle()),
                    EXIT_BOUND.as_millis() as u32,
                )
            };
            assert_eq!(result, WAIT_OBJECT_0, "descendant process remained alive");
        }
    }

    fn wait_for_exit(child: &mut ManagedPtyChild) {
        let deadline = Instant::now() + EXIT_BOUND;
        loop {
            match child.try_wait().expect("query managed child") {
                Some(_) => return,
                None if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(5)),
                None => panic!("managed child {} did not exit", child.process_id()),
            }
        }
    }

    fn close_registered(registry: ProcessRegistry<ManagedProcessJob>, child: &mut ManagedPtyChild) {
        drop(registry);
        wait_for_exit(child);
    }

    #[test]
    fn child_is_suspended_until_job_assignment() {
        let harness = tempfile::tempdir().expect("launcher harness");
        let marker = harness.path().join("started.marker");
        let pty = TestPty::open();
        let pending = prepare_suspended_pty(
            pty.slave(),
            intent(resource_id(1), 1, marker_args("mark-wait", &[&marker])),
        )
        .expect("prepare suspended managed launch");

        assert!(
            pending
                .active_process_ids()
                .expect("query pre-resume Job")
                .contains(&pending.process_id()),
            "root must already be in its Job while suspended"
        );
        assert_file_absent_for(&marker, Duration::from_millis(150));

        let mut registry = ProcessRegistry::new();
        let mut child = pending
            .register_and_resume(&mut registry)
            .expect("register then resume");
        assert_eq!(
            registry
                .current(resource_id(1))
                .expect("resumed process")
                .state(),
            ManagedProcessState::Running
        );
        wait_for_file_or_child_exit(&marker, &mut child);
        close_registered(registry, &mut child);
    }

    #[test]
    fn assignment_failure_never_executes_child() {
        let harness = tempfile::tempdir().expect("launcher harness");
        let first_marker = harness.path().join("first.marker");
        let rejected_marker = harness.path().join("rejected.marker");
        let resource = resource_id(2);
        let mut registry = ProcessRegistry::new();

        let first_pty = TestPty::open();
        let first_pending = prepare_suspended_pty(
            first_pty.slave(),
            intent(resource, 1, marker_args("mark-wait", &[&first_marker])),
        )
        .expect("prepare first launch");
        let mut first = first_pending
            .register_and_resume(&mut registry)
            .expect("register first launch");
        wait_for_file(&first_marker);

        let rejected_pty = TestPty::open();
        let rejected = prepare_suspended_pty(
            rejected_pty.slave(),
            intent(resource, 2, marker_args("mark-wait", &[&rejected_marker])),
        )
        .expect("prepare rejected launch")
        .register_and_resume(&mut registry)
        .expect_err("active resource generation must reject replacement");
        assert!(matches!(
            rejected.registry_error(),
            Some(ProcessRegistryError::ActiveGeneration { .. })
        ));
        assert_file_absent_for(&rejected_marker, Duration::from_millis(100));

        close_registered(registry, &mut first);
    }

    #[test]
    fn nested_children_join_job() {
        let harness = tempfile::tempdir().expect("launcher harness");
        let root_marker = harness.path().join("root.marker");
        let child_marker = harness.path().join("child.marker");
        let child_pid_file = harness.path().join("child.pid");
        let resource = resource_id(3);
        let pty = TestPty::open();
        let mut registry = ProcessRegistry::new();
        let mut root = prepare_suspended_pty(
            pty.slave(),
            intent(
                resource,
                1,
                marker_args(
                    "spawn-child",
                    &[&root_marker, &child_marker, &child_pid_file],
                ),
            ),
        )
        .expect("prepare tree launch")
        .register_and_resume(&mut registry)
        .expect("register tree launch");

        wait_for_file(&child_pid_file);
        wait_for_file(&child_marker);
        let child_pid: u32 = std::fs::read_to_string(&child_pid_file)
            .expect("read child PID")
            .trim()
            .parse()
            .expect("numeric child PID");
        let child_wait = ProcessWaitHandle::open(child_pid);
        let members = registry
            .current(resource)
            .expect("registered tree")
            .job()
            .active_process_ids()
            .expect("query complete Job tree");
        assert!(members.contains(&root.process_id()));
        assert!(members.contains(&child_pid));

        close_registered(registry, &mut root);
        child_wait.assert_signaled();
    }

    #[test]
    fn breakaway_is_disabled() {
        let harness = tempfile::tempdir().expect("launcher harness");
        let result = harness.path().join("breakaway.result");
        let escaped_marker = harness.path().join("escaped.marker");
        let pty = TestPty::open();
        let mut registry = ProcessRegistry::new();
        let mut root = prepare_suspended_pty(
            pty.slave(),
            intent(
                resource_id(4),
                1,
                marker_args("attempt-breakaway", &[&result, &escaped_marker]),
            ),
        )
        .expect("prepare breakaway probe")
        .register_and_resume(&mut registry)
        .expect("register breakaway probe");

        wait_for_file(&result);
        let outcome = std::fs::read_to_string(&result).expect("read breakaway result");
        assert!(
            outcome.starts_with("blocked:"),
            "managed child escaped Job containment: {outcome}"
        );
        assert!(!escaped_marker.exists());
        close_registered(registry, &mut root);
    }

    #[test]
    fn closing_job_terminates_entire_tree() {
        let harness = tempfile::tempdir().expect("launcher harness");
        let root_marker = harness.path().join("root.marker");
        let child_marker = harness.path().join("child.marker");
        let child_pid_file = harness.path().join("child.pid");
        let resource = resource_id(5);
        let pty = TestPty::open();
        let mut registry = ProcessRegistry::new();
        let mut root = prepare_suspended_pty(
            pty.slave(),
            intent(
                resource,
                1,
                marker_args(
                    "spawn-child",
                    &[&root_marker, &child_marker, &child_pid_file],
                ),
            ),
        )
        .expect("prepare tree launch")
        .register_and_resume(&mut registry)
        .expect("register tree launch");
        wait_for_file(&child_pid_file);
        let child_pid: u32 = std::fs::read_to_string(&child_pid_file)
            .expect("read child PID")
            .trim()
            .parse()
            .expect("numeric child PID");
        let child_wait = ProcessWaitHandle::open(child_pid);

        close_registered(registry, &mut root);
        child_wait.assert_signaled();
    }
}

#[cfg(not(windows))]
#[test]
fn suspended_managed_pty_is_windows_only() {
    assert!(devmanager::process::launcher::is_supported().is_err());
}
