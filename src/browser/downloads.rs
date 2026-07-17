use super::{BrowserDownloadEntry, BrowserError, BrowserStorageLayout};
use sha2::{Digest, Sha256};
#[cfg(target_os = "windows")]
use std::os::windows::fs::MetadataExt;
use std::path::{Path, PathBuf};

const DOWNLOAD_ID_PREFIX: &str = "download-";

#[derive(Debug, Clone)]
pub struct BrowserDownloadStore {
    root: PathBuf,
    trusted_root: Option<PathBuf>,
}

impl BrowserDownloadStore {
    pub fn open(root: impl AsRef<Path>) -> Result<Self, BrowserError> {
        let root = root.as_ref();
        if root.exists() {
            let metadata = std::fs::symlink_metadata(root)
                .map_err(|error| io_error("inspect download root", root, error))?;
            if metadata_is_redirect(&metadata) || !metadata.is_dir() {
                return Err(BrowserError::OutsideWorkspace {
                    path: root.to_path_buf(),
                });
            }
        } else {
            std::fs::create_dir_all(root)
                .map_err(|error| io_error("create download root", root, error))?;
        }
        let root = root
            .canonicalize()
            .map_err(|error| io_error("canonicalize download root", root, error))?;
        Ok(Self {
            root,
            trusted_root: None,
        })
    }

