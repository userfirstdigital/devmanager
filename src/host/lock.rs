//! Exclusive OS-backed host lock for one profile root.
//!
//! The open exclusive file handle is the ownership truth. JSON identity is
//! diagnostic metadata only and never kill or signal authority.

use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::config::paths::AppProfile;
use crate::protocol::PROTOCOL_MAJOR;

const LOCK_FILE_NAME: &str = "host.lock";
/// Maximum accepted host.lock identity JSON size (bytes).
const MAX_HOST_IDENTITY_JSON_BYTES: u64 = 64 * 1024;

/// Process exit code when another host already owns this profile lock.
///
/// Documented distinct code so generic startup failure cannot pass lock-conflict
/// acceptance.
pub const HOST_EXIT_ALREADY_RUNNING: u8 = 75;

/// Diagnostic identity written while a [`HostLock`] is held.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostIdentity {
    pub pid: u32,
    /// Raw Windows `FILETIME` creation ticks (100 ns since 1601-01-01 UTC).
    pub process_creation_filetime_ticks: u64,
    pub executable_path: PathBuf,
    pub profile: String,
    pub protocol_major: u16,
    pub boot_id: Uuid,
}

/// Errors from acquiring a [`HostLock`].
#[derive(Debug)]
pub enum HostLockError {
    /// Another live holder already owns this profile (OS lock or exact live identity).
    AlreadyRunning { identity: Option<HostIdentity> },
    /// Profile name failed validation.
    InvalidProfile(String),
    /// Filesystem failure while creating or writing the lock.
    Io(std::io::Error),
    /// Host locking is not implemented on this platform.
    Unsupported,
}

impl std::fmt::Display for HostLockError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AlreadyRunning { .. } => {
                write!(f, "another host already holds this profile lock")
            }
            Self::InvalidProfile(name) => write!(f, "invalid host profile name: {name:?}"),
            Self::Io(error) => write!(f, "host lock I/O error: {error}"),
            Self::Unsupported => write!(f, "host locking is unsupported on this platform"),
        }
    }
}

impl std::error::Error for HostLockError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::AlreadyRunning { .. } | Self::InvalidProfile(_) | Self::Unsupported => None,
        }
    }
}

/// Exclusive holder of a per-profile host lock file.
#[derive(Debug)]
pub struct HostLock {
    _file: File,
    identity: HostIdentity,
    profile_root: PathBuf,
}

impl HostLock {
    /// Acquire an exclusive OS lock under `profile_root` for `profile`.
    ///
    /// `profile_root` must be supplied explicitly; this never resolves app-data.
    pub fn acquire(profile_root: &Path, profile: &str) -> Result<Self, HostLockError> {
        let profile = validate_profile(profile)?;
        #[cfg(windows)]
        {
            acquire_windows(profile_root, profile)
        }
        #[cfg(target_os = "linux")]
        {
            acquire_linux(profile_root, profile)
        }
        #[cfg(not(any(windows, target_os = "linux")))]
        {
            let _ = profile_root;
            let _ = profile;
            Err(HostLockError::Unsupported)
        }
    }

    pub fn identity(&self) -> &HostIdentity {
        &self.identity
    }

    pub fn profile_root(&self) -> &Path {
        &self.profile_root
    }
}

fn validate_profile(profile: &str) -> Result<String, HostLockError> {
    match AppProfile::named(profile) {
        Ok(AppProfile::Named(name)) => Ok(name),
        Ok(_) => Err(HostLockError::InvalidProfile(profile.to_string())),
        Err(_) => Err(HostLockError::InvalidProfile(profile.to_string())),
    }
}

/// A wait-only OS handle to the exact host generation named by authenticated
/// Hello and its profile metadata. Metadata never grants signal/kill authority.
#[derive(Debug)]
pub struct HostExitWait {
    profile_root: PathBuf,
    #[cfg(target_os = "linux")]
    process: std::os::fd::OwnedFd,
    #[cfg(windows)]
    process: std::os::windows::io::OwnedHandle,
}

