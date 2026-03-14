use crate::models::config::{ProcessTreeInfo, ChildProcessInfo};
use sysinfo::{Pid, System};

/// Get resource usage for a process tree
pub fn get_process_tree_resources(command_id: &str, pid: u32) -> Result<ProcessTreeInfo, String> {
    let mut sys = System::new();
    sys.refresh_processes(sysinfo::ProcessesToUpdate::All, true);

    let root_pid = Pid::from_u32(pid);

    // Find the root process
    if sys.process(root_pid).is_none() {
        return Err(format!("Process {} not found", pid));
    }

    // Collect all descendant PIDs by walking the parent chain
    // For each process, check if its parent chain leads back to root_pid
    let mut tree_pids: Vec<Pid> = Vec::new();
    tree_pids.push(root_pid);

    for (proc_pid, process) in sys.processes() {
        if *proc_pid == root_pid {
            continue;
        }
        // Walk up the parent chain to see if this process is a descendant
        let mut current_pid = process.parent();
        while let Some(parent_pid) = current_pid {
            if parent_pid == root_pid {
                tree_pids.push(*proc_pid);
                break;
            }
            // Get the parent process and continue walking up
            match sys.process(parent_pid) {
                Some(parent_proc) => current_pid = parent_proc.parent(),
                None => break,
            }
        }
    }

    // Collect info for all processes in the tree
    let mut processes = Vec::new();
    let mut total_memory_mb = 0.0;
    let mut total_cpu_percent: f32 = 0.0;

    for proc_pid in &tree_pids {
        if let Some(process) = sys.process(*proc_pid) {
            let memory_mb = process.memory() as f64 / (1024.0 * 1024.0);
            let cpu_percent = process.cpu_usage();

            total_memory_mb += memory_mb;
            total_cpu_percent += cpu_percent;

            processes.push(ChildProcessInfo {
                pid: proc_pid.as_u32(),
                name: process.name().to_string_lossy().to_string(),
                memory_mb,
                cpu_percent,
            });
        }
    }

    Ok(ProcessTreeInfo {
        command_id: command_id.to_string(),
        processes,
        total_memory_mb,
        total_cpu_percent,
    })
}
