use crate::models::config::{PortConflict, PortConflictEntry, PortStatus};
use crate::services::platform;
use crate::state::AppState;
use regex::Regex;
use std::collections::HashMap;
use tauri::State;

#[tauri::command]
pub fn check_port_in_use(port: u16) -> Result<PortStatus, String> {
    match platform::find_pid_on_port(port)? {
        Some(pid) => {
            let process_name = platform::get_process_name(pid)?;
            Ok(PortStatus {
                port,
                in_use: true,
                pid: Some(pid),
                process_name,
            })
        }
        None => Ok(PortStatus {
            port,
            in_use: false,
            pid: None,
            process_name: None,
        }),
    }
}

#[tauri::command]
pub fn kill_port(port: u16) -> Result<(), String> {
    let pid = platform::find_pid_on_port(port)?
        .ok_or_else(|| format!("No process found listening on port {}", port))?;
    platform::kill_process(pid)
}

#[tauri::command]
pub fn get_port_conflicts(state: State<'_, AppState>) -> Result<Vec<PortConflict>, String> {
    let config = state.config.read().map_err(|e| e.to_string())?;
    let cfg = match config.as_ref() {
        Some(c) => c,
        None => return Ok(Vec::new()),
    };

    // Collect all ports and which commands use them
    let mut port_map: HashMap<u16, Vec<PortConflictEntry>> = HashMap::new();

    for project in &cfg.projects {
        for folder in &project.folders {
            for command in &folder.commands {
                if let Some(port) = command.port {
                    port_map.entry(port).or_default().push(PortConflictEntry {
                        project_name: project.name.clone(),
                        command_label: command.label.clone(),
                        command_id: command.id.clone(),
                    });
                }
            }
        }
    }

    // Only return entries where more than one command uses the same port
    let conflicts: Vec<PortConflict> = port_map
        .into_iter()
        .filter(|(_, entries)| entries.len() > 1)
        .map(|(port, commands)| PortConflict { port, commands })
        .collect();

    Ok(conflicts)
}

#[tauri::command]
pub fn update_env_port(
    env_file_path: String,
    variable: String,
    new_port: u16,
) -> Result<(), String> {
    let contents = std::fs::read_to_string(&env_file_path)
        .map_err(|e| format!("Failed to read env file: {}", e))?;

    // Build a regex to match the specific variable line
    let pattern = format!(r"(?im)^({})\s*=\s*\d+", regex::escape(&variable));
    let re = Regex::new(&pattern).map_err(|e| format!("Failed to compile regex: {}", e))?;

    let new_contents = re
        .replace(&contents, format!("{}={}", variable, new_port))
        .to_string();

    // Atomic write
    let path = std::path::Path::new(&env_file_path);
    let temp_path = path.with_extension("tmp");
    std::fs::write(&temp_path, &new_contents)
        .map_err(|e| format!("Failed to write temp env file: {}", e))?;
    std::fs::rename(&temp_path, path)
        .map_err(|e| format!("Failed to rename temp env file: {}", e))?;

    Ok(())
}