    pub fn open_verified(
        app_config_dir: impl AsRef<Path>,
        project_id: impl AsRef<str>,
    ) -> Result<Self, BrowserError> {
        let app_config_dir = verified_app_config_root(app_config_dir.as_ref())?;
        let root = prepare_verified_download_root(&app_config_dir, project_id)?;
        Ok(Self {
            root,
            trusted_root: Some(app_config_dir),
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn list(&self) -> Result<Vec<BrowserDownloadEntry>, BrowserError> {
        Ok(self
            .verified_files()?
            .into_iter()
            .map(|file| BrowserDownloadEntry {
                id: file.id,
                file_name: file
                    .path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or("download")
                    .to_string(),
                byte_size: file.byte_size,
                completed: true,
            })
            .collect())
    }

    pub fn resolve(&self, id: &str) -> Result<PathBuf, BrowserError> {
        validate_download_id(id)?;
        self.verified_files()?
            .into_iter()
            .find(|file| file.id == id)
            .map(|file| file.path)
            .ok_or_else(|| BrowserError::MissingFile {
                path: self.root.join("download"),
            })
    }

    pub fn delete(&self, id: &str) -> Result<(), BrowserError> {
        let path = self.resolve(id)?;
        if !is_direct_regular_file(&self.root, &path) {
            return Err(BrowserError::OutsideWorkspace { path });
        }
        std::fs::remove_file(&path)
            .map_err(|error| io_error("delete browser download", &path, error))
    }

    fn verified_files(&self) -> Result<Vec<VerifiedDownload>, BrowserError> {
        if let Some(trusted_root) = &self.trusted_root {
            verify_prepared_storage_root(trusted_root, &self.root)?;
        }
        let root_metadata = std::fs::symlink_metadata(&self.root)
            .map_err(|error| io_error("inspect download root", &self.root, error))?;
        if metadata_is_redirect(&root_metadata) || !root_metadata.is_dir() {
            return Err(BrowserError::OutsideWorkspace {
                path: self.root.clone(),
            });
        }
        let mut files = Vec::new();
        for entry in std::fs::read_dir(&self.root)
            .map_err(|error| io_error("list browser downloads", &self.root, error))?
        {
            let entry = entry
                .map_err(|error| io_error("read browser download entry", &self.root, error))?;
            let path = entry.path();
            let metadata = match std::fs::symlink_metadata(&path) {
                Ok(metadata) => metadata,
                Err(_) => continue,
            };
            if metadata_is_redirect(&metadata) || !metadata.is_file() {
                continue;
            }
            let canonical_path = match path.canonicalize() {
                Ok(path) => path,
                Err(_) => continue,
            };
            if !is_direct_regular_file(&self.root, &canonical_path) {
                continue;
            }
            files.push(VerifiedDownload {
                id: download_id(&canonical_path),
                path: canonical_path,
                byte_size: metadata.len(),
            });
        }
        files.sort_by(|left, right| left.path.file_name().cmp(&right.path.file_name()));
        Ok(files)
    }
}

pub fn prepare_verified_download_root(
    app_config_dir: impl AsRef<Path>,
    project_id: impl AsRef<str>,
) -> Result<PathBuf, BrowserError> {
    let trusted_root = verified_app_config_root(app_config_dir.as_ref())?;
    let layout = BrowserStorageLayout::new(&trusted_root, project_id);
    prepare_verified_storage_root(&trusted_root, &layout.downloads_dir)
}

pub fn prepare_verified_profile_root(
    app_config_dir: impl AsRef<Path>,
    project_id: impl AsRef<str>,
) -> Result<PathBuf, BrowserError> {
    let trusted_root = verified_app_config_root(app_config_dir.as_ref())?;
    let layout = BrowserStorageLayout::new(&trusted_root, project_id);
    prepare_verified_storage_root(&trusted_root, &layout.profile_dir)
}

pub fn remove_verified_profile(
    trusted_app_config_dir: impl AsRef<Path>,
    profile_dir: impl AsRef<Path>,
) -> Result<(), BrowserError> {
    let trusted_app_config_dir = trusted_app_config_dir.as_ref();
    let profile_dir = profile_dir.as_ref();
    verify_prepared_storage_root(trusted_app_config_dir, trusted_app_config_dir)?;
    let relative = profile_dir
        .strip_prefix(trusted_app_config_dir)
        .map_err(|_| BrowserError::OutsideWorkspace {
            path: profile_dir.to_path_buf(),
        })?;
    let components = relative.components().collect::<Vec<_>>();
    let hash_is_valid = components.len() == 3
        && components[0].as_os_str() == "browser"
        && components[1].as_os_str() == "profiles"
        && components[2].as_os_str().to_str().is_some_and(|value| {
            value.len() == 64
                && value
                    .chars()
                    .all(|character| character.is_ascii_digit() || ('a'..='f').contains(&character))
        });
    if !hash_is_valid {
        return Err(BrowserError::OutsideWorkspace {
            path: profile_dir.to_path_buf(),
        });
    }
    let metadata = match std::fs::symlink_metadata(profile_dir) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(io_error(
                "inspect browser profile directory",
                profile_dir,
                error,
            ))
        }
    };
    let profiles_root = profile_dir
        .parent()
        .ok_or_else(|| BrowserError::OutsideWorkspace {
            path: profile_dir.to_path_buf(),
        })?;
    verify_prepared_storage_root(trusted_app_config_dir, profiles_root)?;
    if !metadata.is_dir() || metadata_is_redirect(&metadata) {
        return Err(BrowserError::OutsideWorkspace {
            path: profile_dir.to_path_buf(),
        });
    }
    verify_prepared_storage_root(trusted_app_config_dir, profile_dir)?;
    std::fs::remove_dir_all(profile_dir)
        .map_err(|error| io_error("clear browser project profile", profile_dir, error))
}

pub(crate) fn prepare_verified_resource_root(
    app_config_dir: &Path,
    project_id: &str,
) -> Result<(PathBuf, PathBuf), BrowserError> {
    let trusted_root = verified_app_config_root(app_config_dir)?;
    let layout = BrowserStorageLayout::new(&trusted_root, project_id);
    let resources_dir = prepare_verified_storage_root(&trusted_root, &layout.resources_dir)?;
    Ok((trusted_root, resources_dir))
}

pub(crate) fn prepare_verified_storage_layout(
    app_config_dir: &Path,
    project_id: &str,
) -> Result<(PathBuf, BrowserStorageLayout), BrowserError> {
    let trusted_root = verified_app_config_root(app_config_dir)?;
    let mut layout = BrowserStorageLayout::new(&trusted_root, project_id);
    layout.profile_dir = prepare_verified_storage_root(&trusted_root, &layout.profile_dir)?;
    layout.downloads_dir = prepare_verified_storage_root(&trusted_root, &layout.downloads_dir)?;
    layout.resources_dir = prepare_verified_storage_root(&trusted_root, &layout.resources_dir)?;
    Ok((trusted_root, layout))
}

pub(crate) fn verified_unique_download_path(
    trusted_root: &Path,
    downloads_dir: &Path,
    suggested_path: &Path,
) -> Result<PathBuf, BrowserError> {
    verify_prepared_storage_root(trusted_root, trusted_root)?;
    verify_prepared_storage_root(trusted_root, downloads_dir)?;
    unique_path_in(downloads_dir, suggested_path)
}

pub(crate) fn prepare_untrusted_download_root(root: &Path) -> Result<PathBuf, BrowserError> {
    let mut ancestors = root
        .ancestors()
        .filter(|ancestor| !ancestor.as_os_str().is_empty())
        .collect::<Vec<_>>();
    ancestors.reverse();
    for ancestor in ancestors {
        create_or_validate_directory(ancestor)?;
    }
    let canonical = root
        .canonicalize()
        .map_err(|error| io_error("canonicalize download root", root, error))?;
    validate_directory(&canonical)?;
    Ok(root.to_path_buf())
}

pub(crate) fn unique_path_in(
    downloads_dir: &Path,
    suggested_path: &Path,
) -> Result<PathBuf, BrowserError> {
    let suggested_name = suggested_path
        .file_name()
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| std::ffi::OsStr::new("download"));
    let direct = downloads_dir.join(suggested_name);
    if !path_is_occupied(&direct)? {
        return Ok(direct);
    }

