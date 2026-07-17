use super::BrowserError;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrowserStorageLayout {
    pub profile_dir: PathBuf,
    pub downloads_dir: PathBuf,
    pub resources_dir: PathBuf,
}

impl BrowserStorageLayout {
    pub fn new(app_config_dir: impl AsRef<Path>, project_id: impl AsRef<str>) -> Self {
        let project_hash = format!("{:x}", Sha256::digest(project_id.as_ref().as_bytes()));
        let browser_root = app_config_dir.as_ref().join("browser");
        Self {
            profile_dir: browser_root.join("profiles").join(&project_hash),
            downloads_dir: browser_root.join("downloads").join(&project_hash),
            resources_dir: browser_root.join("resources").join(project_hash),
        }
    }

    pub fn profile_dir(&self) -> &Path {
        &self.profile_dir
    }

    pub fn downloads_dir(&self) -> &Path {
        &self.downloads_dir
    }

    pub fn resources_dir(&self) -> &Path {
        &self.resources_dir
    }

    pub fn ensure(&self) -> Result<(), BrowserError> {
        for path in [&self.profile_dir, &self.downloads_dir, &self.resources_dir] {
            ensure_storage_directory(path)?;
        }
        Ok(())
    }
}

fn ensure_storage_directory(path: &Path) -> Result<(), BrowserError> {
    std::fs::create_dir_all(path).map_err(|error| BrowserError::Io {
        operation: "create storage directory".to_string(),
        path: path.to_path_buf(),
        message: error.to_string(),
    })
}