impl HostExitWait {
    pub fn capture(profile_root: &Path, profile: &str, boot_id: Uuid) -> Result<Self, String> {
        let mut options = fs::OpenOptions::new();
        options.read(true);
        #[cfg(target_os = "linux")]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
        }
        let mut file = options
            .open(lock_path(profile_root))
            .map_err(|e| e.to_string())?;
        let metadata = file.metadata().map_err(|e| e.to_string())?;
        if !metadata.is_file() {
            return Err("Host identity is not a regular file.".into());
        }
        #[cfg(target_os = "linux")]
        {
            use std::os::unix::fs::MetadataExt;
            if metadata.uid() != unsafe { libc::geteuid() }
                || metadata.nlink() != 1
                || metadata.mode() & 0o022 != 0
            {
                return Err("Host identity ownership changed.".into());
            }
        }
        let identity = read_identity_from_file(&mut file).ok_or("Host identity is unavailable.")?;
        if boot_id.is_nil()
            || identity.boot_id != boot_id
            || identity.profile != profile
            || identity.pid == 0
        {
            return Err("Host process identity does not match authenticated Hello.".into());
        }
        #[cfg(target_os = "linux")]
        {
            use std::os::fd::FromRawFd;
            let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, identity.pid, 0) };
            if fd < 0 {
                return Err(std::io::Error::last_os_error().to_string());
            }
            let process = unsafe { std::os::fd::OwnedFd::from_raw_fd(fd as i32) };
            let ticks = crate::services::platform_service::capture_process_creation_time_100ns(
                identity.pid,
            );
            let exe =
                fs::read_link(format!("/proc/{}/exe", identity.pid)).map_err(|e| e.to_string())?;
            if ticks != Some(identity.process_creation_filetime_ticks)
                || exe != identity.executable_path
            {
                return Err("Host process generation changed before update.".into());
            }
            let wait = Self {
                process,
                profile_root: profile_root.to_path_buf(),
            };
            if wait.has_exited()? {
                return Err("Host exited before update preparation.".into());
            }
            return Ok(wait);
        }
        #[cfg(windows)]
        {
            use std::os::windows::io::FromRawHandle;
            use windows::Win32::System::Threading::{
                OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE,
            };
            let handle = unsafe {
                OpenProcess(
                    PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
                    false,
                    identity.pid,
                )
            }
            .map_err(|e| e.to_string())?;
            let process = unsafe { std::os::windows::io::OwnedHandle::from_raw_handle(handle.0) };
            if process_creation_ticks(handle).map_err(|e| e.to_string())?
                != identity.process_creation_filetime_ticks
                || process_image_path(handle).map_err(|e| e.to_string())?
                    != identity
                        .executable_path
                        .canonicalize()
                        .map_err(|e| e.to_string())?
            {
                return Err("Host process generation changed before update.".into());
            }
            let wait = Self {
                process,
                profile_root: profile_root.to_path_buf(),
            };
            if wait.has_exited()? {
                return Err("Host exited before update preparation.".into());
            }
            return Ok(wait);
        }
        #[cfg(not(any(windows, target_os = "linux")))]
        Err("Host exit verification is unsupported on this platform.".into())
    }

    pub fn has_exited(&self) -> Result<bool, String> {
        #[cfg(target_os = "linux")]
        {
            use std::os::fd::AsRawFd;
            let mut descriptor = libc::pollfd {
                fd: self.process.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            let result = unsafe { libc::poll(&mut descriptor, 1, 0) };
            if result < 0 {
                return Err(std::io::Error::last_os_error().to_string());
            }
            if descriptor.revents & (libc::POLLERR | libc::POLLNVAL) != 0 {
                return Err("Host exit handle failed.".into());
            }
            return Ok(descriptor.revents & (libc::POLLIN | libc::POLLHUP) != 0);
        }
        #[cfg(windows)]
        {
            use std::os::windows::io::AsRawHandle;
            use windows::Win32::{
                Foundation::{HANDLE, WAIT_OBJECT_0, WAIT_TIMEOUT},
                System::Threading::WaitForSingleObject,
            };
            return match unsafe { WaitForSingleObject(HANDLE(self.process.as_raw_handle()), 0) } {
                WAIT_OBJECT_0 => Ok(true),
                WAIT_TIMEOUT => Ok(false),
                _ => Err(std::io::Error::last_os_error().to_string()),
            };
        }
        #[cfg(not(any(windows, target_os = "linux")))]
        Err("Host exit verification is unsupported on this platform.".into())
    }

    pub async fn wait_until(&self, deadline: std::time::Instant) -> Result<HostUpdateLock, String> {
        loop {
            if self.has_exited()? {
                return HostUpdateLock::acquire(&self.profile_root);
            }
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                return Err(
                    "The old host has not exited; the installed image was preserved.".into(),
                );
            }
            tokio::time::sleep(remaining.min(std::time::Duration::from_millis(20))).await;
        }
    }
}

