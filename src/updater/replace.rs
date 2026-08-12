//! Retryable staged two-binary replacement with backups and rollback.
//!
//! Replaces `devmanager.exe` and `devmanager-host.exe` as one atomic pair.
//! Failure or interruption restores both backups; ready installer bytes are
//! never consumed by this module.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use super::handoff::{extract_build_version, AtomicBundleError, AtomicInstallerBundle};

const CLIENT_EXE: &str = "devmanager.exe";
const HOST_EXE: &str = "devmanager-host.exe";
const CLIENT_BACKUP_SUFFIX: &str = ".devmanager-update.bak";
const HOST_BACKUP_SUFFIX: &str = ".devmanager-host-update.bak";
const STAGE_MARKER: &str = "devmanager-update-stage.json";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StagedReplaceError {
    Io { detail: String },
    Bundle(AtomicBundleError),
    MissingStagedBinary { name: String },
    IdentityMismatch { detail: String },
    Interrupted { detail: String },
    RollbackFailed { detail: String },
}

impl std::fmt::Display for StagedReplaceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io { detail } => write!(f, "staged replace I/O error: {detail}"),
            Self::Bundle(error) => write!(f, "{error}"),
            Self::MissingStagedBinary { name } => {
                write!(f, "staged payload is missing `{name}`")
            }
            Self::IdentityMismatch { detail } => {
                write!(f, "staged binary identity mismatch: {detail}")
            }
            Self::Interrupted { detail } => write!(f, "staged replace interrupted: {detail}"),
            Self::RollbackFailed { detail } => {
                write!(f, "staged replace rollback failed: {detail}")
            }
        }
    }
}

impl std::error::Error for StagedReplaceError {}

