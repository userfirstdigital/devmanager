use crate::services::{pid_file, platform};
use crate::state::{AppState, MonitorEntry, ProcessInfo, PtyOutputBuffer, PtySession};
use base64::Engine;
use portable_pty::{CommandBuilder, NativePtySystem, PtySize, PtySystem};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs::File;
use std::io::{BufWriter, Read, Write};
use std::sync::{Arc, Mutex};
use tauri::{AppHandle, Emitter, Manager, State};

/// Per-session ring buffer cap: 4 MB.
/// ~35-50k lines of terminal output. Pre-allocated upfront to avoid VecDeque doubling cascades.
const PTY_BUFFER_MAX_BYTES: usize = 4 * 1024 * 1024;

/// Inner helper that creates a PTY session and returns the PID.
/// Does not touch processes or monitors — the caller handles that.
fn create_pty_session_inner(
    app: &AppHandle,
    state: &State<'_, AppState>,
    id: &str,
    cwd: &str,
    command: &str,
    args: &[String],
    env: Option<&HashMap<String, String>>,
    cols: u16,
    rows: u16,
    log_file: Option<&str>,
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

    let mut cmd = CommandBuilder::new(command);
    cmd.args(args);
    cmd.cwd(cwd);

    platform::apply_runtime_env(&mut cmd, &state.runtime_platform, env);

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

    let reader = pair
        .master
        .try_clone_reader()
        .map_err(|e| format!("Failed to clone PTY reader: {}", e))?;

    let writer = Arc::new(Mutex::new(writer));

    let session = PtySession {
        writer: writer.clone(),
        master: pair.master,
        child,
        session_id: id.to_string(),
    };

    // Remove any existing session first, then release the lock before blocking on wait()
    let old_session = {
        let mut sessions = state.pty_sessions.lock().unwrap();
        let old = sessions.remove(id);
        sessions.insert(id.to_string(), session);
        old
    };
    // Kill outside the lock to avoid deadlocking other PTY operations
    if let Some(mut old) = old_session {
        platform::kill_pty_session(&mut old);
    }

    // Create ring buffer for this session
    let buffer = Arc::new(Mutex::new(PtyOutputBuffer::new(PTY_BUFFER_MAX_BYTES)));
    {
        let mut buffers = state.pty_buffers.lock().unwrap();
        buffers.insert(id.to_string(), buffer.clone());
    }

    start_pty_read_loop(
        app.clone(),
        reader,
        id.to_string(),
        buffer,
        log_file.map(|s| s.to_string()),
    );

    Ok(pid)
}

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
    log_file: Option<String>,
) -> Result<u32, String> {
    create_pty_session_inner(
        &app,
        &state,
        &id,
        &cwd,
        &command,
        &args,
        env.as_ref(),
        cols,
        rows,
        log_file.as_deref(),
    )
}

/// Batch command: create PTY + register process + start resource monitor atomically.
/// Returns `{ pid, command_id }`. Rolls back on failure.
#[derive(Serialize)]
pub struct ServerSessionResult {
    pub pid: u32,
    pub command_id: String,
}

#[tauri::command]
pub async fn create_server_session(
    app: AppHandle,
    state: State<'_, AppState>,
    id: String,
    cwd: String,
    command: String,
    args: Vec<String>,
    env: Option<HashMap<String, String>>,
    cols: u16,
    rows: u16,
    log_file: Option<String>,
    command_id: String,
    project_id: String,
) -> Result<ServerSessionResult, String> {
    let pid = create_pty_session_inner(
        &app,
        &state,
        &id,
        &cwd,
        &command,
        &args,
        env.as_ref(),
        cols,
        rows,
        log_file.as_deref(),
    )?;

    // Register process
    {
        let mut processes = state.processes.lock().map_err(|e| e.to_string())?;
        processes.insert(
            id.clone(),
            ProcessInfo {
                pid,
                command_id: command_id.clone(),
                project_id,
            },
        );
    }
    pid_file::track_pid(pid);

    // Start resource monitor (just insert into the map — the unified loop picks it up)
    {
        let mut monitors = state
            .monitored_processes
            .lock()
            .map_err(|e| e.to_string())?;
        monitors.insert(
            command_id.clone(),
            MonitorEntry {
                command_id: command_id.clone(),
                pid,
            },
        );
    }

    Ok(ServerSessionResult { pid, command_id })
}

/// Request for batch session restore
#[derive(Deserialize)]
pub struct RestoreRequest {
    pub id: String,
    pub cwd: String,
    pub command: String,
    pub args: Vec<String>,
    pub env: Option<HashMap<String, String>>,
    pub cols: Option<u16>,
    pub rows: Option<u16>,
    pub project_id: String,
    #[serde(rename = "checkAlive")]
    pub check_alive: bool,
}

