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

/// Diagnostic identity written while a [`HostLock`] is held.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostIdentity {
    pub pid: u32,
    pub process_start_time_unix_secs: u64,
    pub executable_path: PathBuf,
    pub profile: String,
    pub protocol_major: u16,
    pub boot_id: Uuid,
}

/// Errors from acquiring a [`HostLock`].
#[derive(Debug)]
pub enum HostLockError {
    /// Another live holder already owns the exclusive OS lock for this root.
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

fn build_identity(profile: String) -> Result<HostIdentity, HostLockError> {
    let pid = std::process::id();
    let process_start_time_unix_secs = current_process_start_time_unix_secs()?;
    let executable_path = std::env::current_exe()
        .and_then(|path| path.canonicalize())
        .map_err(HostLockError::Io)?;
    Ok(HostIdentity {
        pid,
        process_start_time_unix_secs,
        executable_path,
        profile,
        protocol_major: PROTOCOL_MAJOR,
        boot_id: Uuid::now_v7(),
    })
}

fn current_process_start_time_unix_secs() -> Result<u64, HostLockError> {
    let mut system = sysinfo::System::new();
    system.refresh_processes(sysinfo::ProcessesToUpdate::All, true);
    let pid = sysinfo::Pid::from_u32(std::process::id());
    system
        .process(pid)
        .map(|process| process.start_time())
        .filter(|start| *start > 0)
        .ok_or_else(|| {
            HostLockError::Io(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "current process start time unavailable",
            ))
        })
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

fn read_identity_file(path: &Path) -> Option<HostIdentity> {
    let mut file = File::open(path).ok()?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).ok()?;
    serde_json::from_slice(&bytes).ok()
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
