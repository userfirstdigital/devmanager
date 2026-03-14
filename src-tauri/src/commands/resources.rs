use crate::models::config::ProcessTreeInfo;
use crate::services::resource_service;
use crate::state::AppState;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tauri::{AppHandle, Emitter, State};

#[tauri::command]
pub fn get_process_resources(command_id: String, pid: u32) -> Result<ProcessTreeInfo, String> {
    resource_service::get_process_tree_resources(&command_id, pid)
}

#[tauri::command]
pub fn start_resource_monitor(
    command_id: String,
    pid: u32,
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<(), String> {
    let mut monitors = state.resource_monitors.lock().map_err(|e| e.to_string())?;

    // If there's already a monitor for this command, stop it first
    if let Some(existing_flag) = monitors.get(&command_id) {
        existing_flag.store(false, Ordering::Relaxed);
    }

    let running = Arc::new(AtomicBool::new(true));
    monitors.insert(command_id.clone(), running.clone());

    let cmd_id = command_id.clone();

    // Spawn a background task via Tauri's managed tokio runtime
    tauri::async_runtime::spawn(async move {
        let mut sys = sysinfo::System::new();

        while running.load(Ordering::Relaxed) {
            // Refresh processes
            sys.refresh_processes(sysinfo::ProcessesToUpdate::All, true);

            let root_pid = sysinfo::Pid::from_u32(pid);

            // Check if root process still exists
            if sys.process(root_pid).is_none() {
                break;
            }

            // Collect process tree
            let mut tree_pids: Vec<sysinfo::Pid> = Vec::new();
            tree_pids.push(root_pid);

            for (proc_pid, process) in sys.processes() {
                if *proc_pid == root_pid {
                    continue;
                }
                let mut current_pid = process.parent();
                while let Some(parent_pid) = current_pid {
                    if parent_pid == root_pid {
                        tree_pids.push(*proc_pid);
                        break;
                    }
                    match sys.process(parent_pid) {
                        Some(parent_proc) => current_pid = parent_proc.parent(),
                        None => break,
                    }
                }
            }

            let mut processes = Vec::new();
            let mut total_memory_mb = 0.0;
            let mut total_cpu_percent: f32 = 0.0;

            for proc_pid in &tree_pids {
                if let Some(process) = sys.process(*proc_pid) {
                    let memory_mb = process.memory() as f64 / (1024.0 * 1024.0);
                    let cpu_percent = process.cpu_usage();
                    total_memory_mb += memory_mb;
                    total_cpu_percent += cpu_percent;
                    processes.push(crate::models::config::ChildProcessInfo {
                        pid: proc_pid.as_u32(),
                        name: process.name().to_string_lossy().to_string(),
                        memory_mb,
                        cpu_percent,
                    });
                }
            }

            let info = ProcessTreeInfo {
                command_id: cmd_id.clone(),
                processes,
                total_memory_mb,
                total_cpu_percent,
            };

            // Emit the resource-update event
            let _ = app.emit("resource-update", &info);

            // Sleep for 3 seconds
            tokio::time::sleep(std::time::Duration::from_secs(3)).await;
        }
    });

    Ok(())
}

#[tauri::command]
pub fn stop_resource_monitor(
    command_id: String,
    state: State<'_, AppState>,
) -> Result<(), String> {
    let mut monitors = state.resource_monitors.lock().map_err(|e| e.to_string())?;
    if let Some(flag) = monitors.remove(&command_id) {
        flag.store(false, Ordering::Relaxed);
    }
    Ok(())
}
