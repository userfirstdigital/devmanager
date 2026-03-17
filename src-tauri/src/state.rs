use crate::models::config::AppConfig;
use crate::services::platform::RuntimePlatformState;
use portable_pty::MasterPty;
use std::collections::HashMap;
use std::collections::VecDeque;
use std::io::Write;
use std::sync::{Arc, Mutex, RwLock};

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

/// Ring buffer for PTY output, stored in Rust to avoid JS memory pressure.
/// Keeps the most recent `max_bytes` of output per session.
pub struct PtyOutputBuffer {
    data: VecDeque<u8>,
    max_bytes: usize,
}

impl PtyOutputBuffer {
    pub fn new(max_bytes: usize) -> Self {
        Self {
            // Pre-allocate full capacity upfront to avoid repeated VecDeque doublings
            // (64K→128K→…) which cause allocation failures on Windows debug builds.
            data: VecDeque::with_capacity(max_bytes),
            max_bytes,
        }
    }

    /// Append bytes, evicting old data if the cap is exceeded.
    pub fn push(&mut self, bytes: &[u8]) {
        self.data.extend(bytes.iter());
        if self.data.len() > self.max_bytes {
            let excess = self.data.len() - self.max_bytes;
            self.data.drain(..excess);
        }
    }

    /// Return all buffered data and clear the buffer.
    pub fn drain(&mut self) -> Vec<u8> {
        self.data.drain(..).collect()
    }

    /// Return the two contiguous slices of the ring buffer for zero-copy access.
    /// VecDeque stores data in at most two slices; callers can encode directly
    /// without allocating an intermediate Vec.
    pub fn slices(&self) -> (&[u8], &[u8]) {
        self.data.as_slices()
    }

    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }
}

/// Entry for the unified resource monitoring loop.
pub struct MonitorEntry {
    pub command_id: String,
    pub pid: u32,
}

pub struct AppState {
    /// RwLock: config is read far more than written
    pub config: RwLock<Option<AppConfig>>,
    pub runtime_platform: RuntimePlatformState,
    pub processes: Mutex<HashMap<String, ProcessInfo>>,
    /// Unified monitoring: one loop reads this map each tick
    pub monitored_processes: Mutex<HashMap<String, MonitorEntry>>,
    pub pty_sessions: Mutex<HashMap<String, PtySession>>,
    pub pty_buffers: Mutex<HashMap<String, Arc<Mutex<PtyOutputBuffer>>>>,
    /// File watcher for .git/HEAD changes
    pub git_watcher: Mutex<Option<notify::RecommendedWatcher>>,
}
