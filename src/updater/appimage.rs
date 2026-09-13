//! Whole-AppImage identity and atomic replacement for Linux.
//!
//! The signed artifact is a standard type-2 AppImage with a bounded identity
//! trailer. Its SquashFS contains both product binaries. The trailer is covered
//! by the existing packager signature and binds their build/protocol identities
//! to the payload hash; it is never an alternative to signature verification.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use super::handoff::AtomicInstallerBundle;

const MAGIC: &[u8; 16] = b"DEVMANAGER-AI-V1";
const MAX_MANIFEST_BYTES: usize = 64 * 1024;
const MAX_IMAGE_BYTES: u64 = 2 * 1024 * 1024 * 1024;
const STAGED_IMAGE: &str = "replacement.AppImage";
const JOURNAL: &str = "image-replacement.json";
static PENDING_RESTART: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();

/// Called after the native application has released its window and client
/// runtime. exec preserves the PID and replaces the old mount's executable.
/// No detached launcher or timer can race old-client teardown.
pub fn restart_after_native_shutdown() -> Result<(), String> {
    use std::os::unix::process::CommandExt;
    let Some(image) = PENDING_RESTART.get() else {
        return Ok(());
    };
    restart_committed_image(image, |path| {
        std::process::Command::new(path)
            .args(std::env::args_os().skip(1))
            .env_remove("APPDIR")
            .env_remove("APPIMAGE")
            .env_remove("ARGV0")
            .env_remove("OWD")
            .exec()
    })
}