/// Holds the existing profile lock without rewriting diagnostic identity while
/// the installer commits. A competing host cannot start from the old image.
#[derive(Debug)]
pub struct HostUpdateLock {
    _file: File,
}

impl HostUpdateLock {
    fn acquire(root: &Path) -> Result<Self, String> {
        let mut options = fs::OpenOptions::new();
        options.read(true).write(true);
        #[cfg(target_os = "linux")]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW);
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            options.share_mode(windows::Win32::Storage::FileSystem::FILE_SHARE_READ.0);
        }
        let file = options
            .open(lock_path(root))
            .map_err(|error| format!("Host profile was reacquired before update: {error}"))?;
        #[cfg(target_os = "linux")]
        {
            use std::os::{fd::AsRawFd, unix::fs::MetadataExt};
            let metadata = file.metadata().map_err(|e| e.to_string())?;
            if !metadata.is_file()
                || metadata.nlink() != 1
                || metadata.uid() != unsafe { libc::geteuid() }
                || metadata.mode() & 0o022 != 0
            {
                return Err("Host profile lock ownership changed before update.".into());
            }
            if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
                return Err(
                    "Another host acquired the profile before update; image preserved.".into(),
                );
            }
        }
        Ok(Self { _file: file })
    }
}

#[cfg(all(test, any(windows, target_os = "linux")))]
mod exit_wait_tests {
    use super::*;