    let suggested = Path::new(suggested_name);
    let stem = suggested
        .file_stem()
        .filter(|stem| !stem.is_empty())
        .unwrap_or_else(|| std::ffi::OsStr::new("download"))
        .to_string_lossy();
    let extension = suggested.extension().map(|value| value.to_string_lossy());
    for suffix in 1_u64.. {
        let name = match &extension {
            Some(extension) => format!("{stem} ({suffix}).{extension}"),
            None => format!("{stem} ({suffix})"),
        };
        let candidate = downloads_dir.join(name);
        if !path_is_occupied(&candidate)? {
            return Ok(candidate);
        }
    }
    unreachable!("the download suffix space is unbounded")
}

fn path_is_occupied(path: &Path) -> Result<bool, BrowserError> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(io_error("inspect download destination", path, error)),
    }
}

pub(crate) fn verified_app_config_root(app_config_dir: &Path) -> Result<PathBuf, BrowserError> {
    if !app_config_dir.exists() {
        std::fs::create_dir_all(app_config_dir)
            .map_err(|error| io_error("create app config directory", app_config_dir, error))?;
    }
    validate_directory(app_config_dir)?;
    let canonical = app_config_dir
        .canonicalize()
        .map_err(|error| io_error("canonicalize app config directory", app_config_dir, error))?;
    validate_directory(&canonical)?;
    Ok(canonical)
}

fn ensure_trusted_descendant(trusted_root: &Path, descendant: &Path) -> Result<(), BrowserError> {
    let relative =
        descendant
            .strip_prefix(trusted_root)
            .map_err(|_| BrowserError::OutsideWorkspace {
                path: descendant.to_path_buf(),
            })?;
    let mut current = trusted_root.to_path_buf();
    for component in relative.components() {
        if !matches!(component, std::path::Component::Normal(_)) {
            return Err(BrowserError::OutsideWorkspace {
                path: descendant.to_path_buf(),
            });
        }
        current.push(component.as_os_str());
        create_or_validate_directory(&current)?;
    }
    Ok(())
}

fn prepare_verified_storage_root(
    trusted_root: &Path,
    storage_root: &Path,
) -> Result<PathBuf, BrowserError> {
    ensure_trusted_descendant(trusted_root, storage_root)?;
    verify_prepared_storage_root(trusted_root, storage_root)?;
    storage_root
        .canonicalize()
        .map_err(|error| io_error("canonicalize browser storage root", storage_root, error))
}

fn create_or_validate_directory(path: &Path) -> Result<(), BrowserError> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => validate_directory(path),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            match std::fs::create_dir(path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(io_error("create download directory", path, error)),
            }
            validate_directory(path)
        }
        Err(error) => Err(io_error("inspect download directory", path, error)),
    }
}

pub(crate) fn verify_prepared_storage_root(
    trusted_root: &Path,
    downloads_dir: &Path,
) -> Result<(), BrowserError> {
    validate_directory(trusted_root)?;
    let relative =
        downloads_dir
            .strip_prefix(trusted_root)
            .map_err(|_| BrowserError::OutsideWorkspace {
                path: downloads_dir.to_path_buf(),
            })?;
    let mut current = trusted_root.to_path_buf();
    for component in relative.components() {
        if !matches!(component, std::path::Component::Normal(_)) {
            return Err(BrowserError::OutsideWorkspace {
                path: downloads_dir.to_path_buf(),
            });
        }
        current.push(component.as_os_str());
        validate_directory(&current)?;
    }
    let canonical = downloads_dir
        .canonicalize()
        .map_err(|error| io_error("canonicalize download root", downloads_dir, error))?;
    if !canonical.starts_with(trusted_root) || canonical != downloads_dir {
        return Err(BrowserError::OutsideWorkspace {
            path: downloads_dir.to_path_buf(),
        });
    }
    Ok(())
}

