//! Windows Job Object acceptance tests.

#[cfg(windows)]
mod windows {
    use std::process::{Child, Command};
    use std::time::{Duration, Instant};

    use devmanager::process::job::ManagedProcessJob;
    use devmanager::services::platform_service::{
        capture_process_identity, claim_suspended_process, process_matches_identity,
        MANAGED_PROCESS_CREATION_FLAGS,
    };

    static_assertions::assert_impl_all!(ManagedProcessJob: Send, Sync);
    static_assertions::assert_not_impl_any!(ManagedProcessJob: Clone);

    struct TestChild {
        child: Child,
    }

    impl TestChild {
        fn spawn_sleeping() -> Self {
            use std::os::windows::process::CommandExt;

            let child = Command::new("powershell.exe")
                .args([
                    "-NoProfile",
                    "-NonInteractive",
                    "-Command",
                    "Start-Sleep -Seconds 30",
                ])
                .creation_flags(MANAGED_PROCESS_CREATION_FLAGS)
                .spawn()
                .expect("spawn suspended test child");
            Self { child }
        }

        fn id(&self) -> u32 {
            self.child.id()
        }

        fn wait_for_exit(&mut self, timeout: Duration) -> bool {
            let deadline = Instant::now() + timeout;
            loop {
                match self.child.try_wait() {
                    Ok(Some(_)) => return true,
                    Ok(None) if Instant::now() < deadline => {
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    _ => return false,
                }
            }
        }
    }

    impl Drop for TestChild {
        fn drop(&mut self) {
            if matches!(self.child.try_wait(), Ok(Some(_))) {
                return;
            }
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }

    fn claim(child: &TestChild) -> ManagedProcessJob {
        claim_suspended_process(child.id())
            .expect("claim suspended test child")
            .expect("Windows managed Job Object")
    }

    #[test]
    fn managed_job_queries_attached_root() {
        let mut child = TestChild::spawn_sleeping();
        let pid = child.id();
        let job = claim(&child);

        let members = job.active_process_ids().expect("query managed Job Object");
        assert!(
            members.contains(&pid),
            "managed Job members {members:?} did not contain root {pid}"
        );

        drop(job);
        assert!(
            child.wait_for_exit(Duration::from_secs(3)),
            "dropping the last Job handle must terminate root {pid} within the bound"
        );
    }

    #[test]
    fn dropping_last_job_handle_terminates_attached_tree_within_bound() {
        use std::os::windows::process::CommandExt;

        let harness = tempfile::tempdir().expect("create Job Object harness");
        let launcher_script = harness.path().join("launcher.ps1");
        let worker_script = harness.path().join("worker.ps1");
        let worker_pid_file = harness.path().join("worker.pid");
        std::fs::write(&worker_script, "Start-Sleep -Seconds 5").expect("write worker script");
        std::fs::write(
            &launcher_script,
            "$worker = Start-Process -NoNewWindow -FilePath powershell.exe -ArgumentList '-NoProfile','-NonInteractive','-File',$env:DEVMANAGER_JOB_WORKER_SCRIPT -PassThru\n[IO.File]::WriteAllText($env:DEVMANAGER_JOB_WORKER_PID, [string]$worker.Id)\n",
        )
        .expect("write launcher script");

        let child = Command::new("powershell.exe")
            .args([
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                "$launcher = Start-Process -NoNewWindow -FilePath powershell.exe -ArgumentList '-NoProfile','-NonInteractive','-File',$env:DEVMANAGER_JOB_LAUNCHER_SCRIPT -PassThru; $launcher.WaitForExit(); Start-Sleep -Seconds 30",
            ])
            .env("DEVMANAGER_JOB_LAUNCHER_SCRIPT", &launcher_script)
            .env("DEVMANAGER_JOB_WORKER_SCRIPT", &worker_script)
            .env("DEVMANAGER_JOB_WORKER_PID", &worker_pid_file)
            .creation_flags(MANAGED_PROCESS_CREATION_FLAGS)
            .spawn()
            .expect("spawn suspended Job Object root");
        let mut child = TestChild { child };
        let root_pid = child.id();
        let job = claim(&child);

        let marker_deadline = Instant::now() + Duration::from_secs(5);
        while !worker_pid_file.exists() && Instant::now() < marker_deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        let worker_pid: u32 = std::fs::read_to_string(&worker_pid_file)
            .expect("launcher must record worker PID")
            .trim()
            .parse()
            .expect("worker PID must be numeric");
        let worker_identity =
            capture_process_identity(worker_pid).expect("capture worker identity");

        let membership_deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let members = job
                .active_process_ids()
                .expect("query managed process tree");
            if members.contains(&root_pid) && members.contains(&worker_pid) {
                break;
            }
            assert!(
                Instant::now() < membership_deadline,
                "managed Job members {members:?} did not contain root {root_pid} and worker {worker_pid}"
            );
            std::thread::sleep(Duration::from_millis(10));
        }

        drop(job);
        let root_exited = child.wait_for_exit(Duration::from_secs(3));
        let worker_deadline = Instant::now() + Duration::from_secs(3);
        while process_matches_identity(
            worker_identity.pid,
            worker_identity.started_at_unix_secs,
            worker_identity.process_name.as_deref(),
        ) && Instant::now() < worker_deadline
        {
            std::thread::sleep(Duration::from_millis(10));
        }
        let worker_exited_within_bound = !process_matches_identity(
            worker_identity.pid,
            worker_identity.started_at_unix_secs,
            worker_identity.process_name.as_deref(),
        );

        if !worker_exited_within_bound {
            let natural_exit_deadline = Instant::now() + Duration::from_secs(5);
            while process_matches_identity(
                worker_identity.pid,
                worker_identity.started_at_unix_secs,
                worker_identity.process_name.as_deref(),
            ) && Instant::now() < natural_exit_deadline
            {
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        assert!(root_exited, "Job close did not terminate root {root_pid}");
        assert!(
            worker_exited_within_bound,
            "Job close did not terminate worker {worker_pid} within the bound"
        );
    }
}

#[cfg(not(windows))]
#[test]
fn managed_job_is_safely_unavailable_off_windows() {
    let job = devmanager::process::job::attach_process_to_managed_job(std::process::id())
        .expect("non-Windows Job lookup must be a safe no-op");
    assert!(job.is_none());
}
