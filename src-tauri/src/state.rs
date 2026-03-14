use std::collections::HashMap;
use std::io::Write;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use portable_pty::MasterPty;
use crate::models::config::AppConfig;

#[allow(dead_code)]
pub struct ProcessInfo {
    pub pid: u32,
    pub command_id: String,
    pub project_id: String,
}

#[allow(dead_code)]
pub struct PtySession {
    pub writer: Arc<Mutex<Box<dyn Write + Send>>>,
    pub master: Box<dyn MasterPty + Send>,
    pub child: Box<dyn portable_pty::Child + Send>,
    pub session_id: String,
}

pub struct AppState {
    pub config: Mutex<Option<AppConfig>>,
    pub processes: Mutex<HashMap<String, ProcessInfo>>,
    pub resource_monitors: Mutex<HashMap<String, Arc<AtomicBool>>>,
    pub pty_sessions: Mutex<HashMap<String, PtySession>>,
}
