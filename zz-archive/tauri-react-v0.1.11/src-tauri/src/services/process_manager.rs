use crate::services::platform;

/// Kill a process tree by PID (Windows)
#[allow(dead_code)]
pub fn kill_process_tree(pid: u32) -> Result<(), String> {
    platform::kill_process_tree(pid)
}

/// Check if a process is still running
#[allow(dead_code)]
pub fn is_process_running(pid: u32) -> bool {
    platform::is_pid_running(pid)
}
