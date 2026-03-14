use tauri::State;
use crate::state::AppState;
use crate::models::config::{AppConfig, Project, Settings};
use crate::services::config_service;

#[tauri::command]
pub fn get_config(state: State<'_, AppState>) -> Result<AppConfig, String> {
    let mut config = state.config.lock().map_err(|e| e.to_string())?;
    if config.is_none() {
        // Load from disk on first call
        let loaded = config_service::load_config()?;
        *config = Some(loaded);
    }
    Ok(config.clone().unwrap_or_default())
}

#[tauri::command]
pub fn save_full_config(config: AppConfig, state: State<'_, AppState>) -> Result<(), String> {
    let mut current = state.config.lock().map_err(|e| e.to_string())?;
    config_service::save_config(&config)?;
    *current = Some(config);
    Ok(())
}

#[tauri::command]
pub fn add_project(project: Project, state: State<'_, AppState>) -> Result<(), String> {
    let mut config = state.config.lock().map_err(|e| e.to_string())?;
    let mut cfg = config.clone().unwrap_or_default();
    cfg.projects.push(project);
    config_service::save_config(&cfg)?;
    *config = Some(cfg);
    Ok(())
}

#[tauri::command]
pub fn update_project(project: Project, state: State<'_, AppState>) -> Result<(), String> {
    let mut config = state.config.lock().map_err(|e| e.to_string())?;
    let mut cfg = config.clone().unwrap_or_default();
    if let Some(pos) = cfg.projects.iter().position(|p| p.id == project.id) {
        cfg.projects[pos] = project;
    } else {
        return Err(format!("Project with id '{}' not found", project.id));
    }
    config_service::save_config(&cfg)?;
    *config = Some(cfg);
    Ok(())
}

#[tauri::command]
pub fn remove_project(project_id: String, state: State<'_, AppState>) -> Result<(), String> {
    let mut config = state.config.lock().map_err(|e| e.to_string())?;
    let mut cfg = config.clone().unwrap_or_default();
    cfg.projects.retain(|p| p.id != project_id);
    config_service::save_config(&cfg)?;
    *config = Some(cfg);
    Ok(())
}

#[tauri::command]
pub fn update_settings(settings: Settings, state: State<'_, AppState>) -> Result<(), String> {
    let mut config = state.config.lock().map_err(|e| e.to_string())?;
    let mut cfg = config.clone().unwrap_or_default();
    cfg.settings = settings;
    config_service::save_config(&cfg)?;
    *config = Some(cfg);
    Ok(())
}