    #[test]
    fn host_exit_wait_requires_the_authenticated_live_generation() {
        let directory = tempfile::tempdir().unwrap();
        let lock = HostLock::acquire(directory.path(), "exitidentity").unwrap();
        assert!(HostExitWait::capture(directory.path(), "exitidentity", Uuid::now_v7()).is_err());
        let wait = HostExitWait::capture(directory.path(), "exitidentity", lock.identity().boot_id)
            .unwrap();
        assert!(!wait.has_exited().unwrap());
        assert!(
            HostExitWait::capture(directory.path(), "foreign", lock.identity().boot_id).is_err()
        );
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn host_exit_wait_waits_for_physical_exit_and_reserves_profile_until_commit() {
        struct Child(std::process::Child);
        impl Drop for Child {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        let directory = tempfile::tempdir().unwrap();
        let mut child = Child(
            std::process::Command::new("/usr/bin/sleep")
                .arg("1")
                .spawn()
                .unwrap(),
        );
        let identity = HostIdentity {
            pid: child.0.id(),
            process_creation_filetime_ticks:
                crate::services::platform_service::capture_process_creation_time_100ns(child.0.id())
                    .unwrap(),
            executable_path: fs::read_link(format!("/proc/{}/exe", child.0.id())).unwrap(),
            profile: "exitwait".into(),
            protocol_major: PROTOCOL_MAJOR,
            boot_id: Uuid::now_v7(),
        };
        let bytes = serde_json::to_vec(&identity).unwrap();
        fs::write(lock_path(directory.path()), &bytes).unwrap();
        let wait = HostExitWait::capture(directory.path(), "exitwait", identity.boot_id).unwrap();
        assert!(wait
            .wait_until(std::time::Instant::now() + std::time::Duration::from_millis(1))
            .await
            .is_err());
        let reservation = wait
            .wait_until(std::time::Instant::now() + std::time::Duration::from_secs(5))
            .await
            .unwrap();
        assert!(child.0.try_wait().unwrap().is_some());
        assert!(matches!(
            HostLock::acquire(directory.path(), "exitwait"),
            Err(HostLockError::AlreadyRunning { .. })
        ));
        assert_eq!(fs::read(lock_path(directory.path())).unwrap(), bytes);
        drop(reservation);
        assert!(HostLock::acquire(directory.path(), "exitwait").is_ok());
    }
}

fn lock_path(profile_root: &Path) -> PathBuf {
    profile_root.join(LOCK_FILE_NAME)
}

fn write_identity(file: &mut File, identity: &HostIdentity) -> Result<(), HostLockError> {
    let bytes = serde_json::to_vec_pretty(identity).map_err(|error| {
        HostLockError::Io(std::io::Error::new(std::io::ErrorKind::InvalidData, error))
    })?;
    file.set_len(0).map_err(HostLockError::Io)?;
    file.seek(SeekFrom::Start(0)).map_err(HostLockError::Io)?;
    file.write_all(&bytes).map_err(HostLockError::Io)?;
    file.flush().map_err(HostLockError::Io)?;
    Ok(())
}

fn read_identity_from_file(file: &mut File) -> Option<HostIdentity> {
    file.seek(SeekFrom::Start(0)).ok()?;
    // Read at most MAX+1 bytes so oversize metadata is detected without unbounded growth.
    let mut limited = file.take(MAX_HOST_IDENTITY_JSON_BYTES.saturating_add(1));
    let mut bytes = Vec::new();
    limited.read_to_end(&mut bytes).ok()?;
    if (bytes.len() as u64) > MAX_HOST_IDENTITY_JSON_BYTES {
        return None;
    }
    if bytes.iter().all(|byte| byte.is_ascii_whitespace()) {
        return None;
    }
    serde_json::from_slice(&bytes).ok()
}

fn read_identity_file(path: &Path) -> Option<HostIdentity> {
    let mut file = File::open(path).ok()?;
    read_identity_from_file(&mut file)
}

#[cfg(windows)]
fn filetime_to_ticks(time: windows::Win32::Foundation::FILETIME) -> u64 {
    (u64::from(time.dwHighDateTime) << 32) | u64::from(time.dwLowDateTime)
}

#[cfg(windows)]
fn process_creation_ticks(
    handle: windows::Win32::Foundation::HANDLE,
) -> Result<u64, HostLockError> {
    use windows::Win32::Foundation::FILETIME;
    use windows::Win32::System::Threading::GetProcessTimes;

    let mut creation = FILETIME::default();
    let mut exit = FILETIME::default();
    let mut kernel = FILETIME::default();
    let mut user = FILETIME::default();
    unsafe { GetProcessTimes(handle, &mut creation, &mut exit, &mut kernel, &mut user) }.map_err(
        |error| {
            HostLockError::Io(std::io::Error::new(
                std::io::ErrorKind::Other,
                format!("GetProcessTimes failed: {error}"),
            ))
        },
    )?;
    let ticks = filetime_to_ticks(creation);
    if ticks == 0 {
        return Err(HostLockError::Io(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "process creation FILETIME ticks unavailable",
        )));
    }
    Ok(ticks)
}

#[cfg(windows)]
fn process_image_path(
    handle: windows::Win32::Foundation::HANDLE,
) -> Result<PathBuf, HostLockError> {
    use std::ffi::OsString;
    use std::os::windows::ffi::OsStringExt;

    use windows::core::PWSTR;
    use windows::Win32::Foundation::ERROR_INSUFFICIENT_BUFFER;
    use windows::Win32::System::Threading::{QueryFullProcessImageNameW, PROCESS_NAME_WIN32};

    // Windows extended-path ceiling for a Win32 image path.
    const MAX_IMAGE_PATH_CHARS: usize = 32_767;
    let mut capacity = 260usize;

    loop {
        if capacity == 0 || capacity > MAX_IMAGE_PATH_CHARS {
            return Err(HostLockError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "QueryFullProcessImageNameW buffer capacity out of range",
            )));
        }

