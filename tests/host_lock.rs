//! Exclusive per-profile HostLock acceptance for the real host process.
//!
//! All fixtures use process-unique TempDir config bases and resolve named
//! profile roots through `resolve_app_paths`. These tests must not resolve or
//! touch installed DevManager app-data, config.json, remote.json, session.json,
//! or production kernel.sqlite3.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use tempfile::TempDir;
use uuid::Uuid;

use devmanager::config::paths::{resolve_app_paths, AppProfile, BuildKind};
use devmanager::host::{HostIdentity, HostLock, HostLockError, HOST_EXIT_ALREADY_RUNNING};
use devmanager::protocol::PROTOCOL_MAJOR;

const READY_TIMEOUT: Duration = Duration::from_secs(15);
const EXIT_TIMEOUT: Duration = Duration::from_secs(10);
const TERMINATE_TIMEOUT: Duration = Duration::from_secs(5);
const POLL: Duration = Duration::from_millis(25);

fn host_exe() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_devmanager-host"))
}

fn unique_config_base() -> TempDir {
    TempDir::new().expect("process-unique temp config base")
}

fn named_profile_root(config_base: &Path, profile: &str) -> PathBuf {
    resolve_app_paths(
        config_base,
        AppProfile::named(profile).expect("valid named profile"),
        BuildKind::Debug,
    )
    .expect("resolve named debug profile")
    .root
}

fn lock_path(profile_root: &Path) -> PathBuf {
    profile_root.join("host.lock")
}

fn write_stale_identity(profile_root: &Path, identity: &HostIdentity) {
    fs::create_dir_all(profile_root).expect("create profile root for stale identity");
    let bytes = serde_json::to_vec_pretty(identity).expect("encode stale identity");
    fs::write(lock_path(profile_root), bytes).expect("write stale identity");
}

fn mismatched_live_identity(pid: u32, profile: &str) -> HostIdentity {
    HostIdentity {
        pid,
        process_creation_filetime_ticks: 1,
        executable_path: PathBuf::from("C:\\Windows\\System32\\notepad.exe"),
        profile: profile.to_string(),
        protocol_major: PROTOCOL_MAJOR,
        boot_id: Uuid::nil(),
    }
}

#[cfg(windows)]
fn query_process_creation_filetime_ticks(pid: u32) -> u64 {
    use std::mem::MaybeUninit;

    #[repr(C)]
    struct FileTime {
        dw_low_date_time: u32,
        dw_high_date_time: u32,
    }

    #[link(name = "kernel32")]
    extern "system" {
        fn OpenProcess(desired_access: u32, inherit_handle: i32, process_id: u32) -> isize;
        fn GetProcessTimes(
            process: isize,
            creation: *mut FileTime,
            exit: *mut FileTime,
            kernel: *mut FileTime,
            user: *mut FileTime,
        ) -> i32;
        fn CloseHandle(handle: isize) -> i32;
    }

    const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    assert!(
        handle != 0 && handle != -1,
        "OpenProcess({pid}) for FILETIME query"
    );
    let mut creation = MaybeUninit::<FileTime>::uninit();
    let mut exit = MaybeUninit::<FileTime>::uninit();
    let mut kernel = MaybeUninit::<FileTime>::uninit();
    let mut user = MaybeUninit::<FileTime>::uninit();
    let ok = unsafe {
        GetProcessTimes(
            handle,
            creation.as_mut_ptr(),
            exit.as_mut_ptr(),
            kernel.as_mut_ptr(),
            user.as_mut_ptr(),
        )
    };
    unsafe {
        let _ = CloseHandle(handle);
    }
    assert_ne!(ok, 0, "GetProcessTimes({pid})");
    let creation = unsafe { creation.assume_init() };
    let ticks =
        (u64::from(creation.dw_high_date_time) << 32) | u64::from(creation.dw_low_date_time);
    assert!(ticks > 0, "creation FILETIME ticks must be nonzero");
    ticks
}

#[cfg(windows)]
fn create_directory_junction(target: &Path, link: &Path) {
    let status = Command::new("cmd.exe")
        .args(["/c", "mklink", "/J"])
        .arg(link)
        .arg(target)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("spawn mklink /J");
    assert!(
        status.success(),
        "mklink /J failed creating junction {} -> {}; cannot silently skip",
        link.display(),
        target.display()
    );
}