/// Result for each restore request
#[derive(Serialize)]
pub struct RestoreResult {
    pub id: String,
    pub pid: Option<u32>,
    pub alive: bool,
    pub error: Option<String>,
}

#[tauri::command]
pub async fn restore_sessions(
    app: AppHandle,
    state: State<'_, AppState>,
    requests: Vec<RestoreRequest>,
) -> Result<Vec<RestoreResult>, String> {
    let mut results = Vec::with_capacity(requests.len());

    for req in requests {
        // Check if session is still alive from a previous run
        if req.check_alive {
            let sessions = state.pty_sessions.lock().unwrap();
            if sessions.contains_key(&req.id) {
                // Session exists in HashMap = still alive, reconnect
                results.push(RestoreResult {
                    id: req.id,
                    pid: None,
                    alive: true,
                    error: None,
                });
                continue;
            }
        }

        // Create fresh session
        let cols = req.cols.unwrap_or(80);
        let rows = req.rows.unwrap_or(24);

        match create_pty_session_inner(
            &app,
            &state,
            &req.id,
            &req.cwd,
            &req.command,
            &req.args,
            req.env.as_ref(),
            cols,
            rows,
            None,
        ) {
            Ok(pid) => {
                // Register process + monitor
                {
                    let mut processes = state.processes.lock().unwrap();
                    processes.insert(
                        req.id.clone(),
                        ProcessInfo {
                            pid,
                            command_id: req.id.clone(),
                            project_id: req.project_id,
                        },
                    );
                }
                pid_file::track_pid(pid);
                {
                    let mut monitors = state.monitored_processes.lock().unwrap();
                    monitors.insert(
                        req.id.clone(),
                        MonitorEntry {
                            command_id: req.id.clone(),
                            pid,
                        },
                    );
                }

                results.push(RestoreResult {
                    id: req.id,
                    pid: Some(pid),
                    alive: false,
                    error: None,
                });
            }
            Err(e) => {
                results.push(RestoreResult {
                    id: req.id,
                    pid: None,
                    alive: false,
                    error: Some(e),
                });
            }
        }
    }

    Ok(results)
}

/// Helper that strips ANSI escape codes and adds local timestamps to log output.
struct LogWriter {
    writer: BufWriter<File>,
    line_buf: Vec<u8>,
    ansi_re: regex::Regex,
}

impl LogWriter {
    fn new(file: File) -> Self {
        Self {
            writer: BufWriter::new(file),
            line_buf: Vec::new(),
            ansi_re: regex::Regex::new(
                r"\x1b\[[0-9;?]*[a-zA-Z]|\x1b\][^\x07]*\x07|\x1b[()][0-9A-Z]|\x0f",
            )
            .unwrap(),
        }
    }

    fn write_chunk(&mut self, chunk: &[u8]) {
        let text = String::from_utf8_lossy(chunk);
        let clean = self.ansi_re.replace_all(&text, "");
        for ch in clean.bytes() {
            match ch {
                b'\n' => {
                    let ts = chrono::Local::now().format("%Y-%m-%d %H:%M:%S");
                    let line = String::from_utf8_lossy(&self.line_buf);
                    let _ = writeln!(self.writer, "[{}] {}", ts, line.trim_end());
                    self.line_buf.clear();
                }
                b'\r' => {}
                _ => self.line_buf.push(ch),
            }
        }
    }

    fn flush(&mut self) {
        if !self.line_buf.is_empty() {
            let ts = chrono::Local::now().format("%Y-%m-%d %H:%M:%S");
            let line = String::from_utf8_lossy(&self.line_buf);
            let _ = writeln!(self.writer, "[{}] {}", ts, line.trim_end());
            self.line_buf.clear();
        }
        let _ = self.writer.flush();
    }
}

