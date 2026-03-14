use std::collections::HashMap;
use std::io::{Read, Write};
use std::sync::{Arc, Mutex};
use base64::Engine;
use portable_pty::{CommandBuilder, NativePtySystem, PtySize, PtySystem};
use tauri::{AppHandle, Emitter, State};
use crate::state::{AppState, PtySession};

#[tauri::command]
pub async fn create_pty_session(
    app: AppHandle,
    state: State<'_, AppState>,
    id: String,
    cwd: String,
    command: String,
    args: Vec<String>,
    env: Option<HashMap<String, String>>,
    cols: u16,
    rows: u16,
) -> Result<u32, String> {
    let pty_system = NativePtySystem::default();

    let pair = pty_system
        .openpty(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(|e| format!("Failed to open PTY: {}", e))?;

    let mut cmd = CommandBuilder::new(&command);
    cmd.args(&args);
    cmd.cwd(&cwd);

    if let Some(env_vars) = env {
        for (key, value) in env_vars {
            cmd.env(key, value);
        }
    }

    let child = pair
        .slave
        .spawn_command(cmd)
        .map_err(|e| format!("Failed to spawn command: {}", e))?;

    let pid = child
        .process_id()
        .ok_or_else(|| "Failed to get child PID".to_string())?;

    let writer: Box<dyn Write + Send> = pair
        .master
        .take_writer()
        .map_err(|e| format!("Failed to take PTY writer: {}", e))?;

    let mut reader = pair
        .master
        .try_clone_reader()
        .map_err(|e| format!("Failed to clone PTY reader: {}", e))?;

    let writer = Arc::new(Mutex::new(writer));

    let session = PtySession {
        writer: writer.clone(),
        master: pair.master,
        child,
        session_id: id.clone(),
    };

    // Remove any existing session first, then release the lock before blocking on wait()
    let old_session = {
        let mut sessions = state.pty_sessions.lock().unwrap();
        let old = sessions.remove(&id);
        sessions.insert(id.clone(), session);
        old
    };
    // Kill outside the lock to avoid deadlocking other PTY operations
    if let Some(mut old) = old_session {
        let _ = old.child.kill();
        let _ = old.child.wait();
    }

    // Start async read loop
    let event_id = format!("pty-data-{}", id);
    let exit_event_id = format!("pty-exit-{}", id);
    let session_id = id.clone();

    tauri::async_runtime::spawn_blocking(move || {
        let mut buf = [0u8; 4096];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    let encoded = base64::engine::general_purpose::STANDARD.encode(&buf[..n]);
                    let _ = app.emit(&event_id, encoded);
                }
                Err(_) => break,
            }
        }
        let _ = app.emit(&exit_event_id, session_id);
    });

    Ok(pid)
}

#[tauri::command]
pub async fn write_pty(
    state: State<'_, AppState>,
    id: String,
    data: String,
) -> Result<(), String> {
    let sessions = state.pty_sessions.lock().unwrap();
    let session = sessions
        .get(&id)
        .ok_or_else(|| format!("PTY session '{}' not found", id))?;

    let mut writer = session.writer.lock().unwrap();
    writer
        .write_all(data.as_bytes())
        .map_err(|e| format!("Failed to write to PTY: {}", e))?;
    writer
        .flush()
        .map_err(|e| format!("Failed to flush PTY: {}", e))?;

    Ok(())
}

#[tauri::command]
pub async fn resize_pty(
    state: State<'_, AppState>,
    id: String,
    cols: u16,
    rows: u16,
) -> Result<(), String> {
    let sessions = state.pty_sessions.lock().unwrap();
    let session = sessions
        .get(&id)
        .ok_or_else(|| format!("PTY session '{}' not found", id))?;

    session
        .master
        .resize(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(|e| format!("Failed to resize PTY: {}", e))?;

    Ok(())
}

#[tauri::command]
pub async fn close_pty(
    state: State<'_, AppState>,
    id: String,
) -> Result<(), String> {
    // Remove session from map first, then release the lock before blocking on wait()
    let session = {
        let mut sessions = state.pty_sessions.lock().unwrap();
        sessions.remove(&id)
    };
    // Kill outside the lock to avoid deadlocking other PTY operations
    if let Some(mut session) = session {
        let _ = session.child.kill();
        let _ = session.child.wait();
    }
    Ok(())
}
