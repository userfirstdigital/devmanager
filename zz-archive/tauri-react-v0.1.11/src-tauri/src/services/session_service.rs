use crate::models::config::SessionState;
use std::path::PathBuf;

/// Get the session file path
pub fn get_session_path() -> Result<PathBuf, String> {
    let config_dir =
        dirs::config_dir().ok_or_else(|| "Could not determine config directory".to_string())?;
    Ok(config_dir
        .join("com.userfirst.devmanager")
        .join("session.json"))
}

/// Load session state from disk. If the file is corrupt or from an old schema, delete it and start fresh.
pub fn load_session() -> Result<SessionState, String> {
    let path = get_session_path()?;
    if !path.exists() {
        return Ok(SessionState::default());
    }
    let contents = std::fs::read_to_string(&path)
        .map_err(|e| format!("Failed to read session file: {}", e))?;

    match serde_json::from_str::<SessionState>(&contents) {
        Ok(session) => Ok(session),
        Err(_) => {
            // Old or corrupt session — delete and start fresh
            let _ = std::fs::remove_file(&path);
            Ok(SessionState::default())
        }
    }
}

/// Save session state to disk (atomic write)
pub fn save_session(session: &SessionState) -> Result<(), String> {
    let path = get_session_path()?;

    // Create parent directories if needed
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("Failed to create session directory: {}", e))?;
    }

    let contents = serde_json::to_string_pretty(session)
        .map_err(|e| format!("Failed to serialize session: {}", e))?;

    // Atomic write: write to temp file, then rename
    let temp_path = path.with_extension("json.tmp");
    std::fs::write(&temp_path, &contents)
        .map_err(|e| format!("Failed to write temp session file: {}", e))?;
    std::fs::rename(&temp_path, &path)
        .map_err(|e| format!("Failed to rename temp session file: {}", e))?;

    Ok(())
}
