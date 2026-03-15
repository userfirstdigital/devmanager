use crate::models::config::{PortStatus, PortConflict, PortConflictEntry};
use regex::Regex;
use std::collections::HashMap;
use tauri::State;
use crate::state::AppState;

/// Find the PID listening on a given port using netstat
fn find_pid_on_port(port: u16) -> Result<Option<u32>, String> {
    let output = std::process::Command::new("netstat")
        .args(["-ano"])
        .output()
        .map_err(|e| format!("Failed to run netstat: {}", e))?;

    if !output.status.success() || output.stdout.is_empty() {
        return Ok(None);
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let port_suffix = format!(":{}", port);

    // Parse netstat output lines, looking for LISTENING state with exact port match
    // Format: "  TCP    0.0.0.0:3000    0.0.0.0:0    LISTENING    12345"
    for line in stdout.lines() {
        let line = line.trim();
        if !line.contains("LISTENING") {
            continue;
        }

        // Split into columns and check if the local address ends with exactly :<port>
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() >= 5 {
            let local_addr = parts[1];
            // Exact match: address must end with ":PORT" and not ":PORT0", ":PORT1", etc.
            if local_addr.ends_with(&port_suffix) {
                // Verify it's an exact port match by checking the char before the port suffix
                // is ':' (i.e., the entire suffix after the last ':' is our port)
                if let Some(addr_port_str) = local_addr.rsplit(':').next() {
                    if addr_port_str == port.to_string() {
                        if let Ok(pid) = parts[parts.len() - 1].parse::<u32>() {
                            return Ok(Some(pid));
                        }
                    }
                }
            }
        }
    }

    Ok(None)
}

/// Get the process name for a given PID using tasklist
fn get_process_name(pid: u32) -> Result<Option<String>, String> {
    let output = std::process::Command::new("tasklist")
        .args(["/FI", &format!("PID eq {}", pid), "/FO", "CSV", "/NH"])
        .output()
        .map_err(|e| format!("Failed to run tasklist: {}", e))?;

    if !output.status.success() {
        return Ok(None);
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let line = stdout.trim();
    if line.is_empty() || line.starts_with("INFO:") {
        return Ok(None);
    }

    // CSV format: "process_name.exe","PID","Session Name","Session#","Mem Usage"
    // Remove surrounding quotes and split by ","
    let parts: Vec<&str> = line.split(',').collect();
    if let Some(name) = parts.first() {
        let name = name.trim_matches('"').to_string();
        return Ok(Some(name));
    }

    Ok(None)
}

#[tauri::command]
pub fn check_port_in_use(port: u16) -> Result<PortStatus, String> {
    match find_pid_on_port(port)? {
        Some(pid) => {
            let process_name = get_process_name(pid)?;
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
    let pid = find_pid_on_port(port)?
        .ok_or_else(|| format!("No process found listening on port {}", port))?;

    let output = std::process::Command::new("taskkill")
        .args(["/F", "/PID", &pid.to_string()])
        .output()
        .map_err(|e| format!("Failed to run taskkill: {}", e))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("Failed to kill process {}: {}", pid, stderr));
    }

    Ok(())
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
pub fn update_env_port(env_file_path: String, variable: String, new_port: u16) -> Result<(), String> {
    let contents = std::fs::read_to_string(&env_file_path)
        .map_err(|e| format!("Failed to read env file: {}", e))?;

    // Build a regex to match the specific variable line
    let pattern = format!(r"(?im)^({})\s*=\s*\d+", regex::escape(&variable));
    let re = Regex::new(&pattern)
        .map_err(|e| format!("Failed to compile regex: {}", e))?;

    let new_contents = re.replace(&contents, format!("{}={}", variable, new_port)).to_string();

    // Atomic write
    let path = std::path::Path::new(&env_file_path);
    let temp_path = path.with_extension("tmp");
    std::fs::write(&temp_path, &new_contents)
        .map_err(|e| format!("Failed to write temp env file: {}", e))?;
    std::fs::rename(&temp_path, path)
        .map_err(|e| format!("Failed to rename temp env file: {}", e))?;

    Ok(())
}