        let mut buffer = vec![0u16; capacity];
        let mut size = capacity as u32;
        match unsafe {
            QueryFullProcessImageNameW(
                handle,
                PROCESS_NAME_WIN32,
                PWSTR(buffer.as_mut_ptr()),
                &mut size,
            )
        } {
            Ok(()) => {
                let returned = size as usize;
                if returned == 0 || returned > buffer.len() {
                    return Err(HostLockError::Io(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        format!("QueryFullProcessImageNameW success size out of range: {returned}"),
                    )));
                }
                let path = OsString::from_wide(&buffer[..returned]);
                return PathBuf::from(path)
                    .canonicalize()
                    .map_err(HostLockError::Io);
            }
            Err(error) => {
                if error.code() != ERROR_INSUFFICIENT_BUFFER.to_hresult() {
                    return Err(HostLockError::Io(std::io::Error::new(
                        std::io::ErrorKind::Other,
                        format!("QueryFullProcessImageNameW failed: {error}"),
                    )));
                }
                // Microsoft documents lpdwSize as meaningful only on success.
                // Do not trust size after ERROR_INSUFFICIENT_BUFFER.
                if capacity >= MAX_IMAGE_PATH_CHARS {
                    return Err(HostLockError::Io(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "QueryFullProcessImageNameW exhausted Windows path buffer maximum",
                    )));
                }
                capacity = capacity
                    .checked_mul(2)
                    .map(|grown| grown.min(MAX_IMAGE_PATH_CHARS))
                    .unwrap_or(MAX_IMAGE_PATH_CHARS);
            }
        }
    }
}

#[cfg(windows)]
fn build_identity(profile: String) -> Result<HostIdentity, HostLockError> {
    use windows::Win32::System::Threading::GetCurrentProcess;

    let current = unsafe { GetCurrentProcess() };
    let process_creation_filetime_ticks = process_creation_ticks(current)?;
    let executable_path = process_image_path(current)?;
    Ok(HostIdentity {
        pid: std::process::id(),
        process_creation_filetime_ticks,
        executable_path,
        profile,
        protocol_major: PROTOCOL_MAJOR,
        boot_id: Uuid::now_v7(),
    })
}

