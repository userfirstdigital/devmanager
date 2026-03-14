use tauri::State;
use crate::state::{AppState, ProcessInfo};
use crate::services::pid_file;
use std::collections::HashMap;

#[tauri::command]
pub fn register_process(
    key: String,
    pid: u32,
    command_id: String,
    project_id: String,
    state: State<'_, AppState>,
) -> Result<(), String> {
    let mut processes = state.processes.lock().map_err(|e| e.to_string())?;
    processes.insert(key, ProcessInfo {
        pid,
        command_id,
        project_id,
    });
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
    std::process::Command::new("taskkill")
        .args(["/T", "/F", "/PID", &pid.to_string()])
        .output()
        .map_err(|e| e.to_string())?;
    Ok(())
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
