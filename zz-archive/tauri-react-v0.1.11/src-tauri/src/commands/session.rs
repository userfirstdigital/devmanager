use crate::models::config::SessionState;
use crate::services::session_service;

#[tauri::command]
pub fn get_session() -> Result<SessionState, String> {
    session_service::load_session()
}

#[tauri::command]
pub fn save_session(session: SessionState) -> Result<(), String> {
    session_service::save_session(&session)
}
