use crate::models::config::ProcessTreeInfo;
use crate::services::resource_service;
use crate::state::{AppState, MonitorEntry};
use tauri::State;

#[tauri::command]
pub fn get_process_resources(command_id: String, pid: u32) -> Result<ProcessTreeInfo, String> {
    resource_service::get_process_tree_resources(&command_id, pid)
}

#[tauri::command]
pub fn start_resource_monitor(
    command_id: String,
    pid: u32,
    state: State<'_, AppState>,
) -> Result<(), String> {
    let mut monitors = state
        .monitored_processes
        .lock()
        .map_err(|e| e.to_string())?;
    monitors.insert(command_id.clone(), MonitorEntry { command_id, pid });
    Ok(())
}

#[tauri::command]
pub fn stop_resource_monitor(command_id: String, state: State<'_, AppState>) -> Result<(), String> {
    let mut monitors = state
        .monitored_processes
        .lock()
        .map_err(|e| e.to_string())?;
    monitors.remove(&command_id);
    Ok(())
}