/// Paths involved in one staged client+host replacement attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StagedBinaryReplacement {
    pub install_dir: PathBuf,
    pub staged_dir: PathBuf,
    pub expected: AtomicInstallerBundle,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StagedReplacePhase {
    Validated,
    BackedUp,
    ClientReplaced,
    HostReplaced,
    Committed,
    RolledBack,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StagedReplaceProgress {
    pub phase: StagedReplacePhase,
    pub client_backup: PathBuf,
    pub host_backup: PathBuf,
}

impl StagedBinaryReplacement {
    pub fn new(
        install_dir: impl Into<PathBuf>,
        staged_dir: impl Into<PathBuf>,
        expected: AtomicInstallerBundle,
    ) -> Self {
        Self {
            install_dir: install_dir.into(),
            staged_dir: staged_dir.into(),
            expected,
        }
    }

    pub fn client_target(&self) -> PathBuf {
        self.install_dir.join(CLIENT_EXE)
    }

    pub fn host_target(&self) -> PathBuf {
        self.install_dir.join(HOST_EXE)
    }

    pub fn client_staged(&self) -> PathBuf {
        self.staged_dir.join(CLIENT_EXE)
    }

    pub fn host_staged(&self) -> PathBuf {
        self.staged_dir.join(HOST_EXE)
    }

    pub fn client_backup(&self) -> PathBuf {
        self.install_dir
            .join(format!("{CLIENT_EXE}{CLIENT_BACKUP_SUFFIX}"))
    }

    pub fn host_backup(&self) -> PathBuf {
        self.install_dir
            .join(format!("{HOST_EXE}{HOST_BACKUP_SUFFIX}"))
    }

    pub fn stage_marker_path(&self) -> PathBuf {
        self.install_dir.join(STAGE_MARKER)
    }

    /// Validate staged payload contains both binaries with matching identity.
    /// Does not mutate installed binaries or consume caller-owned ready bytes.
    pub fn validate_staged_payload(&self) -> Result<(), StagedReplaceError> {
        super::handoff::assert_atomic_installer_bundle(&self.expected)
            .map_err(StagedReplaceError::Bundle)?;
        for (path, name) in [
            (self.client_staged(), CLIENT_EXE),
            (self.host_staged(), HOST_EXE),
        ] {
            if !path.is_file() {
                return Err(StagedReplaceError::MissingStagedBinary {
                    name: name.to_string(),
                });
            }
        }
        let client_version = read_optional_product_version(&self.client_staged());
        let host_version = read_optional_product_version(&self.host_staged());
        match (client_version.as_deref(), host_version.as_deref()) {
            (Some(client), Some(host)) if client == host => {
                let expected = extract_build_version(&self.expected.client_build);
                if expected != Some(client) {
                    return Err(StagedReplaceError::IdentityMismatch {
                        detail: format!(
                            "staged binaries report {client} but bundle expects {}",
                            self.expected.version
                        ),
                    });
                }
            }
            (Some(client), Some(host)) => {
                return Err(StagedReplaceError::IdentityMismatch {
                    detail: format!("staged client={client} host={host}"),
                });
            }
            // Non-Windows / unsigned test fixtures may omit VERSIONINFO.
            _ => {}
        }
        Ok(())
    }

    /// Run one retryable replacement attempt. On failure restores backups.
    pub fn replace_with_rollback(&self) -> Result<StagedReplaceProgress, StagedReplaceError> {
        self.prepare_durable_backups()?;
        self.commit_after_durable_backups()
    }

    /// Write recoverable stage marker and durable backups before seal.
    ///
    /// Does not mutate installed binaries beyond creating `.bak` copies.
    pub fn prepare_durable_backups(&self) -> Result<StagedReplaceProgress, StagedReplaceError> {
        self.validate_staged_payload()?;
        let client_backup = self.client_backup();
        let host_backup = self.host_backup();
        write_stage_marker(&self.stage_marker_path(), "validated")?;
        backup_file(&self.client_target(), &client_backup)?;
        backup_file(&self.host_target(), &host_backup)?;
        write_stage_marker(&self.stage_marker_path(), "backed_up")?;
        Ok(StagedReplaceProgress {
            phase: StagedReplacePhase::BackedUp,
            client_backup,
            host_backup,
        })
    }

    /// Commit client+host replacement after durable backups exist.
    ///
    /// Call only after the handoff FSM has sealed irreversibility.
    pub fn commit_after_durable_backups(
        &self,
    ) -> Result<StagedReplaceProgress, StagedReplaceError> {
        let client_backup = self.client_backup();
        let host_backup = self.host_backup();
        let marker = fs::read_to_string(self.stage_marker_path()).map_err(io_err)?;
        if marker.trim() != "backed_up" {
            return Err(StagedReplaceError::Interrupted {
                detail: format!(
                    "commit requires durable backed_up marker, found `{}`",
                    marker.trim()
                ),
            });
        }

        if let Err(error) = replace_file(&self.client_staged(), &self.client_target()) {
            self.rollback_from_backups(&client_backup, &host_backup)?;
            return Err(error);
        }
        write_stage_marker(&self.stage_marker_path(), "client_replaced")?;

        if let Err(error) = replace_file(&self.host_staged(), &self.host_target()) {
            self.rollback_from_backups(&client_backup, &host_backup)?;
            return Err(error);
        }
        write_stage_marker(&self.stage_marker_path(), "host_replaced")?;

        let _ = fs::remove_file(self.stage_marker_path());
        let _ = fs::remove_file(&client_backup);
        let _ = fs::remove_file(&host_backup);
        Ok(StagedReplaceProgress {
            phase: StagedReplacePhase::Committed,
            client_backup,
            host_backup,
        })
    }

    /// Resume after interruption: if a stage marker remains, restore old host/client.
    pub fn recover_interrupted(&self) -> Result<StagedReplacePhase, StagedReplaceError> {
        let marker = self.stage_marker_path();
        if !marker.exists() {
            return Ok(StagedReplacePhase::Committed);
        }
        let phase = fs::read_to_string(&marker).map_err(io_err)?;
        match phase.trim() {
            "validated" => {
                let _ = fs::remove_file(&marker);
                Ok(StagedReplacePhase::Validated)
            }
            "backed_up" | "client_replaced" | "host_replaced" => {
                self.rollback_from_backups(&self.client_backup(), &self.host_backup())?;
                Ok(StagedReplacePhase::RolledBack)
            }
            other => Err(StagedReplaceError::Interrupted {
                detail: format!("unknown stage marker `{other}`"),
            }),
        }
    }

    fn rollback_from_backups(
        &self,
        client_backup: &Path,
        host_backup: &Path,
    ) -> Result<(), StagedReplaceError> {
        let mut errors = Vec::new();
        if client_backup.is_file() {
            if let Err(error) = replace_file(client_backup, &self.client_target()) {
                errors.push(format!("client: {error}"));
            }
        }
        if host_backup.is_file() {
            if let Err(error) = replace_file(host_backup, &self.host_target()) {
                errors.push(format!("host: {error}"));
            }
        }
        let _ = fs::remove_file(self.stage_marker_path());
        if errors.is_empty() {
            Ok(())
        } else {
            Err(StagedReplaceError::RollbackFailed {
                detail: errors.join("; "),
            })
        }
    }
}

fn backup_file(source: &Path, backup: &Path) -> Result<(), StagedReplaceError> {
    if source.is_file() {
        fs::copy(source, backup).map_err(io_err)?;
    }
    Ok(())
}

fn replace_file(source: &Path, destination: &Path) -> Result<(), StagedReplaceError> {
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent).map_err(io_err)?;
    }
    match fs::rename(source, destination) {
        Ok(()) => Ok(()),
        Err(_) => {
            fs::copy(source, destination).map_err(io_err)?;
            let _ = fs::remove_file(source);
            Ok(())
        }
    }
}

