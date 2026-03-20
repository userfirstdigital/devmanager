use crate::services::platform;
use crate::state::AppState;
use tauri::State;

#[tauri::command]
pub fn open_terminal(
    state: State<'_, AppState>,
    folder_path: String,
    shell_path: Option<String>,
) -> Result<(), String> {
    platform::open_terminal(&state.runtime_platform, &folder_path, shell_path.as_deref())
}