fn validate_directory(path: &Path) -> Result<(), BrowserError> {
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|error| io_error("inspect download directory", path, error))?;
    if !metadata.is_dir() || metadata_is_redirect(&metadata) {
        return Err(BrowserError::OutsideWorkspace {
            path: path.to_path_buf(),
        });
    }
    Ok(())
}

fn metadata_is_redirect(metadata: &std::fs::Metadata) -> bool {
    if metadata.file_type().is_symlink() {
        return true;
    }
    #[cfg(target_os = "windows")]
    {
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0400;
        return metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0;
    }
    #[cfg(not(target_os = "windows"))]
    false
}

struct VerifiedDownload {
    id: String,
    path: PathBuf,
    byte_size: u64,
}

fn is_direct_regular_file(root: &Path, path: &Path) -> bool {
    path.parent() == Some(root)
        && std::fs::symlink_metadata(path)
            .is_ok_and(|metadata| metadata.is_file() && !metadata_is_redirect(&metadata))
}

fn download_id(path: &Path) -> String {
    format!(
        "{DOWNLOAD_ID_PREFIX}{:x}",
        Sha256::digest(path.as_os_str().to_string_lossy().as_bytes())
    )
}

fn validate_download_id(id: &str) -> Result<(), BrowserError> {
    let digest = id.strip_prefix(DOWNLOAD_ID_PREFIX).unwrap_or_default();
    if digest.len() == 64
        && digest
            .chars()
            .all(|character| character.is_ascii_hexdigit() && !character.is_ascii_uppercase())
    {
        Ok(())
    } else {
        Err(BrowserError::InvalidInvocation {
            field: "downloadId".to_string(),
        })
    }
}

fn io_error(operation: &str, path: &Path, error: std::io::Error) -> BrowserError {
    BrowserError::Io {
        operation: operation.to_string(),
        path: path.to_path_buf(),
        message: error.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Stdio;
    use std::sync::atomic::{AtomicU64, Ordering};

    struct TestDir(PathBuf);

    impl TestDir {
        fn new(label: &str) -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(1);
            let path = std::env::temp_dir().join(format!(
                "devmanager-browser-downloads-{label}-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[cfg(target_os = "windows")]
    fn create_directory_redirect(target: &Path, link: &Path) {
        let status = std::process::Command::new("cmd.exe")
            .args(["/c", "mklink", "/J"])
            .arg(link)
            .arg(target)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();
        assert!(status.success());
    }

    #[cfg(not(target_os = "windows"))]
    fn create_directory_redirect(target: &Path, link: &Path) {
        std::os::unix::fs::symlink(target, link).unwrap();
    }

    #[cfg(target_os = "windows")]
    fn remove_directory_redirect(link: &Path) {
        std::fs::remove_dir(link).unwrap();
    }

    #[cfg(not(target_os = "windows"))]
    fn remove_directory_redirect(link: &Path) {
        std::fs::remove_file(link).unwrap();
    }

    #[test]
    fn retained_download_root_swap_is_rejected_without_recreating_outside_directories() {
        let temp = TestDir::new("retained-root-swap");
        let ancestor = temp.0.join("live-ancestor");
        let app_config = ancestor.join("trusted-config");
        let downloads_dir = prepare_verified_download_root(&app_config, "project-a").unwrap();
        let retained_root = app_config.canonicalize().unwrap();
        let parked_ancestor = temp.0.join("parked-ancestor");
        std::fs::rename(&ancestor, &parked_ancestor).unwrap();
        let outside_ancestor = temp.0.join("outside-ancestor");
        std::fs::create_dir_all(&outside_ancestor).unwrap();
        create_directory_redirect(&outside_ancestor, &ancestor);

        let result =
            verified_unique_download_path(&retained_root, &downloads_dir, Path::new("report.pdf"));

        assert!(result.is_err());
        assert!(
            !outside_ancestor.join("trusted-config").exists(),
            "validating a retained root must not recreate it through a swapped ancestor"
        );
        remove_directory_redirect(&ancestor);
    }
}
