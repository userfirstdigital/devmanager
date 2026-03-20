use crate::terminal::session::TerminalBackend;
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SessionStatus {
    #[default]
    Stopped,
    Starting,
    Running,
    Stopping,
    Crashed,
    Exited,
    Failed,
}

impl SessionStatus {
    pub fn is_live(self) -> bool {
        matches!(self, Self::Starting | Self::Running | Self::Stopping)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SessionKind {
    #[default]
    Shell,
    Server,
    Claude,
    Codex,
    Ssh,
}

impl SessionKind {
    pub fn is_ai(self) -> bool {
        matches!(self, Self::Claude | Self::Codex)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AiActivity {
    #[default]
    Idle,
    Thinking,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionDimensions {
    pub cols: u16,
    pub rows: u16,
    pub cell_width: u16,
    pub cell_height: u16,
}

impl Default for SessionDimensions {
    fn default() -> Self {
        Self {
            cols: 100,
            rows: 30,
            cell_width: 8,
            cell_height: 18,
        }
    }
}

impl SessionDimensions {
    pub fn from_available_space(
        available_width: f32,
        available_height: f32,
        cell_width: f32,
        cell_height: f32,
    ) -> Self {
        let cols = (available_width / cell_width).floor().max(2.0) as u16;
        let rows = (available_height / cell_height).floor().max(1.0) as u16;

        Self {
            cols,
            rows,
            cell_width: cell_width.round().max(1.0) as u16,
            cell_height: cell_height.round().max(1.0) as u16,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct SessionExitState {
    pub code: Option<u32>,
    pub signal: Option<String>,
    pub closed_by_user: bool,
    pub summary: String,
}

#[derive(Debug, Clone, Default)]
pub struct ResourceSnapshot {
    pub cpu_percent: f32,
    pub memory_bytes: u64,
    pub process_count: u32,
    pub process_ids: Vec<u32>,
    pub last_sample_at: Option<Instant>,
}

#[derive(Debug, Clone)]
pub struct ServerLaunchSpec {
    pub command_id: String,
    pub project_id: String,
    pub cwd: PathBuf,
    pub program: String,
    pub args: Vec<String>,
    pub env: HashMap<String, String>,
    pub auto_restart: bool,
    pub log_file_path: Option<PathBuf>,
}

#[derive(Debug, Clone)]
pub struct AiLaunchSpec {
    pub tab_id: String,
    pub project_id: String,
    pub tool: SessionKind,
    pub cwd: PathBuf,
    pub shell_program: String,
    pub shell_args: Vec<String>,
    pub startup_command: String,
}

#[derive(Debug, Clone)]
pub struct SshLaunchSpec {
    pub tab_id: String,
    pub ssh_connection_id: String,
    pub project_id: String,
    pub cwd: PathBuf,
    pub program: String,
    pub args: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct SessionMetrics {
    pub total_pty_bytes: u64,
    pub pty_bytes_per_second: u64,
    pub total_frames: u64,
    pub frames_per_second: u64,
    pub last_render_micros: u64,
    pub resize_events: u64,
    pub scroll_events: u64,
    pub last_bytes_sample_at: Instant,
    pub last_bytes_total: u64,
    pub last_frames_sample_at: Instant,
    pub last_frames_total: u64,
}

impl Default for SessionMetrics {
    fn default() -> Self {
        let now = Instant::now();
        Self {
            total_pty_bytes: 0,
            pty_bytes_per_second: 0,
            total_frames: 0,
            frames_per_second: 0,
            last_render_micros: 0,
            resize_events: 0,
            scroll_events: 0,
            last_bytes_sample_at: now,
            last_bytes_total: 0,
            last_frames_sample_at: now,
            last_frames_total: 0,
        }
    }
}

#[derive(Debug, Clone)]
pub struct SessionRuntimeState {
    pub session_id: String,
    pub pid: Option<u32>,
    pub status: SessionStatus,
    pub session_kind: SessionKind,
    pub project_id: Option<String>,
    pub command_id: Option<String>,
    pub tab_id: Option<String>,
    pub started_at: Option<Instant>,
    pub exit_code: Option<u32>,
    pub auto_restart: bool,
    pub title: Option<String>,
    pub cwd: PathBuf,
    pub shell_program: String,
    pub bell_count: u64,
    pub last_bell_at: Option<Instant>,
    pub dirty_generation: u64,
    pub frame_generation: u64,
    pub display_offset: usize,
    pub dimensions: SessionDimensions,
    pub exit: Option<SessionExitState>,
    pub backend: TerminalBackend,
    pub metrics: SessionMetrics,
    pub resources: ResourceSnapshot,
    pub server_launch: Option<ServerLaunchSpec>,
    pub ai_launch: Option<AiLaunchSpec>,
    pub ssh_launch: Option<SshLaunchSpec>,
    pub ai_activity: Option<AiActivity>,
    pub last_output_at: Option<Instant>,
    pub thinking_since: Option<Instant>,
    pub unseen_ready: bool,
    last_output_event_at: Option<Instant>,
    output_burst_count: u8,
}

impl SessionRuntimeState {
    pub fn new(
        session_id: impl Into<String>,
        cwd: PathBuf,
        dimensions: SessionDimensions,
        backend: TerminalBackend,
    ) -> Self {
        Self {
            session_id: session_id.into(),
            pid: None,
            status: SessionStatus::Starting,
            session_kind: SessionKind::Shell,
            project_id: None,
            command_id: None,
            tab_id: None,
            started_at: None,
            exit_code: None,
            auto_restart: false,
            title: None,
            cwd,
            shell_program: backend.label().to_string(),
            bell_count: 0,
            last_bell_at: None,
            dirty_generation: 0,
            frame_generation: 0,
            display_offset: 0,
            dimensions,
            exit: None,
            backend,
            metrics: SessionMetrics::default(),
            resources: ResourceSnapshot::default(),
            server_launch: None,
            ai_launch: None,
            ssh_launch: None,
            ai_activity: None,
            last_output_at: None,
            thinking_since: None,
            unseen_ready: false,
            last_output_event_at: None,
            output_burst_count: 0,
        }
    }

    pub fn mark_dirty(&mut self) {
        self.dirty_generation = self.dirty_generation.saturating_add(1);
    }

    pub fn record_pty_bytes(&mut self, count: usize) {
        self.metrics.total_pty_bytes = self.metrics.total_pty_bytes.saturating_add(count as u64);
        let now = Instant::now();
        if now
            .duration_since(self.metrics.last_bytes_sample_at)
            .as_secs_f32()
            >= 1.0
        {
            self.metrics.pty_bytes_per_second = self
                .metrics
                .total_pty_bytes
                .saturating_sub(self.metrics.last_bytes_total);
            self.metrics.last_bytes_total = self.metrics.total_pty_bytes;
            self.metrics.last_bytes_sample_at = now;
        }
        self.mark_dirty();
    }

    pub fn note_output_activity(&mut self) {
        if !self.session_kind.is_ai() {
            return;
        }

        let now = Instant::now();
        self.last_output_at = Some(now);
        self.output_burst_count = match self.last_output_event_at {
            Some(previous) if now.duration_since(previous) <= Duration::from_secs(1) => {
                self.output_burst_count.saturating_add(1)
            }
            _ => 1,
        };
        self.last_output_event_at = Some(now);

        if self.output_burst_count >= 3 && self.ai_activity != Some(AiActivity::Thinking) {
            self.ai_activity = Some(AiActivity::Thinking);
            self.thinking_since = Some(now);
            self.mark_dirty();
        }
    }

    pub fn record_frame(&mut self, render_micros: u64) {
        self.metrics.total_frames = self.metrics.total_frames.saturating_add(1);
        self.metrics.last_render_micros = render_micros;
        self.frame_generation = self.frame_generation.saturating_add(1);

        let now = Instant::now();
        if now
            .duration_since(self.metrics.last_frames_sample_at)
            .as_secs_f32()
            >= 1.0
        {
            self.metrics.frames_per_second = self
                .metrics
                .total_frames
                .saturating_sub(self.metrics.last_frames_total);
            self.metrics.last_frames_total = self.metrics.total_frames;
            self.metrics.last_frames_sample_at = now;
        }
    }

    pub fn note_resize(&mut self, dimensions: SessionDimensions) {
        self.dimensions = dimensions;
        self.metrics.resize_events = self.metrics.resize_events.saturating_add(1);
        self.mark_dirty();
    }

    pub fn note_scroll(&mut self, display_offset: usize) {
        self.display_offset = display_offset;
        self.metrics.scroll_events = self.metrics.scroll_events.saturating_add(1);
        self.mark_dirty();
    }

    pub fn note_title(&mut self, title: Option<String>) {
        self.title = title;
        self.mark_dirty();
    }

    pub fn note_bell(&mut self) {
        self.bell_count = self.bell_count.saturating_add(1);
        self.last_bell_at = Some(Instant::now());
        self.mark_dirty();
    }

    pub fn note_exit(&mut self, exit: SessionExitState, status: SessionStatus) {
        self.exit_code = exit.code;
        self.exit = Some(exit);
        self.status = status;
        self.pid = None;
        self.resources = ResourceSnapshot::default();
        if self.session_kind.is_ai() {
            self.ai_activity = Some(AiActivity::Idle);
            self.thinking_since = None;
            self.last_output_event_at = None;
            self.output_burst_count = 0;
        }
        self.mark_dirty();
    }

    pub fn note_start(&mut self, pid: Option<u32>) {
        self.pid = pid;
        self.status = SessionStatus::Running;
        self.started_at = Some(Instant::now());
        self.exit = None;
        self.exit_code = None;
        self.resources = ResourceSnapshot::default();
        self.last_output_at = None;
        self.last_output_event_at = None;
        self.output_burst_count = 0;
        if self.session_kind.is_ai() {
            self.ai_activity = Some(AiActivity::Idle);
            self.thinking_since = None;
            self.unseen_ready = false;
        }
        self.mark_dirty();
    }

    pub fn note_resource_sample(&mut self, snapshot: ResourceSnapshot) {
        self.resources = snapshot;
        self.mark_dirty();
    }

    pub fn configure_server(&mut self, launch: ServerLaunchSpec) {
        self.session_kind = SessionKind::Server;
        self.project_id = Some(launch.project_id.clone());
        self.command_id = Some(launch.command_id.clone());
        self.auto_restart = launch.auto_restart;
        self.server_launch = Some(launch);
        self.ai_launch = None;
        self.ssh_launch = None;
        self.ai_activity = None;
        self.tab_id = None;
        self.unseen_ready = false;
        self.mark_dirty();
    }

    pub fn configure_ai(&mut self, launch: AiLaunchSpec) {
        self.session_kind = launch.tool;
        self.project_id = Some(launch.project_id.clone());
        self.tab_id = Some(launch.tab_id.clone());
        self.command_id = None;
        self.auto_restart = false;
        self.server_launch = None;
        self.ai_launch = Some(launch);
        self.ssh_launch = None;
        self.ai_activity = Some(AiActivity::Idle);
        self.last_output_at = None;
        self.thinking_since = None;
        self.unseen_ready = false;
        self.last_output_event_at = None;
        self.output_burst_count = 0;
        self.mark_dirty();
    }

    pub fn configure_ssh(&mut self, launch: SshLaunchSpec) {
        self.session_kind = SessionKind::Ssh;
        self.project_id = Some(launch.project_id.clone());
        self.tab_id = Some(launch.tab_id.clone());
        self.command_id = None;
        self.auto_restart = false;
        self.server_launch = None;
        self.ai_launch = None;
        self.ssh_launch = Some(launch);
        self.ai_activity = None;
        self.last_output_at = None;
        self.thinking_since = None;
        self.unseen_ready = false;
        self.last_output_event_at = None;
        self.output_burst_count = 0;
        self.mark_dirty();
    }

    pub fn clear_unseen_ready(&mut self) {
        if self.unseen_ready {
            self.unseen_ready = false;
            self.mark_dirty();
        }
    }

    pub fn reconcile_ai_idle(
        &mut self,
        active_session_id: Option<&str>,
        now: Instant,
    ) -> AiIdleTransition {
        if !self.session_kind.is_ai() || self.ai_activity != Some(AiActivity::Thinking) {
            return AiIdleTransition::NoChange;
        }

        let Some(last_output_at) = self.last_output_at else {
            return AiIdleTransition::NoChange;
        };

        if now.duration_since(last_output_at) < Duration::from_secs(3) {
            return AiIdleTransition::NoChange;
        }

        let thinking_duration = self
            .thinking_since
            .map(|thinking_since| now.duration_since(thinking_since))
            .unwrap_or_default();
        let is_background = active_session_id != Some(self.session_id.as_str());

        self.ai_activity = Some(AiActivity::Idle);
        self.thinking_since = None;
        self.last_output_event_at = None;
        self.output_burst_count = 0;

        if is_background && thinking_duration >= Duration::from_secs(30) {
            self.unseen_ready = true;
            self.mark_dirty();
            return AiIdleTransition::BackgroundReady;
        }

        self.mark_dirty();
        if !is_background && thinking_duration >= Duration::from_secs(60) {
            AiIdleTransition::ForegroundReady
        } else {
            AiIdleTransition::NoChange
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AiIdleTransition {
    NoChange,
    BackgroundReady,
    ForegroundReady,
}

#[derive(Debug, Clone, Default)]
pub struct RuntimeState {
    pub sessions: HashMap<String, SessionRuntimeState>,
    pub active_session_id: Option<String>,
    pub debug_enabled: bool,
}

impl RuntimeState {
    pub fn new(debug_enabled: bool) -> Self {
        Self {
            sessions: HashMap::new(),
            active_session_id: None,
            debug_enabled,
        }
    }
}

pub use SessionRuntimeState as ProcessState;
pub use SessionStatus as ProcessStatus;

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn note_start_and_exit_clear_stale_resource_metrics() {
        let mut session = SessionRuntimeState::new(
            "session-1",
            PathBuf::from("."),
            SessionDimensions::default(),
            TerminalBackend::PortablePtyFeedingAlacritty,
        );
        session.resources = ResourceSnapshot {
            cpu_percent: 42.0,
            memory_bytes: 12_345,
            process_count: 3,
            process_ids: vec![11, 22, 33],
            last_sample_at: Some(Instant::now()),
        };

        session.note_start(Some(44));
        assert_eq!(session.resources.memory_bytes, 0);
        assert_eq!(session.resources.process_count, 0);
        assert!(session.resources.process_ids.is_empty());
        assert!(session.resources.last_sample_at.is_none());

        session.resources = ResourceSnapshot {
            cpu_percent: 17.5,
            memory_bytes: 9_999,
            process_count: 2,
            process_ids: vec![44, 55],
            last_sample_at: Some(Instant::now()),
        };
        session.note_exit(
            SessionExitState {
                code: Some(0),
                signal: None,
                closed_by_user: true,
                summary: "closed".to_string(),
            },
            SessionStatus::Exited,
        );

        assert_eq!(session.resources.memory_bytes, 0);
        assert_eq!(session.resources.process_count, 0);
        assert!(session.resources.process_ids.is_empty());
        assert!(session.resources.last_sample_at.is_none());
    }
}
