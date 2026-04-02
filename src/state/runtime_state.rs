use crate::terminal::session::TerminalBackend;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

const AI_ACTIVITY_BURST_WINDOW: Duration = Duration::from_secs(1);
const AI_ACTIVITY_MIN_BURST_EVENTS: u8 = 3;
const AI_IDLE_GRACE_PERIOD: Duration = Duration::from_secs(3);
const AI_BACKGROUND_READY_THRESHOLD: Duration = Duration::from_secs(30);
const AI_FOREGROUND_READY_THRESHOLD: Duration = Duration::from_secs(60);
const AI_ACTIVITY_SUPPRESSION_AFTER_RESIZE: Duration = Duration::from_secs(2);
const AI_NOTIFICATION_CONFIRM_DELAY: Duration = Duration::from_secs(2);
const USER_EXIT_GRACE_PERIOD: Duration = Duration::from_secs(3);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum AiActivity {
    #[default]
    Idle,
    Thinking,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum ShellIntegrationKind {
    #[default]
    None,
    Ghostty,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PromptMarkKind {
    PromptStart,
    PromptContinuation,
    InputReady,
    CommandStart,
    CommandFinished,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromptMark {
    pub buffer_line: usize,
    pub kind: PromptMarkKind,
    pub exit_status: Option<i32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
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

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SessionExitState {
    pub code: Option<u32>,
    pub signal: Option<String>,
    pub closed_by_user: bool,
    pub summary: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ResourceSnapshot {
    pub cpu_percent: f32,
    pub memory_bytes: u64,
    pub process_count: u32,
    pub process_ids: Vec<u32>,
    #[serde(skip, default)]
    pub last_sample_at: Option<Instant>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AiLaunchSpec {
    pub tab_id: String,
    pub project_id: String,
    pub tool: SessionKind,
    pub cwd: PathBuf,
    pub shell_program: String,
    pub shell_args: Vec<String>,
    pub startup_command: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SshLaunchSpec {
    pub tab_id: String,
    pub ssh_connection_id: String,
    pub project_id: String,
    pub cwd: PathBuf,
    pub program: String,
    pub args: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionMetrics {
    pub total_pty_bytes: u64,
    pub pty_bytes_per_second: u64,
    pub total_frames: u64,
    pub frames_per_second: u64,
    pub last_render_micros: u64,
    pub resize_events: u64,
    pub scroll_events: u64,
    #[serde(skip, default = "instant_now")]
    pub last_bytes_sample_at: Instant,
    pub last_bytes_total: u64,
    #[serde(skip, default = "instant_now")]
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionRuntimeState {
    pub session_id: String,
    pub pid: Option<u32>,
    pub status: SessionStatus,
    pub session_kind: SessionKind,
    pub interactive_shell: bool,
    pub project_id: Option<String>,
    pub command_id: Option<String>,
    pub tab_id: Option<String>,
    #[serde(skip, default)]
    pub started_at: Option<Instant>,
    pub exit_code: Option<u32>,
    pub auto_restart: bool,
    pub title: Option<String>,
    pub cwd: PathBuf,
    pub shell_program: String,
    pub bell_count: u64,
    #[serde(skip, default)]
    pub last_bell_at: Option<Instant>,
    pub dirty_generation: u64,
    pub frame_generation: u64,
    pub display_offset: usize,
    pub dimensions: SessionDimensions,
    pub exit: Option<SessionExitState>,
    pub backend: TerminalBackend,
    pub metrics: SessionMetrics,
    pub resources: ResourceSnapshot,
    pub awaiting_external_editor: bool,
    pub shell_integration: ShellIntegrationKind,
    pub prompt_marks: Vec<PromptMark>,
    pub reported_cwd: Option<PathBuf>,
    pub at_prompt: bool,
    pub server_launch: Option<ServerLaunchSpec>,
    pub ai_launch: Option<AiLaunchSpec>,
    pub ssh_launch: Option<SshLaunchSpec>,
    pub ai_activity: Option<AiActivity>,
    #[serde(skip, default)]
    pub last_output_at: Option<Instant>,
    #[serde(skip, default)]
    pub thinking_since: Option<Instant>,
    pub unseen_ready: bool,
    #[serde(skip, default)]
    last_user_interrupt_at: Option<Instant>,
    #[serde(skip, default)]
    last_user_stop_request_at: Option<Instant>,
    #[serde(skip, default)]
    suppress_activity_until: Option<Instant>,
    #[serde(skip, default)]
    last_output_event_at: Option<Instant>,
    output_burst_count: u8,
    #[serde(skip, default)]
    pending_notification: Option<(Instant, AiIdleTransition)>,
}

impl SessionRuntimeState {
    pub fn has_live_process(&self) -> bool {
        self.status.is_live() && self.pid.is_some() && !self.interactive_shell
    }

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
            interactive_shell: false,
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
            awaiting_external_editor: false,
            shell_integration: ShellIntegrationKind::None,
            prompt_marks: Vec::new(),
            reported_cwd: None,
            at_prompt: false,
            server_launch: None,
            ai_launch: None,
            ssh_launch: None,
            ai_activity: None,
            last_output_at: None,
            thinking_since: None,
            unseen_ready: false,
            last_user_interrupt_at: None,
            last_user_stop_request_at: None,
            suppress_activity_until: None,
            last_output_event_at: None,
            output_burst_count: 0,
            pending_notification: None,
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
        self.note_output_activity_at(Instant::now());
    }

    fn note_output_activity_at(&mut self, now: Instant) {
        if !self.session_kind.is_ai() {
            return;
        }

        if self.ai_activity_is_suppressed(now) {
            return;
        }

        self.pending_notification = None;
        self.last_output_at = Some(now);
        self.output_burst_count = match self.last_output_event_at {
            Some(previous) if now.duration_since(previous) <= AI_ACTIVITY_BURST_WINDOW => {
                self.output_burst_count.saturating_add(1)
            }
            _ => 1,
        };
        self.last_output_event_at = Some(now);

        if self.output_burst_count >= AI_ACTIVITY_MIN_BURST_EVENTS
            && self.ai_activity != Some(AiActivity::Thinking)
        {
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
        if self.session_kind.is_ai() {
            self.suppress_ai_activity_for(AI_ACTIVITY_SUPPRESSION_AFTER_RESIZE);
        }
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
        self.interactive_shell = false;
        self.pid = None;
        self.resources = ResourceSnapshot::default();
        self.awaiting_external_editor = false;
        self.at_prompt = false;
        if self.session_kind.is_ai() {
            self.ai_activity = Some(AiActivity::Idle);
            self.thinking_since = None;
            self.suppress_activity_until = None;
            self.last_output_event_at = None;
            self.output_burst_count = 0;
            self.pending_notification = None;
        }
        self.mark_dirty();
    }

    pub fn note_start(&mut self, pid: Option<u32>) {
        let now = Instant::now();
        self.pid = pid;
        self.status = SessionStatus::Running;
        self.interactive_shell = false;
        self.started_at = Some(now);
        self.exit = None;
        self.exit_code = None;
        self.resources = ResourceSnapshot::default();
        self.awaiting_external_editor = false;
        self.prompt_marks.clear();
        self.reported_cwd = None;
        self.at_prompt = false;
        self.last_output_at = None;
        self.last_output_event_at = None;
        self.output_burst_count = 0;
        self.last_user_interrupt_at = None;
        self.last_user_stop_request_at = None;
        if self.session_kind.is_ai() {
            self.ai_activity = Some(AiActivity::Idle);
            self.thinking_since = None;
            self.unseen_ready = false;
            self.pending_notification = None;
            self.suppress_ai_activity_until(now + AI_ACTIVITY_SUPPRESSION_AFTER_RESIZE);
        } else {
            self.suppress_activity_until = None;
        }
        self.mark_dirty();
    }

    pub fn note_resource_sample(&mut self, snapshot: ResourceSnapshot) {
        self.resources = snapshot;
        self.mark_dirty();
    }

    pub fn note_external_editor_wait(&mut self, waiting: bool) {
        if self.awaiting_external_editor == waiting {
            return;
        }
        self.awaiting_external_editor = waiting;
        self.mark_dirty();
    }

    pub fn note_user_interrupt(&mut self) {
        self.last_user_interrupt_at = Some(Instant::now());
        self.mark_dirty();
    }

    pub fn note_user_stop_request(&mut self) {
        self.last_user_stop_request_at = Some(Instant::now());
        self.interactive_shell = false;
        self.mark_dirty();
    }

    pub fn has_recent_user_interrupt(&self, now: Instant) -> bool {
        self.last_user_interrupt_at.is_some_and(|interrupted_at| {
            now.duration_since(interrupted_at) <= USER_EXIT_GRACE_PERIOD
        })
    }

    pub fn has_recent_user_stop_request(&self, now: Instant) -> bool {
        self.last_user_stop_request_at
            .is_some_and(|requested_at| now.duration_since(requested_at) <= USER_EXIT_GRACE_PERIOD)
    }

    pub fn clear_user_exit_requests(&mut self) {
        self.last_user_interrupt_at = None;
        self.last_user_stop_request_at = None;
    }

    pub fn activate_interactive_shell(
        &mut self,
        shell_program: String,
        summary: impl Into<String>,
    ) {
        self.status = SessionStatus::Stopped;
        self.interactive_shell = true;
        self.shell_program = shell_program;
        self.exit_code = None;
        self.awaiting_external_editor = false;
        self.at_prompt = true;
        self.exit = Some(SessionExitState {
            code: None,
            signal: None,
            closed_by_user: true,
            summary: summary.into(),
        });
        self.resources = ResourceSnapshot::default();
        self.clear_user_exit_requests();
        self.mark_dirty();
    }

    pub fn configure_server(&mut self, launch: ServerLaunchSpec) {
        self.session_kind = SessionKind::Server;
        self.interactive_shell = false;
        self.awaiting_external_editor = false;
        self.prompt_marks.clear();
        self.reported_cwd = None;
        self.at_prompt = false;
        self.project_id = Some(launch.project_id.clone());
        self.command_id = Some(launch.command_id.clone());
        self.auto_restart = launch.auto_restart;
        self.server_launch = Some(launch);
        self.ai_launch = None;
        self.ssh_launch = None;
        self.ai_activity = None;
        self.tab_id = None;
        self.unseen_ready = false;
        self.clear_user_exit_requests();
        self.suppress_activity_until = None;
        self.pending_notification = None;
        self.mark_dirty();
    }

    pub fn configure_ai(&mut self, launch: AiLaunchSpec) {
        self.session_kind = launch.tool;
        self.interactive_shell = false;
        self.awaiting_external_editor = false;
        self.prompt_marks.clear();
        self.reported_cwd = None;
        self.at_prompt = false;
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
        self.clear_user_exit_requests();
        self.suppress_activity_until = None;
        self.last_output_event_at = None;
        self.output_burst_count = 0;
        self.pending_notification = None;
        self.mark_dirty();
    }

    pub fn configure_ssh(&mut self, launch: SshLaunchSpec) {
        self.session_kind = SessionKind::Ssh;
        self.interactive_shell = false;
        self.awaiting_external_editor = false;
        self.prompt_marks.clear();
        self.reported_cwd = None;
        self.at_prompt = false;
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
        self.clear_user_exit_requests();
        self.suppress_activity_until = None;
        self.last_output_event_at = None;
        self.output_burst_count = 0;
        self.pending_notification = None;
        self.mark_dirty();
    }

    pub fn clear_unseen_ready(&mut self) {
        if self.unseen_ready {
            self.unseen_ready = false;
            self.mark_dirty();
        }
    }

    pub fn note_shell_integration_detected(&mut self, kind: ShellIntegrationKind) {
        if self.shell_integration == kind {
            return;
        }
        self.shell_integration = kind;
        self.mark_dirty();
    }

    pub fn note_shell_reported_cwd(&mut self, cwd: PathBuf) {
        let changed = self.reported_cwd.as_ref() != Some(&cwd) || self.cwd != cwd;
        if !changed {
            return;
        }
        self.reported_cwd = Some(cwd.clone());
        self.cwd = cwd;
        self.mark_dirty();
    }

    pub fn note_prompt_mark(
        &mut self,
        buffer_line: usize,
        kind: PromptMarkKind,
        exit_status: Option<i32>,
    ) {
        const MAX_PROMPT_MARKS: usize = 256;

        self.at_prompt = matches!(
            kind,
            PromptMarkKind::PromptStart
                | PromptMarkKind::PromptContinuation
                | PromptMarkKind::InputReady
                | PromptMarkKind::CommandFinished
        );
        self.prompt_marks.push(PromptMark {
            buffer_line,
            kind,
            exit_status,
        });
        if self.prompt_marks.len() > MAX_PROMPT_MARKS {
            let overflow = self.prompt_marks.len() - MAX_PROMPT_MARKS;
            self.prompt_marks.drain(0..overflow);
        }
        self.mark_dirty();
    }

    pub fn previous_prompt_line(&self, before_buffer_line: Option<usize>) -> Option<usize> {
        self.prompt_marks
            .iter()
            .rev()
            .filter(|mark| {
                matches!(
                    mark.kind,
                    PromptMarkKind::PromptStart
                        | PromptMarkKind::PromptContinuation
                        | PromptMarkKind::InputReady
                )
            })
            .find(|mark| {
                before_buffer_line
                    .map(|buffer_line| mark.buffer_line < buffer_line)
                    .unwrap_or(true)
            })
            .map(|mark| mark.buffer_line)
    }

    pub fn next_prompt_line(&self, after_buffer_line: Option<usize>) -> Option<usize> {
        self.prompt_marks
            .iter()
            .filter(|mark| {
                matches!(
                    mark.kind,
                    PromptMarkKind::PromptStart
                        | PromptMarkKind::PromptContinuation
                        | PromptMarkKind::InputReady
                )
            })
            .find(|mark| {
                after_buffer_line
                    .map(|buffer_line| mark.buffer_line > buffer_line)
                    .unwrap_or(true)
            })
            .map(|mark| mark.buffer_line)
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

        if now.duration_since(last_output_at) < AI_IDLE_GRACE_PERIOD {
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

        let transition = if is_background && thinking_duration >= AI_BACKGROUND_READY_THRESHOLD {
            self.unseen_ready = true;
            AiIdleTransition::BackgroundReady
        } else if !is_background && thinking_duration >= AI_FOREGROUND_READY_THRESHOLD {
            AiIdleTransition::ForegroundReady
        } else {
            AiIdleTransition::NoChange
        };

        self.mark_dirty();

        if transition != AiIdleTransition::NoChange {
            self.pending_notification = Some((now, transition));
        }

        AiIdleTransition::NoChange
    }

    pub fn check_pending_notification(&mut self, now: Instant) -> AiIdleTransition {
        let Some((deferred_at, transition)) = self.pending_notification else {
            return AiIdleTransition::NoChange;
        };

        if now.duration_since(deferred_at) < AI_NOTIFICATION_CONFIRM_DELAY {
            return AiIdleTransition::NoChange;
        }

        self.pending_notification = None;

        if self.ai_activity == Some(AiActivity::Idle) {
            transition
        } else {
            AiIdleTransition::NoChange
        }
    }

    fn ai_activity_is_suppressed(&mut self, now: Instant) -> bool {
        match self.suppress_activity_until {
            Some(until) if now < until => true,
            Some(_) => {
                self.suppress_activity_until = None;
                false
            }
            None => false,
        }
    }

    fn suppress_ai_activity_for(&mut self, duration: Duration) {
        self.suppress_ai_activity_until(Instant::now() + duration);
    }

    fn suppress_ai_activity_until(&mut self, until: Instant) {
        if self.session_kind.is_ai() {
            self.suppress_activity_until = Some(until);
        } else {
            self.suppress_activity_until = None;
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AiIdleTransition {
    NoChange,
    BackgroundReady,
    ForegroundReady,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
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

fn instant_now() -> Instant {
    Instant::now()
}

pub use SessionRuntimeState as ProcessState;
pub use SessionStatus as ProcessStatus;

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn test_ai_session() -> SessionRuntimeState {
        let mut session = SessionRuntimeState::new(
            "session-1",
            PathBuf::from("."),
            SessionDimensions::default(),
            TerminalBackend::PortablePtyFeedingAlacritty,
        );
        session.configure_ai(AiLaunchSpec {
            tab_id: "tab-1".to_string(),
            project_id: "project-1".to_string(),
            tool: SessionKind::Claude,
            cwd: PathBuf::from("."),
            shell_program: "bash".to_string(),
            shell_args: Vec::new(),
            startup_command: "claude".to_string(),
        });
        session.status = SessionStatus::Running;
        session
    }

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

    #[test]
    fn interactive_shell_mode_clears_when_session_restarts_or_exits() {
        let mut session = SessionRuntimeState::new(
            "session-1",
            PathBuf::from("."),
            SessionDimensions::default(),
            TerminalBackend::PortablePtyFeedingAlacritty,
        );

        session.note_user_interrupt();
        assert!(session.has_recent_user_interrupt(Instant::now()));

        session.activate_interactive_shell("powershell.exe".to_string(), "prompt ready");
        assert_eq!(session.status, SessionStatus::Stopped);
        assert!(session.interactive_shell);
        assert!(!session.has_recent_user_interrupt(Instant::now()));

        session.note_start(Some(42));
        assert!(session.status.is_live());
        assert!(!session.interactive_shell);

        session.activate_interactive_shell("powershell.exe".to_string(), "prompt ready");
        session.note_exit(
            SessionExitState {
                code: Some(0),
                signal: None,
                closed_by_user: true,
                summary: "closed".to_string(),
            },
            SessionStatus::Exited,
        );
        assert!(!session.interactive_shell);
    }

    #[test]
    fn resize_suppression_blocks_ai_activity_until_window_expires() {
        let mut session = test_ai_session();
        let base = Instant::now();

        session.suppress_ai_activity_until(base + AI_ACTIVITY_SUPPRESSION_AFTER_RESIZE);
        session.note_output_activity_at(base + Duration::from_millis(100));
        session.note_output_activity_at(base + Duration::from_millis(200));
        session.note_output_activity_at(base + Duration::from_millis(300));

        assert_eq!(session.ai_activity, Some(AiActivity::Idle));
        assert!(session.last_output_at.is_none());

        let resume_at = base + AI_ACTIVITY_SUPPRESSION_AFTER_RESIZE;
        session.note_output_activity_at(resume_at + Duration::from_millis(10));
        session.note_output_activity_at(resume_at + Duration::from_millis(20));
        session.note_output_activity_at(resume_at + Duration::from_millis(30));

        assert_eq!(session.ai_activity, Some(AiActivity::Thinking));
        assert!(session.last_output_at.is_some());
    }

    #[test]
    fn background_ready_transition_sets_badge_after_long_thinking() {
        let mut session = test_ai_session();
        let base = Instant::now();

        session.note_output_activity_at(base + Duration::from_millis(100));
        session.note_output_activity_at(base + Duration::from_millis(200));
        session.note_output_activity_at(base + Duration::from_millis(300));

        let idle_at = base + AI_BACKGROUND_READY_THRESHOLD + AI_IDLE_GRACE_PERIOD;
        let transition = session.reconcile_ai_idle(Some("different-session"), idle_at);

        assert_eq!(transition, AiIdleTransition::NoChange);
        assert_eq!(session.ai_activity, Some(AiActivity::Idle));
        assert!(session.unseen_ready);
        assert!(session.pending_notification.is_some());

        let confirmed = session.check_pending_notification(idle_at + AI_NOTIFICATION_CONFIRM_DELAY);
        assert_eq!(confirmed, AiIdleTransition::BackgroundReady);
    }

    #[test]
    fn foreground_ready_transition_plays_sound_without_badge() {
        let mut session = test_ai_session();
        let base = Instant::now();
        let session_id = session.session_id.clone();

        session.note_output_activity_at(base + Duration::from_millis(100));
        session.note_output_activity_at(base + Duration::from_millis(200));
        session.note_output_activity_at(base + Duration::from_millis(300));

        let idle_at = base + AI_FOREGROUND_READY_THRESHOLD + AI_IDLE_GRACE_PERIOD;
        let transition = session.reconcile_ai_idle(Some(session_id.as_str()), idle_at);

        assert_eq!(transition, AiIdleTransition::NoChange);
        assert_eq!(session.ai_activity, Some(AiActivity::Idle));
        assert!(!session.unseen_ready);
        assert!(session.pending_notification.is_some());

        let confirmed = session.check_pending_notification(idle_at + AI_NOTIFICATION_CONFIRM_DELAY);
        assert_eq!(confirmed, AiIdleTransition::ForegroundReady);
    }

    #[test]
    fn pending_notification_cancelled_when_ai_resumes() {
        let mut session = test_ai_session();
        let base = Instant::now();

        session.note_output_activity_at(base + Duration::from_millis(100));
        session.note_output_activity_at(base + Duration::from_millis(200));
        session.note_output_activity_at(base + Duration::from_millis(300));

        let idle_at = base + AI_BACKGROUND_READY_THRESHOLD + AI_IDLE_GRACE_PERIOD;
        session.reconcile_ai_idle(Some("different-session"), idle_at);
        assert!(session.pending_notification.is_some());

        // AI resumes output — pending notification should be cancelled
        let resume_at = idle_at + Duration::from_secs(1);
        session.note_output_activity_at(resume_at);
        session.note_output_activity_at(resume_at + Duration::from_millis(10));
        session.note_output_activity_at(resume_at + Duration::from_millis(20));
        assert!(session.pending_notification.is_none());

        let confirmed = session.check_pending_notification(idle_at + AI_NOTIFICATION_CONFIRM_DELAY);
        assert_eq!(confirmed, AiIdleTransition::NoChange);
    }

    #[test]
    fn pending_notification_not_returned_before_delay() {
        let mut session = test_ai_session();
        let base = Instant::now();

        session.note_output_activity_at(base + Duration::from_millis(100));
        session.note_output_activity_at(base + Duration::from_millis(200));
        session.note_output_activity_at(base + Duration::from_millis(300));

        let idle_at = base + AI_BACKGROUND_READY_THRESHOLD + AI_IDLE_GRACE_PERIOD;
        session.reconcile_ai_idle(Some("different-session"), idle_at);

        // Check too early — should not fire
        let too_early = idle_at + Duration::from_secs(1);
        let confirmed = session.check_pending_notification(too_early);
        assert_eq!(confirmed, AiIdleTransition::NoChange);
        assert!(session.pending_notification.is_some());
    }

    #[test]
    fn prompt_marks_drive_navigation_and_prompt_state() {
        let mut session = SessionRuntimeState::new(
            "session-1",
            PathBuf::from("."),
            SessionDimensions::default(),
            TerminalBackend::PortablePtyFeedingAlacritty,
        );

        session.note_prompt_mark(4, PromptMarkKind::PromptStart, None);
        session.note_prompt_mark(5, PromptMarkKind::CommandStart, None);
        assert!(!session.at_prompt);

        session.note_prompt_mark(8, PromptMarkKind::InputReady, None);
        session.note_prompt_mark(12, PromptMarkKind::CommandFinished, Some(0));

        assert!(session.at_prompt);
        assert_eq!(session.previous_prompt_line(Some(8)), Some(4));
        assert_eq!(session.previous_prompt_line(Some(13)), Some(8));
        assert_eq!(session.next_prompt_line(Some(4)), Some(8));
        assert_eq!(session.next_prompt_line(Some(8)), None);
    }
}