fn write_stage_marker(path: &Path, phase: &str) -> Result<(), StagedReplaceError> {
    fs::write(path, phase).map_err(io_err)
}

fn io_err(error: io::Error) -> StagedReplaceError {
    StagedReplaceError::Io {
        detail: error.to_string(),
    }
}

fn read_optional_product_version(path: &Path) -> Option<String> {
    #[cfg(windows)]
    {
        super::read_binary_product_version_string(path)
    }
    #[cfg(not(windows))]
    {
        let _ = path;
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::updater::handoff::AtomicInstallerBundle;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_pair() -> (PathBuf, PathBuf, AtomicInstallerBundle) {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("devmanager-staged-replace-{nanos}"));
        let install = root.join("install");
        let staged = root.join("staged");
        fs::create_dir_all(&install).unwrap();
        fs::create_dir_all(&staged).unwrap();
        fs::write(install.join(CLIENT_EXE), b"old-client").unwrap();
        fs::write(install.join(HOST_EXE), b"old-host").unwrap();
        fs::write(staged.join(CLIENT_EXE), b"new-client").unwrap();
        fs::write(staged.join(HOST_EXE), b"new-host").unwrap();
        let bundle = crate::updater::handoff::AtomicInstallerBundle::from_verified_download(
            crate::updater::handoff::VerifiedPackagerDownload::new(
                "0.4.2",
                format!("sha256:{}", "a".repeat(64)),
                "windows-x86_64",
                "https://example.com/devmanager-0.4.2.zip",
                "dGVzdC1zaWduYXR1cmUtcGF5bG9hZA",
                "zip",
            ),
            1,
            0,
            "devmanager/0.4.2",
            "devmanager-host/0.4.2",
        )
        .expect("bundle");
        (install, staged, bundle)
    }

    #[test]
    fn successful_replace_commits_both_binaries() {
        let (install, staged, bundle) = temp_pair();
        let replacement = StagedBinaryReplacement::new(&install, &staged, bundle);
        let progress = replacement.replace_with_rollback().expect("replace");
        assert_eq!(progress.phase, StagedReplacePhase::Committed);
        assert_eq!(fs::read(install.join(CLIENT_EXE)).unwrap(), b"new-client");
        assert_eq!(fs::read(install.join(HOST_EXE)).unwrap(), b"new-host");
        assert!(!replacement.stage_marker_path().exists());
        let _ = fs::remove_dir_all(install.parent().unwrap());
    }

    #[test]
    fn missing_staged_host_does_not_mutate_install() {
        let (install, staged, bundle) = temp_pair();
        fs::remove_file(staged.join(HOST_EXE)).unwrap();
        let replacement = StagedBinaryReplacement::new(&install, &staged, bundle);
        assert!(replacement.replace_with_rollback().is_err());
        assert_eq!(fs::read(install.join(CLIENT_EXE)).unwrap(), b"old-client");
        assert_eq!(fs::read(install.join(HOST_EXE)).unwrap(), b"old-host");
        let _ = fs::remove_dir_all(install.parent().unwrap());
    }

    #[test]
    fn interrupted_marker_restores_old_binaries() {
        let (install, staged, bundle) = temp_pair();
        let replacement = StagedBinaryReplacement::new(&install, &staged, bundle);
        fs::copy(install.join(CLIENT_EXE), replacement.client_backup()).unwrap();
        fs::copy(install.join(HOST_EXE), replacement.host_backup()).unwrap();
        fs::write(install.join(CLIENT_EXE), b"partial-client").unwrap();
        fs::write(replacement.stage_marker_path(), "client_replaced").unwrap();
        assert_eq!(
            replacement.recover_interrupted().unwrap(),
            StagedReplacePhase::RolledBack
        );
        assert_eq!(fs::read(install.join(CLIENT_EXE)).unwrap(), b"old-client");
        assert_eq!(fs::read(install.join(HOST_EXE)).unwrap(), b"old-host");
        let _ = fs::remove_dir_all(install.parent().unwrap());
    }
}