#[cfg(windows)]
fn remove_directory_junction(link: &Path) {
    fs::remove_dir(link).unwrap_or_else(|error| {
        panic!(
            "failed to remove directory junction {}: {error}",
            link.display()
        )
    });
}

fn read_identity(path: &Path) -> Option<HostIdentity> {
    let bytes = fs::read(path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Exact child-handle guard. Drop is best-effort and never panics.
#[cfg(windows)]
struct ChildGuard {
    child: Option<Child>,
}

#[cfg(windows)]
impl ChildGuard {
    fn spawn(mut command: Command) -> Self {
        let child = command.spawn().expect("spawn child process");
        Self { child: Some(child) }
    }

    fn id(&self) -> u32 {
        self.child.as_ref().expect("child still owned").id()
    }

    fn try_wait(&mut self) -> std::io::Result<Option<ExitStatus>> {
        self.child.as_mut().expect("child still owned").try_wait()
    }

    fn wait_exit_bounded(&mut self, timeout: Duration) -> ExitStatus {
        let started = Instant::now();
        let pid = self.id();
        loop {
            if let Some(status) = self.try_wait().expect("poll child exit") {
                return status;
            }
            if started.elapsed() >= timeout {
                panic!("child pid {pid} did not exit within {timeout:?}");
            }
            thread::sleep(POLL);
        }
    }

    /// Normal-path cleanup: kill, poll to completion, and report failures.
    fn terminate_and_wait_bounded(&mut self, timeout: Duration) -> Result<ExitStatus, String> {
        let Some(child) = self.child.as_mut() else {
            return Err("child already taken".to_string());
        };
        let pid = child.id();
        child
            .kill()
            .map_err(|error| format!("kill pid {pid} failed: {error}"))?;
        let started = Instant::now();
        loop {
            match child.try_wait() {
                Ok(Some(status)) => return Ok(status),
                Ok(None) => {
                    if started.elapsed() >= timeout {
                        return Err(format!("pid {pid} did not terminate within {timeout:?}"));
                    }
                    thread::sleep(POLL);
                }
                Err(error) => return Err(format!("wait pid {pid} failed: {error}")),
            }
        }
    }
}

#[cfg(windows)]
impl Drop for ChildGuard {
    fn drop(&mut self) {
        let Some(mut child) = self.child.take() else {
            return;
        };
        let _ = child.kill();
        let started = Instant::now();
        while started.elapsed() < TERMINATE_TIMEOUT {
            match child.try_wait() {
                Ok(Some(_)) => return,
                Ok(None) => thread::sleep(POLL),
                Err(_) => return,
            }
        }
    }
}

#[cfg(windows)]
fn host_command_with_parent(
    config_base: &Path,
    profile: &str,
    label: &str,
    parent_pid: u32,
) -> Command {
    let mut command = Command::new(host_exe());
    command
        .arg("--foreground")
        .arg("--profile")
        .arg(profile)
        .arg("--instance-label")
        .arg(label)
        .arg("--parent-pid")
        .arg(parent_pid.to_string())
        .arg("--config-base")
        .arg(config_base)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    command
}

#[cfg(windows)]
fn host_command(config_base: &Path, profile: &str, label: &str) -> Command {
    host_command_with_parent(config_base, profile, label, std::process::id())
}

#[cfg(windows)]
fn wait_host_ready(child: &mut ChildGuard, profile_root: &Path) -> HostIdentity {
    let path = lock_path(profile_root);
    let started = Instant::now();
    loop {
        if let Some(status) = child.try_wait().expect("poll host while waiting ready") {
            panic!(
                "host pid {} exited before readiness with {status}",
                child.id()
            );
        }
        if let Some(identity) = read_identity(&path) {
            if identity.pid == child.id() {
                return identity;
            }
        }
        if started.elapsed() >= READY_TIMEOUT {
            panic!(
                "timed out waiting for host pid {} readiness at {}",
                child.id(),
                path.display()
            );
        }
        thread::sleep(POLL);
    }
}

#[cfg(windows)]
fn spawn_ready_host(config_base: &Path, profile: &str, label: &str) -> (ChildGuard, HostIdentity) {
    let profile_root = named_profile_root(config_base, profile);
    let mut child = ChildGuard::spawn(host_command(config_base, profile, label));
    let identity = wait_host_ready(&mut child, &profile_root);
    (child, identity)
}

#[test]
#[cfg(windows)]
fn second_host_is_rejected() {
    let config_base = unique_config_base();
    let profile = "lock-a";
    let profile_root = named_profile_root(config_base.path(), profile);

    let (mut first, first_identity) = spawn_ready_host(config_base.path(), profile, "first-host");
    assert_eq!(first_identity.pid, first.id());
    assert_eq!(first_identity.profile, profile);
    assert_eq!(first_identity.protocol_major, PROTOCOL_MAJOR);
    assert_ne!(first_identity.boot_id, Uuid::nil());
    assert!(first_identity.process_creation_filetime_ticks > 0);

    let mut second = ChildGuard::spawn(host_command(config_base.path(), profile, "second-host"));
    let status = second.wait_exit_bounded(EXIT_TIMEOUT);
    assert_eq!(
        status.code(),
        Some(i32::from(HOST_EXIT_ALREADY_RUNNING)),
        "second host must exit with HOST_EXIT_ALREADY_RUNNING; got {status}"
    );

    let still_first = read_identity(&lock_path(&profile_root)).expect("first lock identity");
    assert_eq!(still_first.pid, first.id());
    assert_eq!(still_first.boot_id, first_identity.boot_id);
    assert!(
        first.try_wait().expect("poll first host").is_none(),
        "first host must remain alive after rejected second"
    );
    first
        .terminate_and_wait_bounded(TERMINATE_TIMEOUT)
        .expect("terminate first host");
}

#[test]
#[cfg(windows)]
fn dropping_holder_permits_reacquire_and_replaces_identity() {
    let root = TempDir::new().expect("temp profile root");
    let first = HostLock::acquire(root.path(), "lock-b").expect("first acquire");
    let first_boot = first.identity().boot_id;
    assert!(first.identity().process_creation_filetime_ticks > 0);
    drop(first);

    let second = HostLock::acquire(root.path(), "lock-b").expect("reacquire");
    assert_ne!(second.identity().boot_id, first_boot);
    assert_eq!(second.identity().profile, "lock-b");
    assert_eq!(second.identity().protocol_major, PROTOCOL_MAJOR);
    assert!(second.identity().process_creation_filetime_ticks > 0);
}

#[test]
#[cfg(windows)]
fn different_profiles_can_coexist() {
    let config_base = unique_config_base();
    let (mut host_a, identity_a) = spawn_ready_host(config_base.path(), "profile-a", "label-a");
    let (mut host_b, identity_b) = spawn_ready_host(config_base.path(), "profile-b", "label-b");

    assert_ne!(identity_a.boot_id, identity_b.boot_id);
    assert_eq!(identity_a.profile, "profile-a");
    assert_eq!(identity_b.profile, "profile-b");
    assert_eq!(identity_a.pid, host_a.id());
    assert_eq!(identity_b.pid, host_b.id());
    assert!(identity_a.process_creation_filetime_ticks > 0);
    assert!(identity_b.process_creation_filetime_ticks > 0);
    assert!(host_a.try_wait().expect("poll a").is_none());
    assert!(host_b.try_wait().expect("poll b").is_none());
    host_a
        .terminate_and_wait_bounded(TERMINATE_TIMEOUT)
        .expect("terminate a");
    host_b
        .terminate_and_wait_bounded(TERMINATE_TIMEOUT)
        .expect("terminate b");
}

#[test]
#[cfg(windows)]
fn lock_records_executable_and_start_time() {
    let config_base = unique_config_base();
    let (mut host, identity) =
        spawn_ready_host(config_base.path(), "identity-check", "identity-label");

    let expected_exe = host_exe().canonicalize().expect("canonical host exe");
    let live_ticks = query_process_creation_filetime_ticks(host.id());
    assert_eq!(identity.pid, host.id());
    assert_eq!(
        identity.process_creation_filetime_ticks, live_ticks,
        "HostIdentity creation ticks must exactly match live child GetProcessTimes"
    );
    assert_eq!(identity.executable_path, expected_exe);
    assert_eq!(identity.profile, "identity-check");
    assert_eq!(identity.protocol_major, PROTOCOL_MAJOR);
    assert_ne!(identity.boot_id, Uuid::nil());
    host.terminate_and_wait_bounded(TERMINATE_TIMEOUT)
        .expect("terminate identity host");
}

#[test]
#[cfg(windows)]
fn stale_pid_record_is_recovered() {
    let config_base = unique_config_base();
    let profile = "stale-dead";
    let profile_root = named_profile_root(config_base.path(), profile);
    write_stale_identity(&profile_root, &mismatched_live_identity(u32::MAX, profile));

    let (mut host, identity) = spawn_ready_host(config_base.path(), profile, "stale-recover");
    assert_eq!(identity.pid, host.id());
    assert_ne!(identity.boot_id, Uuid::nil());
    assert_ne!(identity.pid, u32::MAX);
    assert!(identity.process_creation_filetime_ticks > 0);
    host.terminate_and_wait_bounded(TERMINATE_TIMEOUT)
        .expect("terminate recovered host");
}

#[test]
#[cfg(windows)]
fn live_unrelated_pid_is_not_killed() {
    let config_base = unique_config_base();
    let profile = "stale-live";
    let profile_root = named_profile_root(config_base.path(), profile);

    let (mut canary, canary_identity) =
        spawn_ready_host(config_base.path(), "canary-live", "canary-label");
    let canary_pid = canary.id();
    assert_eq!(canary_identity.pid, canary_pid);

    // Plant mismatched metadata that only shares the live PID.
    write_stale_identity(
        &profile_root,
        &mismatched_live_identity(canary_pid, profile),
    );

    let (mut host, identity) = spawn_ready_host(config_base.path(), profile, "stale-live-label");
    assert_eq!(identity.pid, host.id());
    assert_ne!(identity.pid, canary_pid);
    assert!(
        canary.try_wait().expect("poll canary").is_none(),
        "unrelated live canary must not be killed or signaled"
    );

    host.terminate_and_wait_bounded(TERMINATE_TIMEOUT)
        .expect("terminate recovered host");
    canary
        .terminate_and_wait_bounded(TERMINATE_TIMEOUT)
        .expect("terminate canary");
}

#[test]
#[cfg(windows)]
fn stale_identity_wrong_ticks_only_is_recovered() {
    let config_base = unique_config_base();
    let (mut canary, canary_identity) =
        spawn_ready_host(config_base.path(), "canary-ticks", "canary-ticks-label");

    let profile = "stale-ticks-only";
    let profile_root = named_profile_root(config_base.path(), profile);
    let mut planted = canary_identity.clone();
    planted.profile = profile.to_string();
    planted.process_creation_filetime_ticks = canary_identity
        .process_creation_filetime_ticks
        .wrapping_add(1);
    assert_eq!(planted.executable_path, canary_identity.executable_path);
    assert_ne!(
        planted.process_creation_filetime_ticks,
        canary_identity.process_creation_filetime_ticks
    );
    write_stale_identity(&profile_root, &planted);

    let (mut host, identity) = spawn_ready_host(config_base.path(), profile, "recover-ticks-label");
    assert_eq!(identity.pid, host.id());
    assert_ne!(identity.pid, canary.id());
    assert!(
        canary.try_wait().expect("poll canary").is_none(),
        "ticks-only stale recovery must not signal the canary"
    );
    host.terminate_and_wait_bounded(TERMINATE_TIMEOUT)
        .expect("terminate recovered host");
    canary
        .terminate_and_wait_bounded(TERMINATE_TIMEOUT)
        .expect("terminate canary");
}

#[test]
#[cfg(windows)]
fn stale_identity_wrong_exe_only_is_recovered() {
    let config_base = unique_config_base();
    let (mut canary, canary_identity) =
        spawn_ready_host(config_base.path(), "canary-exe", "canary-exe-label");

    let profile = "stale-exe-only";
    let profile_root = named_profile_root(config_base.path(), profile);
    let mut planted = canary_identity.clone();
    planted.profile = profile.to_string();
    planted.executable_path = PathBuf::from("C:\\Windows\\System32\\notepad.exe");
    assert_eq!(
        planted.process_creation_filetime_ticks,
        canary_identity.process_creation_filetime_ticks
    );
    assert_ne!(planted.executable_path, canary_identity.executable_path);
    write_stale_identity(&profile_root, &planted);

    let (mut host, identity) = spawn_ready_host(config_base.path(), profile, "recover-exe-label");
    assert_eq!(identity.pid, host.id());
    assert_ne!(identity.pid, canary.id());
    assert!(
        canary.try_wait().expect("poll canary").is_none(),
        "exe-only stale recovery must not signal the canary"
    );
    host.terminate_and_wait_bounded(TERMINATE_TIMEOUT)
        .expect("terminate recovered host");
    canary
        .terminate_and_wait_bounded(TERMINATE_TIMEOUT)
        .expect("terminate canary");
}

#[test]
#[cfg(windows)]
fn profile_root_junction_is_rejected_before_host_lock() {
    let config_base = unique_config_base();
    let profile = "junction-prof";
    let profile_root = named_profile_root(config_base.path(), profile);
    let junction_target = config_base.path().join("junction-target");
    fs::create_dir(&junction_target).expect("create in-base junction target");
    create_directory_junction(&junction_target, &profile_root);

    let mut host = ChildGuard::spawn(host_command(config_base.path(), profile, "junction-label"));
    let status = host.wait_exit_bounded(EXIT_TIMEOUT);
    assert!(
        !status.success(),
        "host must reject profile-root junction before HostLock; got {status}"
    );
    assert_ne!(
        status.code(),
        Some(i32::from(HOST_EXIT_ALREADY_RUNNING)),
        "junction rejection must not look like a lock conflict"
    );
    assert!(
        !lock_path(&junction_target).exists(),
        "host.lock must not be created under the junction target"
    );
    assert!(
        read_identity(&lock_path(&profile_root)).is_none(),
        "host.lock must not be created through the rejected junction root"
    );

    remove_directory_junction(&profile_root);
}

#[test]
#[cfg(windows)]
fn exact_live_matching_identity_without_os_lock_is_rejected() {
    let config_base = unique_config_base();
    let (mut canary, canary_identity) =
        spawn_ready_host(config_base.path(), "canary-exact", "canary-exact-label");

    let target_profile = "target-exact";
    let target_root = named_profile_root(config_base.path(), target_profile);
    let mut planted = canary_identity.clone();
    planted.profile = target_profile.to_string();
    write_stale_identity(&target_root, &planted);

    let err = HostLock::acquire(&target_root, target_profile).expect_err("exact live match");
    assert!(
        matches!(err, HostLockError::AlreadyRunning { .. }),
        "exact live matching identity without OS lock must fail closed: {err:?}"
    );
    assert!(
        canary.try_wait().expect("poll canary").is_none(),
        "exact-match rejection must not kill the canary"
    );
    canary
        .terminate_and_wait_bounded(TERMINATE_TIMEOUT)
        .expect("terminate canary");
}

#[test]
#[cfg(windows)]
fn exact_live_identity_from_other_profile_is_stale_and_recovered() {
    let config_base = unique_config_base();
    let (mut canary, canary_identity) =
        spawn_ready_host(config_base.path(), "canary-scope", "canary-scope-label");

    let target_profile = "target-scope";
    let target_root = named_profile_root(config_base.path(), target_profile);
    // Plant the canary's exact live identity, including its different profile name.
    assert_ne!(canary_identity.profile, target_profile);
    write_stale_identity(&target_root, &canary_identity);

    let lock = HostLock::acquire(&target_root, target_profile)
        .expect("cross-profile exact live metadata must be recoverable stale");
    assert_eq!(lock.identity().pid, std::process::id());
    assert_eq!(lock.identity().profile, target_profile);
    assert_ne!(lock.identity().boot_id, canary_identity.boot_id);
    assert!(
        canary.try_wait().expect("poll canary").is_none(),
        "cross-profile recovery must not signal the canary"
    );
    drop(lock);
    canary
        .terminate_and_wait_bounded(TERMINATE_TIMEOUT)
        .expect("terminate canary");
}

#[test]
#[cfg(windows)]
fn oversized_lock_metadata_is_treated_as_stale_and_replaced() {
    const MAX_HOST_IDENTITY_JSON_BYTES: usize = 64 * 1024;
    let root = TempDir::new().expect("temp profile root");
    let profile = "oversize-meta";
    fs::create_dir_all(root.path()).expect("create profile root");
    let oversized = vec![b'A'; MAX_HOST_IDENTITY_JSON_BYTES + 1];
    fs::write(lock_path(root.path()), &oversized).expect("plant oversized host.lock");

    let lock = HostLock::acquire(root.path(), profile)
        .expect("oversized host.lock metadata must be treated as invalid/stale");
    assert_eq!(lock.identity().pid, std::process::id());
    assert_eq!(lock.identity().profile, profile);
    assert!(lock.identity().process_creation_filetime_ticks > 0);
    assert_ne!(lock.identity().boot_id, Uuid::nil());

    let replaced = fs::read(lock_path(root.path())).expect("read replaced identity");
    assert!(
        replaced.len() <= MAX_HOST_IDENTITY_JSON_BYTES,
        "replacement identity must be within the bounded JSON ceiling"
    );
    let parsed: HostIdentity =
        serde_json::from_slice(&replaced).expect("replacement must be valid HostIdentity JSON");
    assert_eq!(parsed.boot_id, lock.identity().boot_id);
}

#[test]
#[cfg(windows)]
fn fake_parent_pid_is_rejected_without_lock_or_canary_harm() {
    let config_base = unique_config_base();
    let (mut canary, _) =
        spawn_ready_host(config_base.path(), "canary-parent", "canary-parent-label");
    let canary_pid = canary.id();

    let victim_profile = "victim-parent";
    let victim_root = named_profile_root(config_base.path(), victim_profile);
    let mut victim = ChildGuard::spawn(host_command_with_parent(
        config_base.path(),
        victim_profile,
        "victim-label",
        canary_pid,
    ));
    let status = victim.wait_exit_bounded(EXIT_TIMEOUT);
    assert!(
        !status.success(),
        "host with non-parent canary PID must exit nonzero; got {status}"
    );
    assert_ne!(
        status.code(),
        Some(i32::from(HOST_EXIT_ALREADY_RUNNING)),
        "parent mismatch must not look like a lock conflict"
    );

    if let Some(identity) = read_identity(&lock_path(&victim_root)) {
        panic!("rejected fake-parent host must not leave a lock identity: {identity:?}");
    }
    assert!(
        canary.try_wait().expect("poll canary").is_none(),
        "fake-parent rejection must not affect the canary"
    );
    canary
        .terminate_and_wait_bounded(TERMINATE_TIMEOUT)
        .expect("terminate canary");
}

#[test]
#[cfg(windows)]
fn debug_host_rejects_empty_or_production_profile_and_missing_foreground() {
    let config_base = unique_config_base();
    let exe = host_exe();
    let parent = std::process::id().to_string();
    let base = config_base.path().to_string_lossy().into_owned();

    let cases: [(&str, Vec<&str>); 4] = [
        (
            "missing --foreground",
            vec![
                "--profile",
                "gate-profile",
                "--instance-label",
                "label",
                "--parent-pid",
                parent.as_str(),
                "--config-base",
                base.as_str(),
            ],
        ),
        (
            "empty profile",
            vec![
                "--foreground",
                "--profile",
                "",
                "--instance-label",
                "label",
                "--parent-pid",
                parent.as_str(),
                "--config-base",
                base.as_str(),
            ],
        ),
        (
            "reserved production profile",
            vec![
                "--foreground",
                "--profile",
                "production",
                "--instance-label",
                "label",
                "--parent-pid",
                parent.as_str(),
                "--config-base",
                base.as_str(),
            ],
        ),
        (
            "empty instance label",
            vec![
                "--foreground",
                "--profile",
                "gate-profile",
                "--instance-label",
                "",
                "--parent-pid",
                parent.as_str(),
                "--config-base",
                base.as_str(),
            ],
        ),
    ];

    for (name, args) in cases {
        let mut command = Command::new(&exe);
        command
            .args(&args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let mut child = ChildGuard::spawn(command);
        let status = child.wait_exit_bounded(EXIT_TIMEOUT);
        assert!(
            !status.success(),
            "{name}: expected nonzero exit, got {status}"
        );
    }
}

#[test]
#[cfg(not(windows))]
fn acquire_is_unsupported_off_windows() {
    let root = TempDir::new().expect("temp profile root");
    let err = HostLock::acquire(root.path(), "unsupported").expect_err("unsupported");
    assert!(matches!(err, HostLockError::Unsupported));
}
