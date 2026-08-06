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
        #[cfg(not(windows))]
        {
            let _ = profile_root;
            let _ = profile;
            Err(HostLockError::Unsupported)
        }
    }

    pub fn identity(&self) -> &HostIdentity {
        &self.identity
    }
}

fn validate_profile(profile: &str) -> Result<String, HostLockError> {
    match AppProfile::named(profile) {
        Ok(AppProfile::Named(name)) => Ok(name),
        Ok(_) => Err(HostLockError::InvalidProfile(profile.to_string())),
        Err(_) => Err(HostLockError::InvalidProfile(profile.to_string())),
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
/// Returns `Ok(false)` when the PID is absent, its generation/path differs, or
/// its profile does not match the requested profile (stale for this acquire).
/// Returns `Err` when a live same-profile PID cannot be verified fail-closed.
#[cfg(windows)]
fn prior_identity_names_live_process(
    prior: &HostIdentity,
    requested_profile: &str,
) -> Result<bool, HostLockError> {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Threading::{
        GetProcessId, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
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

    let handle = match unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, prior.pid) } {
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

    let live_pid = unsafe { GetProcessId(guard.0) };
    if live_pid == 0 || live_pid != prior.pid {
        return Ok(false);
    }

    let live_ticks = match process_creation_ticks(guard.0) {
        Ok(ticks) => ticks,
        Err(_) => {
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
    })
}

#[cfg(windows)]
fn is_sharing_violation(error: &std::io::Error) -> bool {
    matches!(error.raw_os_error(), Some(32 | 33))
}
