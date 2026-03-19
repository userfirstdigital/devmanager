use gpui::{AssetSource, Result, SharedString};
use std::borrow::Cow;
use std::fs;
use std::path::PathBuf;

pub struct AppAssets {
    base: PathBuf,
}

impl AppAssets {
    pub fn new() -> Self {
        Self {
            base: PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("assets"),
        }
    }
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
