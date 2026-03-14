use crate::models::config::AppConfig;
use std::path::PathBuf;

/// Get the config file path
pub fn get_config_path() -> Result<PathBuf, String> {
    let config_dir = dirs::config_dir()
        .ok_or_else(|| "Could not determine config directory".to_string())?;
    Ok(config_dir.join("com.userfirst.devmanager").join("config.json"))
}

/// Load config from disk. If the file is corrupt or from an old schema, delete it and start fresh.
pub fn load_config() -> Result<AppConfig, String> {
    let path = get_config_path()?;
    if !path.exists() {
        return Ok(AppConfig::default());
    }
    let contents = std::fs::read_to_string(&path)
        .map_err(|e| format!("Failed to read config file: {}", e))?;

    match serde_json::from_str::<AppConfig>(&contents) {
        Ok(config) => Ok(config),
        Err(_) => {
            // Old or corrupt config — delete and start fresh
            let _ = std::fs::remove_file(&path);
            Ok(AppConfig::default())
        }
    }
}

/// Save config to disk (atomic write: write to temp file, then rename)
pub fn save_config(config: &AppConfig) -> Result<(), String> {
    let path = get_config_path()?;

    // Create parent directories if needed
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("Failed to create config directory: {}", e))?;
    }

    let contents = serde_json::to_string_pretty(config)
        .map_err(|e| format!("Failed to serialize config: {}", e))?;

    // Write to a temp file in the same directory, then rename for atomicity
    let temp_path = path.with_extension("json.tmp");
    std::fs::write(&temp_path, &contents)
        .map_err(|e| format!("Failed to write temp config file: {}", e))?;
    std::fs::rename(&temp_path, &path)
        .map_err(|e| format!("Failed to rename temp config file: {}", e))?;

    Ok(())
}