/// Returns `Ok(true)` when `prior` exactly names a live process generation for
/// the normalized requested profile.
/// Returns `Ok(false)` when the PID is absent, has already exited, its
/// generation/path differs, or its profile does not match the requested
/// profile (stale for this acquire).
/// Returns `Err` when a live same-profile PID cannot be verified fail-closed.
///
/// An exited PID whose process object is still queryable (a peer still holds
/// a child handle) is stale, not an I/O error. Isolated debug keeps that
/// handle after the host dies; treating image-path failure as live contention
/// leaves the next host unable to acquire `host.lock`.
#[cfg(windows)]
fn prior_identity_names_live_process(
    prior: &HostIdentity,
    requested_profile: &str,
) -> Result<bool, HostLockError> {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Threading::{
        GetProcessId, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE,
    };

    let prior_profile = match AppProfile::named(&prior.profile) {
        Ok(AppProfile::Named(name)) => name,
        _ => return Ok(false),
    };
    if prior_profile != requested_profile {
        return Ok(false);
    }

    if prior.pid == 0 || prior.process_creation_filetime_ticks == 0 {
        return Ok(false);
    }

    let handle = match unsafe {
        OpenProcess(
            PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
            false,
            prior.pid,
        )
    } {
        Ok(handle) => handle,
        Err(_) => match unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, prior.pid) }
        {
            Ok(handle) => handle,
            Err(error) => {
                // Fail closed when the PID still appears live but cannot be queried.
                let mut system = sysinfo::System::new();
                system.refresh_processes(sysinfo::ProcessesToUpdate::All, true);
                if system.process(sysinfo::Pid::from_u32(prior.pid)).is_some() {
                    return Err(HostLockError::Io(std::io::Error::new(
                        std::io::ErrorKind::PermissionDenied,
                        format!("unable to verify prior host pid {}: {error}", prior.pid),
                    )));
                }
                // Absent / invalid PID => stale metadata.
                return Ok(false);
            }
        },
    };

    struct HandleGuard(windows::Win32::Foundation::HANDLE);
    impl Drop for HandleGuard {
        fn drop(&mut self) {
            unsafe {
                let _ = CloseHandle(self.0);
            }
        }
    }
    let guard = HandleGuard(handle);

    if prior_process_has_exited(guard.0) {
        return Ok(false);
    }

    let live_pid = unsafe { GetProcessId(guard.0) };
    if live_pid == 0 || live_pid != prior.pid {
        return Ok(false);
    }

    let live_ticks = match process_creation_ticks(guard.0) {
        Ok(ticks) => ticks,
        Err(_) => {
            if prior_process_has_exited(guard.0) {
                return Ok(false);
            }
            return Err(HostLockError::Io(std::io::Error::new(
                std::io::ErrorKind::Other,
                format!(
                    "unable to verify creation ticks for prior host pid {}",
                    prior.pid
                ),
            )));
        }
    };
    let live_exe = match process_image_path(guard.0) {
        Ok(path) => path,
        Err(_) => {
            if prior_process_has_exited(guard.0) {
                return Ok(false);
            }
            return Err(HostLockError::Io(std::io::Error::new(
                std::io::ErrorKind::Other,
                format!(
                    "unable to verify executable path for prior host pid {}",
                    prior.pid
                ),
            )));
        }
    };

    let prior_exe = match prior.executable_path.canonicalize() {
        Ok(path) => path,
        Err(_) => prior.executable_path.clone(),
    };

    Ok(live_ticks == prior.process_creation_filetime_ticks && live_exe == prior_exe)
}

