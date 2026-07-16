use super::{BrowserDownloadEntry, BrowserError};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

const DOWNLOAD_ID_PREFIX: &str = "download-";

#[derive(Debug, Clone)]
pub struct BrowserDownloadStore {
    root: PathBuf,
}

impl BrowserDownloadStore {
    pub fn open(root: impl AsRef<Path>) -> Result<Self, BrowserError> {
        let root = root.as_ref();
        if root.exists() {
            let metadata = std::fs::symlink_metadata(root)
                .map_err(|error| io_error("inspect download root", root, error))?;
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
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
        Ok(Self { root })
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
        let root_metadata = std::fs::symlink_metadata(&self.root)
            .map_err(|error| io_error("inspect download root", &self.root, error))?;
        if root_metadata.file_type().is_symlink() || !root_metadata.is_dir() {
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
            if metadata.file_type().is_symlink() || !metadata.is_file() {
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

struct VerifiedDownload {
    id: String,
    path: PathBuf,
    byte_size: u64,
}

fn is_direct_regular_file(root: &Path, path: &Path) -> bool {
    path.parent() == Some(root)
        && std::fs::symlink_metadata(path)
            .is_ok_and(|metadata| metadata.is_file() && !metadata.file_type().is_symlink())
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
