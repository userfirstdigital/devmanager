use crate::models::config::EnvEntry;

#[tauri::command]
pub fn read_env_file(file_path: String) -> Result<Vec<EnvEntry>, String> {
    let contents = std::fs::read_to_string(&file_path)
        .map_err(|e| format!("Failed to read env file: {}", e))?;

    let mut entries = Vec::new();

    for line in contents.lines() {
        if line.trim().is_empty() {
            entries.push(EnvEntry {
                entry_type: "blank".to_string(),
                key: None,
                value: None,
                raw: String::new(),
            });
        } else if line.trim_start().starts_with('#') {
            entries.push(EnvEntry {
                entry_type: "comment".to_string(),
                key: None,
                value: None,
                raw: line.to_string(),
            });
        } else if let Some(eq_pos) = line.find('=') {
            let key = line[..eq_pos].trim().to_string();
            let value = line[eq_pos + 1..].trim().to_string();
            entries.push(EnvEntry {
                entry_type: "variable".to_string(),
                key: Some(key),
                value: Some(value),
                raw: line.to_string(),
            });
        } else {
            // Line that doesn't match any pattern - treat as comment
            entries.push(EnvEntry {
                entry_type: "comment".to_string(),
                key: None,
                value: None,
                raw: line.to_string(),
            });
        }
    }

    Ok(entries)
}

#[tauri::command]
pub fn write_env_file(file_path: String, entries: Vec<EnvEntry>) -> Result<(), String> {
    let mut lines = Vec::new();

    for entry in &entries {
        match entry.entry_type.as_str() {
            "blank" => lines.push(String::new()),
            "comment" => lines.push(entry.raw.clone()),
            "variable" => {
                if let (Some(key), Some(value)) = (&entry.key, &entry.value) {
                    lines.push(format!("{}={}", key, value));
                } else {
                    // Fallback to raw if key/value not set
                    lines.push(entry.raw.clone());
                }
            }
            _ => lines.push(entry.raw.clone()),
        }
    }

    // Use \r\n on Windows to preserve native line endings
    let contents = lines.join("\r\n");

    // Atomic write
    let path = std::path::Path::new(&file_path);
    let temp_path = path.with_extension("tmp");
    std::fs::write(&temp_path, &contents)
        .map_err(|e| format!("Failed to write temp env file: {}", e))?;
    std::fs::rename(&temp_path, path)
        .map_err(|e| format!("Failed to rename temp env file: {}", e))?;

    Ok(())
}