/// True when the opened process object has already exited.
///
/// `WaitForSingleObject` needs `PROCESS_SYNCHRONIZE`. `GetExitCodeProcess`
/// works with query-limited access and covers the handle we opened after
/// that right was denied. Exit code 259 (`STILL_ACTIVE`) is treated as live.
#[cfg(windows)]
fn prior_process_has_exited(handle: windows::Win32::Foundation::HANDLE) -> bool {
    use windows::Win32::Foundation::{WAIT_OBJECT_0, WAIT_TIMEOUT};
    use windows::Win32::System::Threading::{GetExitCodeProcess, WaitForSingleObject};

    const STILL_ACTIVE: u32 = 259;

    let wait = unsafe { WaitForSingleObject(handle, 0) };
    if wait == WAIT_OBJECT_0 {
        return true;
    }
    if wait == WAIT_TIMEOUT {
        return false;
    }

    let mut exit_code = 0u32;
    unsafe { GetExitCodeProcess(handle, &mut exit_code) }.is_ok() && exit_code != STILL_ACTIVE
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use std::process::{Command, Stdio};

    #[test]
    fn exited_prior_pid_with_held_child_handle_is_stale_not_io_error() {
        let dir = tempfile::tempdir().expect("profile root");
        let profile = "lock-stale-zombie";
        let mut child = Command::new("cmd.exe")
            .args(["/C", "ping", "-n", "30", "127.0.0.1"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn placeholder prior pid");
        let identity = HostIdentity {
            pid: child.id(),
            process_creation_filetime_ticks: 1,
            executable_path: PathBuf::from(r"C:\Windows\System32\cmd.exe"),
            profile: profile.to_string(),
            protocol_major: PROTOCOL_MAJOR,
            boot_id: Uuid::now_v7(),
        };
        std::fs::write(
            dir.path().join(LOCK_FILE_NAME),
            serde_json::to_vec_pretty(&identity).expect("identity json"),
        )
        .expect("write stale host.lock");
        child.kill().expect("kill placeholder prior pid");
        let _ = child.wait();
        let lock = HostLock::acquire(dir.path(), profile).unwrap_or_else(|error| {
            panic!("exited prior pid must be stale even while the child handle is held: {error}")
        });
        assert_eq!(lock.identity().profile, profile);
        drop(child);
    }
}

#[cfg(windows)]
fn acquire_windows(profile_root: &Path, profile: String) -> Result<HostLock, HostLockError> {
    use std::os::windows::fs::OpenOptionsExt;
    use windows::Win32::Storage::FileSystem::FILE_SHARE_READ;

    // Holders keep read+write access, but allow peers to open for read so
    // AlreadyRunning can surface diagnostic identity. Competing write/delete
    // opens remain denied (no FILE_SHARE_WRITE / FILE_SHARE_DELETE).
    const HOST_LOCK_SHARE_MODE: u32 = FILE_SHARE_READ.0;

    fs::create_dir_all(profile_root).map_err(HostLockError::Io)?;
    let path = lock_path(profile_root);

    let open_exclusive = || {
        std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .share_mode(HOST_LOCK_SHARE_MODE)
            .open(&path)
    };

    let mut file = match open_exclusive() {
        Ok(file) => file,
        Err(error) if is_sharing_violation(&error) => {
            return Err(HostLockError::AlreadyRunning {
                identity: read_identity_file(&path),
            });
        }
        Err(error) => return Err(HostLockError::Io(error)),
    };

    if let Some(prior) = read_identity_from_file(&mut file) {
        if prior_identity_names_live_process(&prior, &profile)? {
            // Same-process reacquire after dropping the previous exclusive handle
            // remains allowed; any other exact live identity fails closed.
            if prior.pid != std::process::id() {
                return Err(HostLockError::AlreadyRunning {
                    identity: Some(prior),
                });
            }
        }
    }

    let identity = build_identity(profile)?;
    write_identity(&mut file, &identity)?;
    Ok(HostLock {
        _file: file,
        identity,
        profile_root: profile_root.canonicalize().map_err(HostLockError::Io)?,
    })
}

#[cfg(windows)]
fn is_sharing_violation(error: &std::io::Error) -> bool {
    matches!(error.raw_os_error(), Some(32 | 33))
}

#[cfg(target_os = "linux")]
fn acquire_linux(profile_root: &Path, profile: String) -> Result<HostLock, HostLockError> {
    use std::os::fd::{AsRawFd, FromRawFd};
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    let io_error = |message| {
        HostLockError::Io(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            message,
        ))
    };
    crate::persistence::create_private_directory(profile_root, true).map_err(HostLockError::Io)?;
    let directory = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(profile_root)
        .map_err(HostLockError::Io)?;
    let metadata = directory.metadata().map_err(HostLockError::Io)?;
    if metadata.uid() != unsafe { libc::geteuid() } || metadata.mode() & 0o022 != 0 {
        return Err(io_error(
            "host profile directory must be owned by this user and not writable by others",
        ));
    }
    // The retained directory owns the lookup. Never follow a substituted lock
    // symlink or truncate diagnostic metadata before obtaining the OS lock.
    let fd = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            c"host.lock".as_ptr(),
            libc::O_RDWR | libc::O_CREAT | libc::O_CLOEXEC | libc::O_NOFOLLOW,
            0o600,
        )
    };
    if fd < 0 {
        return Err(HostLockError::Io(std::io::Error::last_os_error()));
    }
    let mut file = unsafe { File::from_raw_fd(fd) };
    let metadata = file.metadata().map_err(HostLockError::Io)?;
    if !metadata.is_file()
        || metadata.nlink() != 1
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o022 != 0
    {
        return Err(io_error(
            "host lock must be a private regular file with one link",
        ));
    }
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        let error = std::io::Error::last_os_error();
        return Err(if error.kind() == std::io::ErrorKind::WouldBlock {
            HostLockError::AlreadyRunning {
                identity: read_identity_from_file(&mut file),
            }
        } else {
            HostLockError::Io(error)
        });
    }
    let creation =
        crate::services::platform_service::capture_process_creation_time_100ns(std::process::id())
            .ok_or_else(|| io_error("cannot observe host process generation"))?;
    let identity = HostIdentity {
        pid: std::process::id(),
        process_creation_filetime_ticks: creation,
        executable_path: std::env::current_exe()
            .and_then(|p| p.canonicalize())
            .map_err(HostLockError::Io)?,
        profile,
        protocol_major: PROTOCOL_MAJOR,
        boot_id: Uuid::now_v7(),
    };
    write_identity(&mut file, &identity)?;
    Ok(HostLock {
        _file: file,
        identity,
        profile_root: profile_root.canonicalize().map_err(HostLockError::Io)?,
    })
}

