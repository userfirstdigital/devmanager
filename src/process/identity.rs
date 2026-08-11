use std::fmt;
use std::io;
use std::path::{Path, PathBuf};

use crate::domain::id::TaskId;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ManagedProcessIdError {
    ZeroPid,
    ZeroCreationTime,
}

impl fmt::Display for ManagedProcessIdError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ZeroPid => write!(f, "managed process PID must be non-zero"),
            Self::ZeroCreationTime => {
                write!(f, "managed process creation time must be non-zero")
            }
        }
    }
}

impl std::error::Error for ManagedProcessIdError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ManagedProcessId {
    pid: u32,
    creation_time_100ns: u64,
}

impl ManagedProcessId {
    pub fn new(pid: u32, creation_time_100ns: u64) -> Result<Self, ManagedProcessIdError> {
        if pid == 0 {
            return Err(ManagedProcessIdError::ZeroPid);
        }
        if creation_time_100ns == 0 {
            return Err(ManagedProcessIdError::ZeroCreationTime);
        }
        Ok(Self {
            pid,
            creation_time_100ns,
        })
    }

    pub fn pid(&self) -> u32 {
        self.pid
    }

    pub fn creation_time_100ns(&self) -> u64 {
        self.creation_time_100ns
    }
}

#[derive(Debug)]
pub struct ManagedProcessIdentityError {
    path: PathBuf,
    source: io::Error,
}

impl ManagedProcessIdentityError {
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn kind(&self) -> io::ErrorKind {
        self.source.kind()
    }
}

impl fmt::Display for ManagedProcessIdentityError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "failed to canonicalize managed process executable: {}",
            self.source
        )
    }
}

impl std::error::Error for ManagedProcessIdentityError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManagedProcessIdentity {
    id: ManagedProcessId,
    canonical_executable: PathBuf,
}

impl ManagedProcessIdentity {
    pub fn new(
        id: ManagedProcessId,
        executable: impl Into<PathBuf>,
    ) -> Result<Self, ManagedProcessIdentityError> {
        let executable = executable.into();
        let canonical_executable =
            std::fs::canonicalize(&executable).map_err(|source| ManagedProcessIdentityError {
                path: executable,
                source,
            })?;
        Ok(Self {
            id,
            canonical_executable,
        })
    }

    pub fn id(&self) -> ManagedProcessId {
        self.id
    }

    pub fn canonical_executable(&self) -> &Path {
        &self.canonical_executable
    }

    pub fn matches_root(&self, other: &Self) -> bool {
        self.id == other.id && self.canonical_executable == other.canonical_executable
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ProcessOwner {
    Task(TaskId),
    Host,
}
