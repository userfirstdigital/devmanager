use crate::services::pid_file;
use crate::services::platform;
use crate::state::{AppState, ProcessInfo};
use std::collections::HashMap;
use std::time::{Duration, Instant};
use tauri::State;

#[tauri::command]
pub fn register_process(
    key: String,
    pid: u32,
    command_id: String,
    project_id: String,
    state: State<'_, AppState>,
) -> Result<(), String> {
    let mut processes = state.processes.lock().map_err(|e| e.to_string())?;
    processes.insert(
        key,
        ProcessInfo {
            pid,
            command_id,
            project_id,
        },
    );
    pid_file::track_pid(pid);
    Ok(())
}

#[tauri::command]
pub fn unregister_process(key: String, state: State<'_, AppState>) -> Result<(), String> {
    let mut processes = state.processes.lock().map_err(|e| e.to_string())?;
    if let Some(info) = processes.remove(&key) {
        pid_file::untrack_pid(info.pid);
    }
    Ok(())
}

#[tauri::command]
pub fn kill_process_tree(pid: u32) -> Result<(), String> {
    platform::kill_process_tree(pid)
}

#[tauri::command]
pub fn get_running_processes(state: State<'_, AppState>) -> Result<HashMap<String, u32>, String> {
    let processes = state.processes.lock().map_err(|e| e.to_string())?;
    let result: HashMap<String, u32> = processes
        .iter()
        .map(|(key, info)| (key.clone(), info.pid))
        .collect();
    Ok(result)
}

#[tauri::command]
pub async fn wait_for_managed_shutdown(
    state: State<'_, AppState>,
    timeout_ms: Option<u64>,
) -> Result<(), String> {
    let timeout = Duration::from_millis(timeout_ms.unwrap_or(15_000));
    let started_at = Instant::now();

    loop {
        let tracked_pids: Vec<u32> = {
            let processes = state.processes.lock().map_err(|e| e.to_string())?;
            processes.values().map(|info| info.pid).collect()
        };

        let active_sessions = {
            let sessions = state.pty_sessions.lock().map_err(|e| e.to_string())?;
            sessions.len()
        };

        let alive_pids: Vec<u32> = tracked_pids
            .iter()
            .copied()
            .filter(|pid| platform::is_pid_running(*pid))
            .collect();

        if active_sessions == 0 && alive_pids.is_empty() {
            {
                let mut processes = state.processes.lock().map_err(|e| e.to_string())?;
                processes.clear();
            }
            {
                let mut monitored = state
                    .monitored_processes
                    .lock()
                    .map_err(|e| e.to_string())?;
                monitored.clear();
            }
            pid_file::clear_all();
            return Ok(());
        }

        if started_at.elapsed() >= timeout {
            return Err(format!(
                "Timed out waiting for {} PTY session(s) and {} process(es) to stop",
                active_sessions,
                alive_pids.len()
            ));
        }

        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}
