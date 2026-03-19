use crate::services::platform;
use crate::state::AppState;
use tauri::{AppHandle, State};

#[tauri::command]
pub fn get_runtime_info(
    state: State<'_, AppState>,
) -> Result<platform::RuntimePlatformInfo, String> {
    Ok(platform::runtime_info(&state.runtime_platform))
}

#[tauri::command]
pub fn quit_app(app: AppHandle, state: State<'_, AppState>) -> Result<(), String> {
    platform::shutdown_managed_processes(&state);
    app.exit(0);
    Ok(())
}