#[cfg(all(test, target_os = "linux"))]
mod linux_tests {
    use super::*;
    #[test]
    fn linux_new_profile_is_private_and_existing_directory_permissions_are_preserved() {
        use std::os::unix::fs::PermissionsExt;
        let parent = tempfile::tempdir().unwrap();
        fs::set_permissions(parent.path(), fs::Permissions::from_mode(0o750)).unwrap();
        let root = parent.path().join("new-profile");
        let lock = HostLock::acquire(&root, "private-profile").unwrap();
        assert_eq!(
            fs::metadata(&root).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(parent.path()).unwrap().permissions().mode() & 0o777,
            0o750
        );
        drop(lock);
        fs::set_permissions(&root, fs::Permissions::from_mode(0o770)).unwrap();
        assert!(HostLock::acquire(&root, "private-profile").is_err());
        assert_eq!(
            fs::metadata(&root).unwrap().permissions().mode() & 0o777,
            0o770
        );
    }

    #[test]
    fn linux_host_lock_retains_exclusion_and_rejects_link_aliases() {
        let root = tempfile::tempdir().unwrap();
        let first = HostLock::acquire(root.path(), "linux-lock").unwrap();
        assert!(matches!(
            HostLock::acquire(root.path(), "linux-lock"),
            Err(HostLockError::AlreadyRunning { .. })
        ));
        let identity = fs::read(root.path().join(LOCK_FILE_NAME)).unwrap();
        assert_eq!(
            serde_json::from_slice::<HostIdentity>(&identity)
                .unwrap()
                .boot_id,
            first.identity().boot_id
        );
        drop(first);
        let second = HostLock::acquire(root.path(), "linux-lock").unwrap();
        drop(second);
        let lock = root.path().join(LOCK_FILE_NAME);
        let alias = root.path().join("alias");
        fs::hard_link(&lock, &alias).unwrap();
        assert!(HostLock::acquire(root.path(), "linux-lock").is_err());
        fs::remove_file(&alias).unwrap();
        fs::remove_file(&lock).unwrap();
        std::os::unix::fs::symlink("alias", &lock).unwrap();
        assert!(HostLock::acquire(root.path(), "linux-lock").is_err());
        assert!(!alias.exists());
    }
}
