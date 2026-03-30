use cargo_packager_resource_resolver::{resources_dir, PackageFormat};
use gpui::{AssetSource, Result, SharedString};
use std::borrow::Cow;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

static ASSETS_DIR: OnceLock<PathBuf> = OnceLock::new();
static GHOSTTY_RESOURCES_DIR: OnceLock<PathBuf> = OnceLock::new();

pub struct AppAssets {
    base: PathBuf,
}

impl AppAssets {
    pub fn new() -> Self {
        Self { base: assets_dir() }
    }
}

pub fn assets_dir() -> PathBuf {
    ASSETS_DIR.get_or_init(resolve_assets_dir).clone()
}

pub fn asset_path(path: impl AsRef<Path>) -> PathBuf {
    assets_dir().join(path)
}

pub fn ghostty_resources_dir() -> PathBuf {
    GHOSTTY_RESOURCES_DIR
        .get_or_init(resolve_ghostty_resources_dir)
        .clone()
}

fn resolve_assets_dir() -> PathBuf {
    asset_dir_candidates()
        .into_iter()
        .find(|candidate| candidate.join("icons").is_dir())
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("assets"))
}

fn resolve_ghostty_resources_dir() -> PathBuf {
    ghostty_resource_candidates()
        .into_iter()
        .find(|candidate| {
            candidate
                .join("shell-integration")
                .join("README.md")
                .is_file()
        })
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("third_party/ghostty"))
}

fn asset_dir_candidates() -> Vec<PathBuf> {
    let mut candidates = Vec::new();

    for format in PackageFormat::platform_all() {
        if let Ok(resource_root) = resources_dir(*format) {
            candidates.push(resource_root.join("assets"));
            candidates.push(resource_root);
        }
    }

    if let Ok(exe_path) = std::env::current_exe() {
        if let Some(exe_dir) = exe_path.parent() {
            candidates.push(exe_dir.join("assets"));
            candidates.push(exe_dir.join("resources").join("assets"));

            if let Some(parent) = exe_dir.parent() {
                candidates.push(parent.join("Resources").join("assets"));
                candidates.push(parent.join("resources").join("assets"));

                if let Some(grandparent) = parent.parent() {
                    candidates.push(grandparent.join("Resources").join("assets"));
                    candidates.push(grandparent.join("resources").join("assets"));
                }
            }
        }
    }

    candidates
}

fn ghostty_resource_candidates() -> Vec<PathBuf> {
    let mut candidates = Vec::new();

    for format in PackageFormat::platform_all() {
        if let Ok(resource_root) = resources_dir(*format) {
            candidates.push(resource_root.join("third_party").join("ghostty"));
            candidates.push(resource_root.join("ghostty"));
        }
    }

    if let Ok(exe_path) = std::env::current_exe() {
        if let Some(exe_dir) = exe_path.parent() {
            candidates.push(exe_dir.join("third_party").join("ghostty"));
            candidates.push(
                exe_dir
                    .join("resources")
                    .join("third_party")
                    .join("ghostty"),
            );

            if let Some(parent) = exe_dir.parent() {
                candidates.push(parent.join("Resources").join("third_party").join("ghostty"));
                candidates.push(parent.join("resources").join("third_party").join("ghostty"));
            }
        }
    }

    candidates.push(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("third_party/ghostty"));
    candidates
}

impl AssetSource for AppAssets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        fs::read(self.base.join(path))
            .map(|data| Some(Cow::Owned(data)))
            .map_err(Into::into)
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        fs::read_dir(self.base.join(path))
            .map(|entries| {
                entries
                    .filter_map(|entry| {
                        entry
                            .ok()
                            .and_then(|entry| entry.file_name().into_string().ok())
                            .map(SharedString::from)
                    })
                    .collect()
            })
            .map_err(Into::into)
    }
}

#[cfg(test)]
mod tests {
    use super::{assets_dir, ghostty_resources_dir};

    #[test]
    fn resolves_existing_assets_directory() {
        let base = assets_dir();
        assert!(base.join("icons").is_dir());
        assert!(base.join("icons/settings.svg").is_file());
    }

    #[test]
    fn resolves_existing_ghostty_resources_directory() {
        let base = ghostty_resources_dir();
        assert!(base.join("shell-integration/README.md").is_file());
    }
}
