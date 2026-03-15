use std::collections::HashSet;
use std::path::PathBuf;

/// Get the PID file path (alongside config)
fn get_pid_file_path() -> Result<PathBuf, String> {
    let config_dir = dirs::config_dir()
        .ok_or_else(|| "Could not determine config directory".to_string())?;
    Ok(config_dir.join("com.userfirst.devmanager").join("running-pids.json"))
}

/// Read all tracked PIDs from disk
fn read_pids() -> HashSet<u32> {
    let path = match get_pid_file_path() {
        Ok(p) => p,
        Err(_) => return HashSet::new(),
    };
    let contents = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(_) => return HashSet::new(),
    };
    serde_json::from_str(&contents).unwrap_or_default()
}

/// Write all tracked PIDs to disk
fn write_pids(pids: &HashSet<u32>) {
    let path = match get_pid_file_path() {
        Ok(p) => p,
        Err(_) => return,
    };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(contents) = serde_json::to_string(pids) {
        let _ = std::fs::write(&path, contents);
    }
}

/// Add a PID to the tracking file
pub fn track_pid(pid: u32) {
    let mut pids = read_pids();
    pids.insert(pid);
    write_pids(&pids);
}

/// Remove a PID from the tracking file
pub fn untrack_pid(pid: u32) {
    let mut pids = read_pids();
    pids.remove(&pid);
    write_pids(&pids);
}

/// Clear all tracked PIDs (called on graceful shutdown)
pub fn clear_all() {
    write_pids(&HashSet::new());
}

/// Kill any orphaned processes from a previous session and clear the file.
/// Called once at app startup.
pub fn cleanup_orphaned_processes() {
    let pids = read_pids();
    if pids.is_empty() {
        return;
    }
    eprintln!("[DevManager] Checking {} tracked PID(s) from previous session", pids.len());
    for pid in &pids {
        let output = std::process::Command::new("tasklist")
            .args(["/FI", &format!("PID eq {}", pid), "/NH", "/FO", "CSV"])
            .output();
        match output {
            Ok(out) if String::from_utf8_lossy(&out.stdout).contains(&pid.to_string()) => {
                eprintln!("[DevManager] Killing orphaned PID {}", pid);
                let _ = std::process::Command::new("taskkill")
                    .args(["/T", "/F", "/PID", &pid.to_string()])
                    .output();
            }
            _ => {
                eprintln!("[DevManager] PID {} already dead, skipping", pid);
            }
        }
    }
    write_pids(&HashSet::new());
}