/// Start the background read loop that forwards PTY output as events,
/// stores it in the Rust-side ring buffer, and optionally writes to a log file.
fn start_pty_read_loop(
    app: AppHandle,
    mut reader: Box<dyn Read + Send>,
    id: String,
    buffer: Arc<Mutex<PtyOutputBuffer>>,
    log_file: Option<String>,
) {
    let event_id = format!("pty-data-{}", id);
    let exit_event_id = format!("pty-exit-{}", id);
    let session_id = id.clone();

    tauri::async_runtime::spawn_blocking(move || {
        // Open log file if configured (truncates existing file on fresh start)
        let mut log_writer: Option<LogWriter> =
            log_file.and_then(|path| match File::create(&path) {
                Ok(f) => {
                    eprintln!("[PTY {}] Writing logs to {}", session_id, path);
                    Some(LogWriter::new(f))
                }
                Err(e) => {
                    eprintln!(
                        "[PTY {}] Failed to create log file {}: {}",
                        session_id, path, e
                    );
                    None
                }
            });

        let mut buf = [0u8; 4096];
        let mut backoff_secs: u64 = 1;
        loop {
            match reader.read(&mut buf) {
                Ok(0) => {
                    // EOF — check if user closed the session
                    let state = app.state::<AppState>();
                    let sessions = state.pty_sessions.lock().unwrap();
                    if !sessions.contains_key(&session_id) {
                        eprintln!(
                            "[PTY {}] Session removed (user closed), exiting",
                            session_id
                        );
                        break;
                    }
                    drop(sessions);
                    std::thread::sleep(std::time::Duration::from_secs(backoff_secs));
                    backoff_secs = (backoff_secs * 2).min(10);
                    continue;
                }
                Ok(n) => {
                    backoff_secs = 1;
                    let chunk = &buf[..n];
                    // Store in Rust ring buffer (survives webview reloads)
                    buffer.lock().unwrap().push(chunk);
                    // Write to log file
                    if let Some(ref mut writer) = log_writer {
                        writer.write_chunk(chunk);
                    }
                    // Emit to frontend for live display
                    let encoded = base64::engine::general_purpose::STANDARD.encode(chunk);
                    let _ = app.emit(&event_id, encoded);
                }
                Err(e) => {
                    eprintln!("[PTY {}] Read error: {}", session_id, e);
                    let state = app.state::<AppState>();
                    let sessions = state.pty_sessions.lock().unwrap();
                    if !sessions.contains_key(&session_id) {
                        eprintln!(
                            "[PTY {}] Session removed (user closed), exiting",
                            session_id
                        );
                        break;
                    }
                    drop(sessions);
                    std::thread::sleep(std::time::Duration::from_secs(backoff_secs));
                    backoff_secs = (backoff_secs * 2).min(10);
                    continue;
                }
            }
        }
        // Flush log file on exit
        if let Some(ref mut writer) = log_writer {
            writer.flush();
        }
        let _ = app.emit(&exit_event_id, session_id);
    });
}

/// Drain the ring buffer for a session, returning all buffered output as base64.
/// Called by the frontend when a terminal component mounts to restore recent history.
#[tauri::command]
pub async fn drain_pty_buffer(state: State<'_, AppState>, id: String) -> Result<String, String> {
    let buffers = state.pty_buffers.lock().unwrap();
    if let Some(buffer) = buffers.get(&id) {
        let data = buffer.lock().unwrap().drain();
        if data.is_empty() {
            Ok(String::new())
        } else {
            Ok(base64::engine::general_purpose::STANDARD.encode(&data))
        }
    } else {
        Ok(String::new())
    }
}

/// Non-destructive read of the ring buffer — returns all buffered output as base64
/// without clearing. Used on terminal mount so screen content survives webview refresh.
/// Encodes directly from VecDeque slices to avoid allocating a full copy of the buffer.
#[tauri::command]
pub async fn snapshot_pty_buffer(state: State<'_, AppState>, id: String) -> Result<String, String> {
    let buffers = state.pty_buffers.lock().unwrap();
    if let Some(buffer) = buffers.get(&id) {
        let buf = buffer.lock().unwrap();
        if buf.is_empty() {
            Ok(String::new())
        } else {
            let (a, b) = buf.slices();
            // Encode directly from the two contiguous slices — no intermediate Vec.
            let mut encoder =
                base64::write::EncoderStringWriter::new(&base64::engine::general_purpose::STANDARD);
            std::io::Write::write_all(&mut encoder, a).unwrap();
            std::io::Write::write_all(&mut encoder, b).unwrap();
            Ok(encoder.into_inner())
        }
    } else {
        Ok(String::new())
    }
}

/// Check if a PTY session exists (session in HashMap = alive)
#[tauri::command]
pub async fn check_pty_session(state: State<'_, AppState>, id: String) -> Result<bool, String> {
    let sessions = state.pty_sessions.lock().unwrap();
    Ok(sessions.contains_key(&id))
}

#[tauri::command]
pub async fn write_pty(state: State<'_, AppState>, id: String, data: String) -> Result<(), String> {
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
pub async fn close_pty(state: State<'_, AppState>, id: String) -> Result<(), String> {
    // Remove session and buffer from maps first
    let session = {
        let mut sessions = state.pty_sessions.lock().unwrap();
        sessions.remove(&id)
    };
    {
        let mut buffers = state.pty_buffers.lock().unwrap();
        buffers.remove(&id);
    }
    // Kill outside the lock to avoid deadlocking other PTY operations
    if let Some(mut session) = session {
        platform::kill_pty_session(&mut session);
    }
    Ok(())
}
