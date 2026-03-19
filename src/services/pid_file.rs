use std::collections::HashSet;
use std::path::PathBuf;

/// Get the PID file path (alongside config)
fn pid_file_path() -> Result<PathBuf, String> {
    let config_dir =
        dirs::config_dir().ok_or_else(|| "Could not determine config directory".to_string())?;
    Ok(config_dir
        .join("com.userfirst.devmanager")
        .join("running-pids.json"))
}

fn read_pids() -> HashSet<u32> {
    let path = match pid_file_path() {
        Ok(path) => path,
        Err(_) => return HashSet::new(),
    };
    let contents = match std::fs::read_to_string(&path) {
        Ok(contents) => contents,
        Err(_) => return HashSet::new(),
    };
    serde_json::from_str(&contents).unwrap_or_default()
}

fn write_pids(pids: &HashSet<u32>) {
    let path = match pid_file_path() {
        Ok(path) => path,
        Err(_) => return,
    };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(contents) = serde_json::to_string(pids) {
        let _ = std::fs::write(&path, contents);
    }
}

pub fn track_pid(pid: u32) {
    let mut pids = read_pids();
    pids.insert(pid);
    write_pids(&pids);
}

pub fn untrack_pid(pid: u32) {
    let mut pids = read_pids();
    pids.remove(&pid);
    write_pids(&pids);
}

pub fn clear_all() {
    write_pids(&HashSet::new());
}

pub fn cleanup_orphaned_processes() {
    let pids = read_pids();
    if pids.is_empty() {
        return;
    }

    let mut system = sysinfo::System::new();
    system.refresh_processes(sysinfo::ProcessesToUpdate::All, true);

    for pid in &pids {
        let sys_pid = sysinfo::Pid::from(*pid as usize);
        if let Some(process) = system.process(sys_pid) {
            let _ = process.kill();
        }
    }

    clear_all();
}