fn restart_committed_image(
    image: &Path,
    launch: impl FnOnce(&Path) -> std::io::Error,
) -> Result<(), String> {
    let mut replacement = AppImageReplacement::open(image)?;
    if !replacement.read_journal()? {
        return Err("The installed update is missing its restart journal.".into());
    }
    replacement.require_committed()?;
    // The lock descriptor is CLOEXEC: the new host can acquire it only after
    // exec succeeds. If exec fails, this owner can still restore the exact pair.
    let error = launch(image);
    replacement.rollback_committed()?;
    Err(format!(
        "The update could not start ({error}). The previous AppImage was restored; open it again."
    ))
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppImageManifest {
    pub schema: u32,
    pub version: String,
    pub platform: String,
    pub client_build: String,
    pub host_build: String,
    pub protocol_major: u16,
    pub protocol_minor: u16,
    pub payload_sha256: String,
    pub client_sha256: String,
    pub host_sha256: String,
}

fn sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn valid_digest(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

impl AppImageManifest {
    fn validate(&self) -> Result<(), String> {
        if self.schema != 1
            || !matches!(self.platform.as_str(), "linux-x86_64" | "linux-aarch64")
            || self.client_build != format!("devmanager/{}", self.version)
            || self.host_build != format!("devmanager-host/{}", self.version)
            || cargo_packager_updater::semver::Version::parse(&self.version).is_err()
            || !valid_digest(&self.payload_sha256)
            || !valid_digest(&self.client_sha256)
            || !valid_digest(&self.host_sha256)
        {
            return Err("Invalid AppImage identity manifest.".into());
        }
        Ok(())
    }

    pub fn require_bundle(&self, bundle: &AtomicInstallerBundle) -> Result<(), String> {
        super::assert_atomic_installer_bundle(bundle).map_err(|error| error.to_string())?;
        if bundle.format != "appimage"
            || bundle.client_exe != "devmanager"
            || bundle.host_exe != "devmanager-host"
            || self.version != bundle.version
            || self.platform != bundle.packager_target
            || self.client_build != bundle.client_build
            || self.host_build != bundle.host_build
            || self.protocol_major != bundle.protocol_major
            || self.protocol_minor != bundle.protocol_minor
        {
            return Err("AppImage client/host identity differs from the verified release.".into());
        }
        Ok(())
    }
}

/// Parse only bounded metadata, then verify the complete standard AppImage
/// payload. No code from the incoming artifact is executed during admission.
pub fn inspect_image(bytes: &[u8]) -> Result<AppImageManifest, String> {
    if bytes.len() < 64 + 24
        || bytes.len() as u64 > MAX_IMAGE_BYTES
        || &bytes[..4] != b"\x7fELF"
        || bytes[4] != 2
        || bytes[5] != 1
        || &bytes[8..11] != b"AI\x02"
        || &bytes[bytes.len() - 16..] != MAGIC
    {
        return Err("Expected a type-2 DevManager AppImage.".into());
    }
    let length_offset = bytes.len() - 24;
    let length = u64::from_le_bytes(bytes[length_offset..length_offset + 8].try_into().unwrap());
    if length == 0 || length > MAX_MANIFEST_BYTES as u64 || length > length_offset as u64 {
        return Err("Invalid AppImage manifest length.".into());
    }
    let payload_end = length_offset - length as usize;
    if payload_end < 64 {
        return Err("AppImage payload is missing.".into());
    }
    let manifest: AppImageManifest = serde_json::from_slice(&bytes[payload_end..length_offset])
        .map_err(|_| "Invalid AppImage identity manifest.".to_string())?;
    manifest.validate()?;
    let machine = u16::from_le_bytes([bytes[18], bytes[19]]);
    if !matches!(
        (manifest.platform.as_str(), machine),
        ("linux-x86_64", 62) | ("linux-aarch64", 183)
    ) || sha256(&bytes[..payload_end]) != manifest.payload_sha256
    {
        return Err("AppImage payload hash or architecture does not match its identity.".into());
    }
    Ok(manifest)
}

fn read_image(file: &mut File) -> Result<Vec<u8>, String> {
    let length = file.metadata().map_err(io_error)?.len();
    if length > MAX_IMAGE_BYTES {
        return Err("AppImage exceeds the update size limit.".into());
    }
    file.seek(SeekFrom::Start(0)).map_err(io_error)?;
    let mut bytes = Vec::new();
    file.take(MAX_IMAGE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(io_error)?;
    if bytes.len() as u64 != length {
        return Err("AppImage changed while being read.".into());
    }
    Ok(bytes)
}

fn io_error(error: std::io::Error) -> String {
    format!("AppImage update I/O failed: {error}")
}

fn name(value: &std::ffi::OsStr) -> Result<std::ffi::CString, String> {
    use std::os::unix::ffi::OsStrExt;
    std::ffi::CString::new(value.as_bytes()).map_err(|_| "Invalid update filename.".into())
}

/// Retained same-user directories keep rename and cleanup relative to the
/// admitted installation, including when a pathname is changed concurrently.
struct Directory {
    file: File,
    path: PathBuf,
}

impl Directory {
    fn open(path: &Path, private: bool) -> Result<Self, String> {
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(path)
            .map_err(io_error)?;
        let metadata = file.metadata().map_err(io_error)?;
        if metadata.uid() != unsafe { libc::geteuid() } || (private && metadata.mode() & 0o077 != 0)
        {
            return Err("The AppImage update directory must be owned by the current user.".into());
        }
        Ok(Self {
            file,
            path: path.to_path_buf(),
        })
    }

    fn require_path(&self) -> Result<(), String> {
        let expected = self.file.metadata().map_err(io_error)?;
        let observed = fs::symlink_metadata(&self.path).map_err(io_error)?;
        if !observed.is_dir()
            || expected.dev() != observed.dev()
            || expected.ino() != observed.ino()
        {
            return Err("The AppImage update directory changed.".into());
        }
        Ok(())
    }

    fn open_file(&self, filename: &std::ffi::OsStr, flags: i32, mode: u32) -> Result<File, String> {
        let filename = name(filename)?;
        let fd = unsafe {
            libc::openat(
                self.file.as_raw_fd(),
                filename.as_ptr(),
                flags | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                mode,
            )
        };
        if fd < 0 {
            return Err(io_error(std::io::Error::last_os_error()));
        }
        // SAFETY: openat transferred this newly opened descriptor to this owner.
        let file = unsafe { File::from_raw_fd(fd) };
        let metadata = file.metadata().map_err(io_error)?;
        if !metadata.is_file()
            || metadata.uid() != unsafe { libc::geteuid() }
            || metadata.nlink() != 1
        {
            return Err("The AppImage update file must be a private, regular file.".into());
        }
        Ok(file)
    }

    fn unlink(&self, filename: &str) -> Result<(), String> {
        let filename = name(filename.as_ref())?;
        let result = unsafe { libc::unlinkat(self.file.as_raw_fd(), filename.as_ptr(), 0) };
        if result != 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() != std::io::ErrorKind::NotFound {
                return Err(io_error(error));
            }
        }
        self.file.sync_all().map_err(io_error)
    }
}

struct ExclusiveFile<'a> {
    directory: &'a Directory,
    filename: &'static str,
    file: File,
    retained: bool,
}

impl<'a> ExclusiveFile<'a> {
    fn create(directory: &'a Directory, filename: &'static str, mode: u32) -> Result<Self, String> {
        let file = directory.open_file(
            filename.as_ref(),
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL,
            mode,
        )?;
        Ok(Self {
            directory,
            filename,
            file,
            retained: false,
        })
    }
}

impl Drop for ExclusiveFile<'_> {
    fn drop(&mut self) {
        if !self.retained {
            let same_file = self
                .file
                .metadata()
                .ok()
                .zip(
                    self.directory
                        .open_file(self.filename.as_ref(), libc::O_RDONLY, 0)
                        .ok()
                        .and_then(|file| file.metadata().ok()),
                )
                .is_some_and(|(owned, current)| {
                    owned.dev() == current.dev() && owned.ino() == current.ino()
                });
            if same_file {
                let _ = self.directory.unlink(self.filename);
            }
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReplacementJournal {
    schema: u32,
    old_sha256: String,
    new_sha256: String,
}

pub fn recovery_directory(image: &Path) -> Result<PathBuf, String> {
    use std::os::unix::ffi::OsStrExt;
    let filename = image.file_name().ok_or("AppImage has no filename.")?;
    let parent = image.parent().ok_or("AppImage has no parent directory.")?;
    Ok(parent.join(format!(
        ".devmanager-update-{}",
        sha256(filename.as_bytes())
    )))
}

/// Owns the exact lock, staged image and old-image hash until atomic cutover.
/// The old image is retained after cutover until a matching new host Hello.
pub struct AppImageReplacement {
    parent: Directory,
    store: Directory,
    _lock: File,
    filename: std::ffi::OsString,
    journal: ReplacementJournal,
}

impl AppImageReplacement {
    pub fn prepare(
        image: &Path,
        bytes: &[u8],
        expected: &AtomicInstallerBundle,
    ) -> Result<Self, String> {
        let manifest = inspect_image(bytes)?;
        manifest.require_bundle(expected)?;
        super::verify_downloaded_artifact_sha256(
            bytes,
            expected
                .artifact_hash
                .as_deref()
                .ok_or("Missing verified artifact hash.")?,
        )
        .map_err(|error| error.to_string())?;
        let replacement = Self::open(image)?;
        if replacement
            .store
            .path
            .join(super::UPDATE_HANDOFF_RECOVERY_MARKER)
            .try_exists()
            .map_err(io_error)?
        {
            return Err("An earlier AppImage handoff must complete before another update.".into());
        }
        let mut current = replacement
            .parent
            .open_file(&replacement.filename, libc::O_RDONLY, 0)?;
        let old_bytes = read_image(&mut current)?;
        inspect_image(&old_bytes)?;
        let journal = ReplacementJournal {
            schema: 1,
            old_sha256: sha256(&old_bytes),
            new_sha256: sha256(bytes),
        };
        if journal.old_sha256 == journal.new_sha256 {
            return Err("This AppImage is already installed.".into());
        }
        if replacement
            .store
            .path
            .join(JOURNAL)
            .try_exists()
            .map_err(io_error)?
        {
            return Err("An earlier AppImage update requires recovery before retrying.".into());
        }
        {
            // Cleanup owns each created pathname before the first fallible write.
            let mut staged = ExclusiveFile::create(&replacement.store, STAGED_IMAGE, 0o700)?;
            staged.file.write_all(bytes).map_err(io_error)?;
            staged.file.sync_all().map_err(io_error)?;
            let mut journal_file = ExclusiveFile::create(&replacement.store, JOURNAL, 0o600)?;
            journal_file
                .file
                .write_all(&serde_json::to_vec(&journal).map_err(|error| error.to_string())?)
                .map_err(io_error)?;
            journal_file.file.sync_all().map_err(io_error)?;
            replacement.store.file.sync_all().map_err(io_error)?;
            replacement.parent.file.sync_all().map_err(io_error)?;
            staged.retained = true;
            journal_file.retained = true;
        }
        Ok(Self {
            journal,
            ..replacement
        })
    }

    fn open(image: &Path) -> Result<Self, String> {
        if !image.is_absolute() {
            return Err("AppImage installation path must be absolute.".into());
        }
        let parent = Directory::open(
            image.parent().ok_or("AppImage has no parent directory.")?,
            false,
        )?;
        let store_path = recovery_directory(image)?;
        let dirname = name(store_path.file_name().unwrap())?;
        if unsafe { libc::mkdirat(parent.file.as_raw_fd(), dirname.as_ptr(), 0o700) } != 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() != std::io::ErrorKind::AlreadyExists {
                return Err(io_error(error));
            }
        }
        let store = Directory::open(&store_path, true)?;
        let lock = store.open_file("lock".as_ref(), libc::O_RDWR | libc::O_CREAT, 0o600)?;
        if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            return Err("Another process owns this AppImage update.".into());
        }
        parent.require_path()?;
        store.require_path()?;
        Ok(Self {
            parent,
            store,
            _lock: lock,
            filename: image.file_name().unwrap().to_owned(),
            journal: ReplacementJournal {
                schema: 1,
                old_sha256: String::new(),
                new_sha256: String::new(),
            },
        })
    }

    pub fn recovery_dir(&self) -> &Path {
        &self.store.path
    }

    fn hashes(&self) -> Result<(String, String), String> {
        let mut current = self.parent.open_file(&self.filename, libc::O_RDONLY, 0)?;
        let mut staged = self
            .store
            .open_file(STAGED_IMAGE.as_ref(), libc::O_RDONLY, 0)?;
        Ok((
            sha256(&read_image(&mut current)?),
            sha256(&read_image(&mut staged)?),
        ))
    }

    fn exchange(&self) -> Result<(), String> {
        self.parent.require_path()?;
        self.store.require_path()?;
        let current = name(&self.filename)?;
        let staged = name(STAGED_IMAGE.as_ref())?;
        let result = unsafe {
            libc::renameat2(
                self.parent.file.as_raw_fd(),
                current.as_ptr(),
                self.store.file.as_raw_fd(),
                staged.as_ptr(),
                libc::RENAME_EXCHANGE,
            )
        };
        if result != 0 {
            return Err(io_error(std::io::Error::last_os_error()));
        }
        self.parent.file.sync_all().map_err(io_error)?;
        self.store.file.sync_all().map_err(io_error)
    }

    pub fn commit(&self) -> Result<(), String> {
        if self.hashes()?
            != (
                self.journal.old_sha256.clone(),
                self.journal.new_sha256.clone(),
            )
        {
            return Err("AppImage files changed before atomic replacement.".into());
        }
        self.exchange()?;
        if self.hashes()?
            != (
                self.journal.new_sha256.clone(),
                self.journal.old_sha256.clone(),
            )
        {
            return Err(
                "AppImage replacement identity is uncertain; automatic retry is disabled.".into(),
            );
        }
        Ok(())
    }

    fn require_committed(&self) -> Result<(), String> {
        if self.hashes()?
            != (
                self.journal.new_sha256.clone(),
                self.journal.old_sha256.clone(),
            )
        {
            return Err(
                "AppImage recovery found foreign image bytes; no files were replaced.".into(),
            );
        }
        Ok(())
    }

    fn rollback_committed(&self) -> Result<(), String> {
        self.require_committed()?;
        self.exchange()?;
        // If interrupted here, old-image startup sees the prepared physical
        // pair and performs the same marker-first abort.
        self.abort_prepared()
    }

    pub(super) fn arm_restart(&self) -> Result<(), String> {
        self.require_committed()?;
        PENDING_RESTART
            .set(self.parent.path.join(&self.filename))
            .map_err(|_| "An AppImage restart is already pending.".to_string())
    }

    fn read_journal(&mut self) -> Result<bool, String> {
        if !self
            .store
            .path
            .join(JOURNAL)
            .try_exists()
            .map_err(io_error)?
        {
            return Ok(false);
        }
        let file = self.store.open_file(JOURNAL.as_ref(), libc::O_RDONLY, 0)?;
        if file.metadata().map_err(io_error)?.len() > 4096 {
            return Err("Oversized AppImage replacement journal.".into());
        }
        let journal: ReplacementJournal = serde_json::from_reader(file.take(4097))
            .map_err(|_| "Corrupt AppImage replacement journal; recovery stopped.".to_string())?;
        if journal.schema != 1
            || !valid_digest(&journal.old_sha256)
            || !valid_digest(&journal.new_sha256)
            || journal.old_sha256 == journal.new_sha256
        {
            return Err("Invalid AppImage replacement lineage.".into());
        }
        self.journal = journal;
        Ok(true)
    }

    fn clear_files(&self) -> Result<(), String> {
        // Remove the durable lineage first. An interruption can leave only an
        // unreferenced generated backup; it can never imply an unfinished swap.
        self.store.unlink(JOURNAL)?;
        self.store.unlink(STAGED_IMAGE)
    }

    /// Abort only while the original image and exact incoming bytes remain.
    /// Once exchanged, preserve the backup and marker for startup/Hello recovery.
    pub fn abort_prepared(&self) -> Result<(), String> {
        if self.hashes()?
            != (
                self.journal.old_sha256.clone(),
                self.journal.new_sha256.clone(),
            )
        {
            return Err("AppImage cutover already occurred; restart to complete recovery.".into());
        }
        // A crash after removing the journal must not strand the old image
        // beside a marker that claims the new host is installed.
        self.store.unlink(super::UPDATE_HANDOFF_RECOVERY_MARKER)?;
        self.clear_files()
    }
}

/// Validate the standard AppImage environment against both mounted binaries.
/// An arbitrary APPIMAGE variable never authorizes replacing another file.
pub fn running_image() -> Result<Option<PathBuf>, String> {
    let Some(image) = std::env::var_os("APPIMAGE") else {
        return Ok(None);
    };
    let image = PathBuf::from(image);
    let appdir = std::env::var_os("APPDIR")
        .map(PathBuf::from)
        .ok_or("AppImage mount directory is missing.")?;
    let executable = std::env::current_exe().map_err(io_error)?;
    running_image_at(&image, &appdir, &executable, env!("CARGO_PKG_VERSION"))
}

fn running_image_at(
    image: &Path,
    appdir: &Path,
    executable: &Path,
    version: &str,
) -> Result<Option<PathBuf>, String> {
    // IDEs and terminals may themselves be AppImages. Their environment is
    // inherited by ordinary checkout binaries; it is not our installation.
    let mount = appdir
        .canonicalize()
        .unwrap_or_else(|_| appdir.to_path_buf());
    if !executable.starts_with(&mount) {
        return Ok(None);
    }
    require_mounted_identity(image, appdir, executable, version)?;
    Ok(Some(image.to_path_buf()))
}

fn require_mounted_identity(
    image: &Path,
    appdir: &Path,
    executable: &Path,
    version: &str,
) -> Result<AppImageManifest, String> {
    if !image.is_absolute() || !appdir.is_absolute() {
        return Err("AppImage paths must be absolute.".into());
    }
    let mut image_file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(image)
        .map_err(io_error)?;
    let metadata = image_file.metadata().map_err(io_error)?;
    if !metadata.is_file() || metadata.uid() != unsafe { libc::geteuid() } || metadata.nlink() != 1
    {
        return Err("This AppImage is not a user-owned update installation.".into());
    }
    let manifest = inspect_image(&read_image(&mut image_file)?)?;
    if manifest.version != version
        || manifest.platform != format!("linux-{}", std::env::consts::ARCH)
    {
        return Err("Installed AppImage identity differs from the running build.".into());
    }
    let client = appdir
        .join("usr/bin/devmanager")
        .canonicalize()
        .map_err(io_error)?;
    let host = appdir
        .join("usr/bin/devmanager-host")
        .canonicalize()
        .map_err(io_error)?;
    let executable = executable.canonicalize().map_err(io_error)?;
    let root = appdir.canonicalize().map_err(io_error)?;
    if (executable != client && executable != host)
        || !client.starts_with(&root)
        || !host.starts_with(&root)
    {
        return Err("Running executable does not belong to this AppImage mount.".into());
    }
    for (path, expected) in [
        (client, &manifest.client_sha256),
        (host, &manifest.host_sha256),
    ] {
        let mut file = File::open(path).map_err(io_error)?;
        if sha256(&read_image(&mut file)?) != *expected {
            return Err("Mounted client/host binaries differ from the AppImage identity.".into());
        }
    }
    Ok(manifest)
}

/// Resolve recovery outside the immutable mount. A non-AppImage build keeps
/// its existing binary-directory recovery contract.
pub fn installed_recovery_directory() -> Result<PathBuf, String> {
    match running_image()? {
        Some(image) => recovery_directory(&image),
        None => std::env::current_exe()
            .map_err(io_error)?
            .parent()
            .map(Path::to_path_buf)
            .ok_or_else(|| "Executable has no parent directory.".into()),
    }
}

/// Before host bootstrap, distinguish a staged old installation from a
/// committed new one using both physical image hashes, never a phase guess.
pub fn recover_running_image() -> Result<(), String> {
    let Some(image) = running_image()? else {
        return Ok(());
    };
    let store_path = recovery_directory(&image)?;
    if !store_path.try_exists().map_err(io_error)? {
        return Ok(());
    }
    let mut replacement = AppImageReplacement::open(&image)?;
    if !replacement.read_journal()? {
        // Only this private generated filename is eligible for orphan cleanup.
        if replacement
            .store
            .path
            .join(STAGED_IMAGE)
            .try_exists()
            .map_err(io_error)?
        {
            let _owned = replacement
                .store
                .open_file(STAGED_IMAGE.as_ref(), libc::O_RDONLY, 0)?;
            replacement.store.unlink(STAGED_IMAGE)?;
        }
        return Ok(());
    }
    let hashes = replacement.hashes()?;
    if hashes
        == (
            replacement.journal.old_sha256.clone(),
            replacement.journal.new_sha256.clone(),
        )
    {
        return replacement.abort_prepared();
    }
    if hashes
        != (
            replacement.journal.new_sha256.clone(),
            replacement.journal.old_sha256.clone(),
        )
    {
        return Err("AppImage recovery found foreign image bytes; no files were replaced.".into());
    }
    let marker = super::read_update_handoff_recovery_marker(replacement.recovery_dir())?
        .ok_or("Committed AppImage is missing its host handoff marker.")?;
    marker.validate_live_host_hello(
        env!("DEVMANAGER_HOST_BUILD_IDENTITY"),
        crate::protocol::PROTOCOL_MAJOR,
        crate::protocol::PROTOCOL_MINOR,
    )?;
    Ok(())
}

/// Called only from the matching live host Hello completion path.
pub fn finalize_running_image(recovery_dir: &Path) -> Result<(), String> {
    let Some(image) = running_image()? else {
        return Ok(());
    };
    if recovery_directory(&image)? != recovery_dir {
        return Err("AppImage recovery directory does not match this installation.".into());
    }
    let mut replacement = AppImageReplacement::open(&image)?;
    if !replacement.read_journal()? {
        return Ok(());
    }
    if replacement.hashes()?
        != (
            replacement.journal.new_sha256.clone(),
            replacement.journal.old_sha256.clone(),
        )
    {
        return Err("Only the committed AppImage may finalize recovery.".into());
    }
    replacement.clear_files()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{symlink, PermissionsExt};

    fn fixture(version: &str) -> Vec<u8> {
        let mut payload = vec![0u8; 512];
        payload[..6].copy_from_slice(b"\x7fELF\x02\x01");
        payload[8..11].copy_from_slice(b"AI\x02");
        payload[18..20].copy_from_slice(&62u16.to_le_bytes());
        payload[32..32 + version.len()].copy_from_slice(version.as_bytes());
        let manifest = AppImageManifest {
            schema: 1,
            version: version.into(),
            platform: "linux-x86_64".into(),
            client_build: format!("devmanager/{version}"),
            host_build: format!("devmanager-host/{version}"),
            protocol_major: crate::protocol::PROTOCOL_MAJOR,
            protocol_minor: crate::protocol::PROTOCOL_MINOR,
            payload_sha256: sha256(&payload),
            client_sha256: sha256(b"client executable"),
            host_sha256: sha256(b"host executable"),
        };
        let encoded = serde_json::to_vec(&manifest).unwrap();
        payload.extend_from_slice(&encoded);
        payload.extend_from_slice(&(encoded.len() as u64).to_le_bytes());
        payload.extend_from_slice(MAGIC);
        payload
    }

    fn bundle(bytes: &[u8]) -> AtomicInstallerBundle {
        let manifest = inspect_image(bytes).unwrap();
        AtomicInstallerBundle::from_verified_download(
            super::super::handoff::VerifiedPackagerDownload::new(
                &manifest.version,
                format!("sha256:{}", sha256(bytes)),
                "linux-x86_64",
                "https://example.test/DevManager.AppImage",
                "verified-test-signature",
                "appimage",
            ),
            manifest.protocol_major,
            manifest.protocol_minor,
            &manifest.client_build,
            &manifest.host_build,
        )
        .unwrap()
    }

    fn installed(directory: &Path) -> PathBuf {
        let image = directory.join("DevManager with spaces.AppImage");
        fs::write(&image, fixture("0.2.0")).unwrap();
        fs::set_permissions(&image, fs::Permissions::from_mode(0o700)).unwrap();
        image
    }

    #[test]
    fn appimage_manifest_rejects_corrupt_payload_footer_architecture_and_release() {
        let good = fixture("0.2.1");
        let manifest = inspect_image(&good).unwrap();
        manifest.require_bundle(&bundle(&good)).unwrap();
        for offset in [0, 4, 8, 18, 100, good.len() - 1, good.len() - 17] {
            let mut changed = good.clone();
            changed[offset] ^= 0x7f;
            assert!(
                inspect_image(&changed).is_err(),
                "accepted corruption at {offset}"
            );
        }
        assert!(manifest.require_bundle(&bundle(&fixture("0.2.2"))).is_err());
        let mut unverified = bundle(&good);
        unverified.signature_verified_by_packager = false;
        assert!(manifest.require_bundle(&unverified).is_err());
    }

    #[test]
    fn appimage_atomic_exchange_retains_whole_old_image_until_completion() {
        let directory = tempfile::tempdir().unwrap();
        let image = installed(directory.path());
        let old = fs::read(&image).unwrap();
        let new = fixture("0.2.1");
        let replacement = AppImageReplacement::prepare(&image, &new, &bundle(&new)).unwrap();
        assert_eq!(fs::read(&image).unwrap(), old);
        assert!(
            AppImageReplacement::prepare(&image, &new, &bundle(&new)).is_err(),
            "second updater acquired the same installation"
        );
        replacement.commit().unwrap();
        assert_eq!(fs::read(&image).unwrap(), new);
        assert_eq!(
            fs::read(replacement.store.path.join(STAGED_IMAGE)).unwrap(),
            old
        );
        assert!(
            replacement.abort_prepared().is_err(),
            "post-cutover failure must retain the old image for recovery"
        );
        drop(replacement);
        let mut resumed = AppImageReplacement::open(&image).unwrap();
        assert!(resumed.read_journal().unwrap());
        assert_eq!(
            resumed.hashes().unwrap(),
            (
                resumed.journal.new_sha256.clone(),
                resumed.journal.old_sha256.clone()
            )
        );
        resumed.clear_files().unwrap();
        assert_eq!(fs::read(&image).unwrap(), new);
        assert!(!resumed.store.path.join(STAGED_IMAGE).exists());
    }

    #[test]
    fn appimage_failed_exec_restores_exact_previous_image_and_retires_marker() {
        let directory = tempfile::tempdir().unwrap();
        let image = installed(directory.path());
        let old = fs::read(&image).unwrap();
        let new = fixture("0.2.1");
        let replacement = AppImageReplacement::prepare(&image, &new, &bundle(&new)).unwrap();
        replacement.commit().unwrap();
        drop(replacement);
        let error = restart_committed_image(&image, |path| {
            assert_eq!(fs::read(path).unwrap(), new);
            std::io::Error::from_raw_os_error(libc::ENOEXEC)
        })
        .unwrap_err();
        assert!(error.contains("previous AppImage was restored"));
        assert_eq!(fs::read(&image).unwrap(), old);
        assert!(!recovery_directory(&image).unwrap().join(JOURNAL).exists());
        assert!(!recovery_directory(&image)
            .unwrap()
            .join(STAGED_IMAGE)
            .exists());
    }

    #[test]
    fn appimage_restart_refuses_corrupt_lineage_before_launch() {
        let directory = tempfile::tempdir().unwrap();
        let image = installed(directory.path());
        let new = fixture("0.2.1");
        let replacement = AppImageReplacement::prepare(&image, &new, &bundle(&new)).unwrap();
        replacement.commit().unwrap();
        let store = replacement.recovery_dir().to_path_buf();
        drop(replacement);
        fs::write(store.join(JOURNAL), b"corrupt journal").unwrap();
        assert!(restart_committed_image(&image, |_| panic!("must not launch")).is_err());
        assert_eq!(fs::read(&image).unwrap(), new);
        assert!(store.join(STAGED_IMAGE).exists());
    }

    #[test]
    fn appimage_precommit_interruption_restores_retry_without_touching_installed_bytes() {
        let directory = tempfile::tempdir().unwrap();
        let image = installed(directory.path());
        let old = fs::read(&image).unwrap();
        let new = fixture("0.2.1");
        drop(AppImageReplacement::prepare(&image, &new, &bundle(&new)).unwrap());
        let mut resumed = AppImageReplacement::open(&image).unwrap();
        assert!(resumed.read_journal().unwrap());
        // A prepared marker must be removed before the old image loses its journal.
        let mut marker = resumed
            .store
            .open_file(
                super::super::UPDATE_HANDOFF_RECOVERY_MARKER.as_ref(),
                libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL,
                0o600,
            )
            .unwrap();
        marker.write_all(b"sealed incoming handoff").unwrap();
        resumed.abort_prepared().unwrap();
        assert_eq!(fs::read(&image).unwrap(), old);
        assert!(!resumed
            .store
            .path
            .join(super::super::UPDATE_HANDOFF_RECOVERY_MARKER)
            .exists());
        drop(resumed);
        AppImageReplacement::prepare(&image, &new, &bundle(&new))
            .unwrap()
            .abort_prepared()
            .unwrap();
    }

    #[test]
    fn appimage_changed_target_and_symlink_staging_fail_without_overwriting() {
        let directory = tempfile::tempdir().unwrap();
        let image = installed(directory.path());
        let new = fixture("0.2.1");
        let replacement = AppImageReplacement::prepare(&image, &new, &bundle(&new)).unwrap();
        fs::write(&image, b"foreign replacement").unwrap();
        assert!(replacement.commit().is_err());
        assert_eq!(fs::read(&image).unwrap(), b"foreign replacement");
        drop(replacement);

        let other = tempfile::tempdir().unwrap();
        let image = installed(other.path());
        let owned = AppImageReplacement::open(&image).unwrap();
        let sentinel = other.path().join("user-file");
        fs::write(&sentinel, b"keep this").unwrap();
        symlink(&sentinel, owned.store.path.join(STAGED_IMAGE)).unwrap();
        drop(owned);
        assert!(AppImageReplacement::prepare(&image, &new, &bundle(&new)).is_err());
        assert_eq!(fs::read(&sentinel).unwrap(), b"keep this");
    }

    #[test]
    fn appimage_environment_inherited_from_an_ide_is_not_an_installation() {
        let directory = tempfile::tempdir().unwrap();
        let appdir = directory.path().join("IDE.AppDir");
        fs::create_dir(&appdir).unwrap();
        let checkout_exe = directory
            .path()
            .join("checkout/target/debug/devmanager-host");
        assert_eq!(
            running_image_at(
                &directory.path().join("IDE.AppImage"),
                &appdir,
                &checkout_exe,
                "0.4.2"
            )
            .unwrap(),
            None
        );
        assert!(
            running_image_at(
                &directory.path().join("corrupt.AppImage"),
                &appdir,
                &appdir.join("usr/bin/devmanager"),
                "0.4.2"
            )
            .is_err(),
            "our own damaged mount must still fail closed"
        );
    }

    #[test]
    fn appimage_running_path_requires_both_exact_mounted_binaries() {
        let directory = tempfile::tempdir().unwrap();
        let image = installed(directory.path());
        let appdir = directory.path().join("mounted AppDir");
        fs::create_dir_all(appdir.join("usr/bin")).unwrap();
        let client = appdir.join("usr/bin/devmanager");
        let host = appdir.join("usr/bin/devmanager-host");
        fs::write(&client, b"client executable").unwrap();
        fs::write(&host, b"host executable").unwrap();
        require_mounted_identity(&image, &appdir, &client, "0.2.0").unwrap();
        require_mounted_identity(&image, &appdir, &host, "0.2.0").unwrap();
        assert!(require_mounted_identity(&image, &appdir, &client, "0.2.1").is_err());
        fs::write(&host, b"different host").unwrap();
        assert!(require_mounted_identity(&image, &appdir, &client, "0.2.0").is_err());
    }
}
