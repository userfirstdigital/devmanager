//! Exclusive per-profile HostLock acceptance for the future host process.
//!
//! All fixtures use TempDir roots. These tests must not resolve or touch
//! installed DevManager app-data, config.json, remote.json, or session.json.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use tempfile::TempDir;
use uuid::Uuid;

use devmanager::host::{HostIdentity, HostLock, HostLockError};
use devmanager::protocol::PROTOCOL_MAJOR;

fn profile_root() -> TempDir {
    TempDir::new().expect("temp profile root")
}

fn write_stale_identity(root: &Path, identity: &HostIdentity) {
    let path = root.join("host.lock");
    let bytes = serde_json::to_vec_pretty(identity).expect("encode stale identity");
    fs::write(&path, bytes).expect("write stale identity");
}

fn sample_identity(pid: u32, profile: &str) -> HostIdentity {
    HostIdentity {
        pid,
        process_start_time_unix_secs: 1,
        executable_path: PathBuf::from("C:\\Windows\\System32\\notepad.exe"),
        profile: profile.to_string(),
        protocol_major: PROTOCOL_MAJOR,
        boot_id: Uuid::nil(),
    }
}

/// Kills and waits for a spawned child on drop so panic paths cannot leak it.
#[cfg(windows)]
struct ChildGuard(Child);

#[cfg(windows)]
impl ChildGuard {
    fn spawn(mut command: Command) -> Self {
        let child = command.spawn().expect("spawn live unrelated process");
        Self(child)
    }

    fn id(&self) -> u32 {
        self.0.id()
    }

    fn try_wait(&mut self) -> std::io::Result<Option<std::process::ExitStatus>> {
        self.0.try_wait()
    }
}

#[cfg(windows)]
impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
#[cfg(windows)]
fn second_host_is_rejected() {
    let root = profile_root();
    let first = HostLock::acquire(root.path(), "lock-a").expect("first acquire");
    let second = HostLock::acquire(root.path(), "lock-a");
    match second {
        Err(HostLockError::AlreadyRunning {
            identity: Some(identity),
        }) => {
            assert_eq!(identity.pid, first.identity().pid);
            assert_eq!(identity.boot_id, first.identity().boot_id);
        }
        other => panic!("expected AlreadyRunning with identity, got {other:?}"),
    }
    drop(first);
}

#[test]
#[cfg(windows)]
fn dropping_holder_permits_reacquire_and_replaces_identity() {
    let root = profile_root();
    let first = HostLock::acquire(root.path(), "lock-b").expect("first acquire");
    let first_boot = first.identity().boot_id;
    drop(first);

    let second = HostLock::acquire(root.path(), "lock-b").expect("reacquire");
    assert_ne!(second.identity().boot_id, first_boot);
    assert_eq!(second.identity().profile, "lock-b");
    assert_eq!(second.identity().protocol_major, PROTOCOL_MAJOR);
}

#[test]
#[cfg(windows)]
fn different_profiles_can_coexist() {
    let root_a = profile_root();
    let root_b = profile_root();
    let a = HostLock::acquire(root_a.path(), "profile-a").expect("acquire a");
    let b = HostLock::acquire(root_b.path(), "profile-b").expect("acquire b");
    assert_ne!(a.identity().boot_id, b.identity().boot_id);
    assert_eq!(a.identity().profile, "profile-a");
    assert_eq!(b.identity().profile, "profile-b");
}

#[test]
#[cfg(windows)]
fn lock_records_executable_and_start_time() {
    let root = profile_root();
    let lock = HostLock::acquire(root.path(), "identity-check").expect("acquire");
    let identity = lock.identity();
    assert_eq!(identity.pid, std::process::id());
    assert!(identity.process_start_time_unix_secs > 0);
    let exe = std::env::current_exe()
        .and_then(|path| path.canonicalize())
        .expect("canonical current exe");
    assert_eq!(identity.executable_path, exe);
    assert_eq!(identity.profile, "identity-check");
    assert_eq!(identity.protocol_major, PROTOCOL_MAJOR);
    assert_ne!(identity.boot_id, Uuid::nil());
}

#[test]
#[cfg(windows)]
fn stale_pid_record_is_recovered() {
    let root = profile_root();
    write_stale_identity(root.path(), &sample_identity(u32::MAX, "stale-dead"));
    let lock = HostLock::acquire(root.path(), "stale-dead").expect("recover stale");
    assert_eq!(lock.identity().pid, std::process::id());
    assert_ne!(lock.identity().boot_id, Uuid::nil());
}

#[test]
#[cfg(windows)]
fn live_unrelated_pid_is_not_killed() {
    let root = profile_root();
    let mut command = Command::new("ping");
    command
        .args(["-n", "60", "127.0.0.1"])
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut child = ChildGuard::spawn(command);
    let child_pid = child.id();
    write_stale_identity(root.path(), &sample_identity(child_pid, "stale-live"));

    let lock = HostLock::acquire(root.path(), "stale-live").expect("recover without kill");
    assert_eq!(lock.identity().pid, std::process::id());
    assert_ne!(lock.identity().pid, child_pid);

    std::thread::sleep(Duration::from_millis(50));
    let status = child.try_wait().expect("poll child");
    assert!(
        status.is_none(),
        "unrelated live PID must not be killed or signaled; exited early with {status:?}"
    );
}

#[test]
#[cfg(not(windows))]
fn acquire_is_unsupported_off_windows() {
    let root = profile_root();
    let err = HostLock::acquire(root.path(), "unsupported").expect_err("unsupported");
    assert!(matches!(err, HostLockError::Unsupported));
}
