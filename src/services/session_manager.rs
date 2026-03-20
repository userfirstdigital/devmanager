use crate::models::{AppConfig, SessionState};
use crate::persistence::{self, PersistenceError, WorkspaceSnapshot};
use rfd::FileDialog;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigImportMode {
    Merge,
    Replace,
}

#[derive(Debug, Clone, Default)]
pub struct SessionManager;

impl SessionManager {
    pub fn new() -> Self {
        Self
    }

    pub fn load_workspace(&self) -> Result<WorkspaceSnapshot, PersistenceError> {
        persistence::load_workspace()
    }

    pub fn save_config(&self, config: &AppConfig) -> Result<(), PersistenceError> {
        persistence::save_config(config)
    }

    pub fn save_session(&self, session: &SessionState) -> Result<(), PersistenceError> {
        persistence::save_session(session)
    }

    pub fn export_config_to_path(
        &self,
        path: &Path,
        config: &AppConfig,
    ) -> Result<(), PersistenceError> {
        persistence::save_config_to_path(path, config)
    }

    pub fn import_config_from_path(&self, path: &Path) -> Result<AppConfig, PersistenceError> {
        persistence::load_config_from_path(path)
    }

    pub fn export_config_dialog(&self, config: &AppConfig) -> Result<Option<PathBuf>, String> {
        let default_name = default_export_file_name();
        let Some(path) = FileDialog::new()
            .add_filter("JSON", &["json"])
            .set_file_name(&default_name)
            .save_file()
        else {
            return Ok(None);
        };

        self.export_config_to_path(&path, config)
            .map_err(|error| error.to_string())?;
        Ok(Some(path))
    }

    pub fn import_config_dialog(
        &self,
        current: &AppConfig,
        mode: ConfigImportMode,
    ) -> Result<Option<(AppConfig, PathBuf)>, String> {
        let Some(path) = FileDialog::new().add_filter("JSON", &["json"]).pick_file() else {
            return Ok(None);
        };

        let imported = self
            .import_config_from_path(&path)
            .map_err(|error| error.to_string())?;
        let next = Self::apply_import_mode(current, imported, mode);
        Ok(Some((next, path)))
    }

    pub fn apply_import_mode(
        current: &AppConfig,
        imported: AppConfig,
        mode: ConfigImportMode,
    ) -> AppConfig {
        match mode {
            ConfigImportMode::Merge => Self::merge_imported_config(current, imported),
            ConfigImportMode::Replace => imported,
        }
    }

    pub fn merge_imported_config(current: &AppConfig, imported: AppConfig) -> AppConfig {
        let mut merged = current.clone();
        let mut existing_names: HashSet<String> = merged
            .projects
            .iter()
            .map(|project| project.name.clone())
            .collect();

        for project in imported.projects {
            if existing_names.insert(project.name.clone()) {
                merged.projects.push(project);
            }
        }

        merged.version = merged.version.max(imported.version);
        merged
    }
}

fn default_export_file_name() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    format!("devmanager-config-{secs}.json")
}
