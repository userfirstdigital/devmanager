use sysinfo::System;

/// Kill a process tree by PID (Windows)
#[allow(dead_code)]
pub fn kill_process_tree(pid: u32) -> Result<(), String> {
    let output = std::process::Command::new("taskkill")
        .args(["/T", "/F", "/PID", &pid.to_string()])
        .output()
        .map_err(|e| format!("Failed to run taskkill: {}", e))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("taskkill failed: {}", stderr));
    }

    Ok(())
}

/// Check if a process is still running
#[allow(dead_code)]
pub fn is_process_running(pid: u32) -> bool {
    let mut sys = System::new();
    sys.refresh_processes(sysinfo::ProcessesToUpdate::All, true);
    let sysinfo_pid = sysinfo::Pid::from_u32(pid);
    sys.process(sysinfo_pid).is_some()
}
