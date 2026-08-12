//! Host-owned terminal service: one attached session/reader, one canonical grid.

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use crate::domain::id::{ClientId, ResourceId, TaskId, TerminalId};
use crate::state::{SessionDimensions, SessionRuntimeState, SessionStatus};
use crate::terminal::protocol::{
    AttachmentFence, ClientInputGrant, CloseReason, CoalesceReason, FocusEpoch, InputAck,
    InputEnvelope, InputId, InputRejectReason, ReplicaUpdate, ResizeFence, TeardownReport,
    TerminalDelta, TerminalDeltaOp, TerminalError, TerminalGeneration, TerminalInputContext,
    TerminalInputRequest, TerminalResourceFence, TerminalSequence, TerminalSessionId, TerminalSize,
    TerminalSnapshot, TerminalSpec, TerminalViewHandle, ViewKind, MAX_ACCEPTED_INPUT_HISTORY_BYTES,
    MAX_INPUT_BYTES, MAX_PENDING_OUTPUT_BYTES, MAX_PENDING_OUTPUT_CHUNKS, MAX_RETAINED_DELTAS,
    MAX_TRACKED_INPUT_IDS,
};
use crate::terminal::session::{
    TerminalBackend, TerminalLifecycleEvent, TerminalLifecycleSink, TerminalModeSnapshot,
    TerminalOutputSink, TerminalReplica, TerminalScreenSnapshot, TerminalSession,
    TerminalSessionView,
};

/// Host-facing runtime capability for one Job-owned (or typed mock) terminal.
pub trait AttachedTerminalRuntime: Send + Sync {
    fn write_bytes(&self, bytes: &[u8], expected: AttachmentFence) -> Result<(), TerminalError>;
    fn resize(&self, size: TerminalSize, expected: AttachmentFence) -> Result<(), TerminalError>;
    fn close_exact(
        &self,
        reason: CloseReason,
        expected: AttachmentFence,
    ) -> Result<(), TerminalError>;
    fn screen_snapshot(&self) -> TerminalScreenSnapshot;
    fn session_view(&self) -> Result<TerminalSessionView, TerminalError>;
    fn bound_history(&self, max_lines: usize);
    fn install_output_sink(&self, sink: TerminalOutputSink) -> Result<(), TerminalError>;
    fn install_lifecycle_sink(&self, sink: TerminalLifecycleSink) -> Result<(), TerminalError>;
    fn current_attachment_fence(&self) -> Result<AttachmentFence, TerminalError>;
    fn matches_attachment(&self, expected: AttachmentFence) -> Result<(), TerminalError> {
        let current = self.current_attachment_fence()?;
        if current.resource_id != expected.resource_id || current.generation != expected.generation
        {
            Err(TerminalError::StaleGeneration)
        } else {
            Ok(())
        }
    }
}

impl AttachedTerminalRuntime for TerminalSession {
    fn write_bytes(&self, bytes: &[u8], expected: AttachmentFence) -> Result<(), TerminalError> {
        self.matches_attachment(expected)?;
        TerminalSession::write_bytes(self, bytes).map_err(|_| TerminalError::RuntimeIo)
    }

    fn resize(&self, size: TerminalSize, expected: AttachmentFence) -> Result<(), TerminalError> {
        self.matches_attachment(expected)?;
        TerminalSession::resize(self, session_dimensions(size))
            .map_err(|_| TerminalError::RuntimeIo)
    }

    fn close_exact(
        &self,
        reason: CloseReason,
        expected: AttachmentFence,
    ) -> Result<(), TerminalError> {
        let closed_by_user = matches!(
            reason,
            CloseReason::ExplicitServiceClose | CloseReason::TaskClose
        );
        match self.close_exact_for_service(
            expected.resource_id,
            expected.generation.get(),
            closed_by_user,
        ) {
            Ok(()) => Ok(()),
            Err(error)
                if error.contains("fence is missing") || error.contains("generation changed") =>
            {
                Err(TerminalError::TeardownFenceMissing)
            }
            Err(_) => Err(TerminalError::TeardownFailed),
        }
    }

    fn screen_snapshot(&self) -> TerminalScreenSnapshot {
        self.snapshot()
    }

    fn session_view(&self) -> Result<TerminalSessionView, TerminalError> {
        TerminalSession::session_view(self).ok_or(TerminalError::CanonicalReaderPoisoned)
    }

    fn bound_history(&self, max_lines: usize) {
        self.bound_history_exact(max_lines);
    }

    fn install_output_sink(&self, sink: TerminalOutputSink) -> Result<(), TerminalError> {
        self.install_service_output_sink(sink)
            .map_err(|_| TerminalError::RuntimeIo)
    }

    fn install_lifecycle_sink(&self, sink: TerminalLifecycleSink) -> Result<(), TerminalError> {
        self.install_service_lifecycle_sink(sink)
            .map_err(|_| TerminalError::RuntimeIo)
    }

    fn current_attachment_fence(&self) -> Result<AttachmentFence, TerminalError> {
        let (resource_id, generation) = TerminalSession::current_attachment_fence(self)
            .map_err(|_| TerminalError::TeardownFenceMissing)?;
        Ok(AttachmentFence {
            resource_id,
            generation: TerminalGeneration::from_raw(generation)
                .map_err(|_| TerminalError::InvalidFence)?,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ClientReplicaCursor {
    generation: TerminalGeneration,
    sequence: TerminalSequence,
}

struct ClientViewState {
    grant: ClientInputGrant,
    cursor: Option<ClientReplicaCursor>,
    resize_sequence: Option<u64>,
}

enum ProjectionSource {
    /// Test-only in-memory grid. Never claims a live PTY/Job.
    Fixture(TerminalReplica),
    /// Production path: project the attached session's single reader/grid.
    Attached(Arc<dyn AttachedTerminalRuntime>),
}

#[derive(Default)]
struct PendingOutputQueue {
    /// Notification markers (byte sizes) awaiting drain. Empty when coalesced.
    chunks: VecDeque<usize>,
    bytes: usize,
    /// True when pressure dropped notifications; drain yields one coalesced update.
    coalesced: bool,
}

impl PendingOutputQueue {
    fn push(&mut self, chunk_len: usize) {
        if self.coalesced {
            return;
        }
        let next_bytes = self.bytes.saturating_add(chunk_len);
        let next_chunks = self.chunks.len().saturating_add(1);
        if next_chunks > MAX_PENDING_OUTPUT_CHUNKS || next_bytes > MAX_PENDING_OUTPUT_BYTES {
            self.chunks.clear();
            self.bytes = 0;
            self.coalesced = true;
            return;
        }
        self.chunks.push_back(chunk_len);
        self.bytes = next_bytes;
    }

    fn take(&mut self) -> PendingOutputDrain {
        if self.coalesced {
            *self = Self::default();
            return PendingOutputDrain::Coalesced;
        }
        let count = self.chunks.len();
        self.chunks.clear();
        self.bytes = 0;
        if count == 0 {
            PendingOutputDrain::Empty
        } else {
            PendingOutputDrain::Notifications(count)
        }
    }
}

enum PendingOutputDrain {
    Empty,
    Notifications(usize),
    Coalesced,
}

struct HostedTerminal {
    task_id: TaskId,
    agent_session_id: Option<crate::domain::AgentSessionId>,
    runtime_generation: Option<u64>,
    action_epoch: Option<u64>,
    session_id: TerminalSessionId,
    resource_id: ResourceId,
    generation: TerminalGeneration,
    sequence: TerminalSequence,
    focus_epoch: FocusEpoch,
    spec: TerminalSpec,
    projection: ProjectionSource,
    pending_output: Arc<Mutex<PendingOutputQueue>>,
    pending_lifecycle: Arc<Mutex<VecDeque<TerminalLifecycleEvent>>>,
    reader_count: u32,
    view_count: u32,
    closed: bool,
    /// Root/reader exit observed; distinct from explicit service/task close.
    exit_summary: Option<String>,
    truncated: bool,
    output_pressure_coalesced: bool,
    provider_session_id: Option<String>,
    accepted_input_sequence: u64,
    accepted_input_ids: HashMap<InputId, u64>,
    accepted_bytes: Vec<u8>,
    last_rows: Vec<String>,
    last_cursor: Option<crate::terminal::session::TerminalCursorSnapshot>,
    last_modes: TerminalModeSnapshot,
    last_title: Option<String>,
    deltas: VecDeque<TerminalDelta>,
    clients: HashMap<ClientId, ClientViewState>,
}

impl HostedTerminal {
    fn open_fixture(
        task_id: TaskId,
        spec: TerminalSpec,
        terminal_id: TerminalId,
    ) -> Result<Self, TerminalError> {
        let spec = spec.validated()?;
        let mut runtime = SessionRuntimeState::new(
            terminal_id.to_string(),
            PathBuf::from("."),
            session_dimensions(spec.size),
            TerminalBackend::PortablePtyFeedingAlacritty,
        );
        runtime.status = SessionStatus::Running;
        runtime.interactive_shell = true;
        runtime.title = spec.title.clone();
        runtime.pid = None;
        let replica = TerminalReplica::from_bootstrap(terminal_id.to_string(), runtime, &[]);
        replica.bound_history(spec.max_scrollback_rows);
        let screen = replica.screen_snapshot();
        let last_rows = rows_from_screen(&screen);
        let last_title = spec.title.clone();
        Ok(Self {
            task_id,
            agent_session_id: None,
            runtime_generation: None,
            action_epoch: None,
            session_id: spec.session_id,
            resource_id: ResourceId::new(),
            generation: TerminalGeneration::initial(),
            sequence: TerminalSequence::ZERO,
            focus_epoch: FocusEpoch::initial(),
            spec,
            projection: ProjectionSource::Fixture(replica),
            pending_output: Arc::new(Mutex::new(PendingOutputQueue::default())),
            pending_lifecycle: Arc::new(Mutex::new(VecDeque::new())),
            reader_count: 1,
            view_count: 0,
            closed: false,
            exit_summary: None,
            truncated: false,
            output_pressure_coalesced: false,
            provider_session_id: None,
            accepted_input_sequence: 0,
            accepted_input_ids: HashMap::new(),
            accepted_bytes: Vec::new(),
            last_cursor: screen.cursor,
            last_modes: screen.mode,
            last_title,
            last_rows,
            deltas: VecDeque::new(),
            clients: HashMap::new(),
        })
    }

    fn open_attached(
        task_id: TaskId,
        spec: TerminalSpec,
        terminal_id: TerminalId,
        runtime: Arc<dyn AttachedTerminalRuntime>,
    ) -> Result<Self, TerminalError> {
        let spec = spec.validated()?;
        let attachment = runtime.current_attachment_fence()?;
        runtime.bound_history(spec.max_scrollback_rows);
        let screen = runtime.screen_snapshot();
        let last_rows = rows_from_screen(&screen);
        let last_title = runtime
            .session_view()
            .ok()
            .and_then(|view| view.runtime.title)
            .or_else(|| spec.title.clone());
        let pending_output = Arc::new(Mutex::new(PendingOutputQueue::default()));
        let pending_for_sink = Arc::clone(&pending_output);
        runtime.install_output_sink(Arc::new(move |bytes, _mode| {
            if bytes.is_empty() {
                return;
            }
            if let Ok(mut pending) = pending_for_sink.lock() {
                pending.push(bytes.len());
            }
        }))?;
        let pending_lifecycle = Arc::new(Mutex::new(VecDeque::new()));
        let pending_lifecycle_for_sink = Arc::clone(&pending_lifecycle);
        runtime.install_lifecycle_sink(Arc::new(move |event| {
            if let Ok(mut pending) = pending_lifecycle_for_sink.lock() {
                pending.push_back(event);
            }
        }))?;
        Ok(Self {
            task_id,
            agent_session_id: None,
            runtime_generation: None,
            action_epoch: None,
            session_id: spec.session_id,
            resource_id: attachment.resource_id,
            generation: attachment.generation,
            sequence: TerminalSequence::ZERO,
            focus_epoch: FocusEpoch::initial(),
            spec,
            projection: ProjectionSource::Attached(runtime),
            pending_output,
            pending_lifecycle,
            reader_count: 1,
            view_count: 0,
            closed: false,
            exit_summary: None,
            truncated: false,
            output_pressure_coalesced: false,
            provider_session_id: None,
            accepted_input_sequence: 0,
            accepted_input_ids: HashMap::new(),
            accepted_bytes: Vec::new(),
            last_cursor: screen.cursor,
            last_modes: screen.mode,
            last_title,
            last_rows,
            deltas: VecDeque::new(),
            clients: HashMap::new(),
        })
    }

    fn is_attached(&self) -> bool {
        matches!(&self.projection, ProjectionSource::Attached(_))
    }

    fn ensure_open(&self) -> Result<(), TerminalError> {
        if self.closed {
            Err(TerminalError::Closed)
        } else {
            Ok(())
        }
    }

    fn fence(&self, terminal_id: TerminalId) -> TerminalResourceFence {
        TerminalResourceFence {
            terminal_id,
            session_id: self.session_id,
            resource_id: self.resource_id,
            generation: self.generation,
        }
    }

    fn bump_sequence(&mut self) -> Result<TerminalSequence, TerminalError> {
        self.sequence = self.sequence.next()?;
        Ok(self.sequence)
    }

    fn screen_snapshot(&self) -> TerminalScreenSnapshot {
        match &self.projection {
            ProjectionSource::Fixture(replica) => replica.screen_snapshot(),
            ProjectionSource::Attached(runtime) => runtime.screen_snapshot(),
        }
    }

    fn session_view(&self) -> Result<TerminalSessionView, TerminalError> {
        match &self.projection {
            ProjectionSource::Fixture(replica) => {
                replica.view().ok_or(TerminalError::CanonicalReaderPoisoned)
            }
            ProjectionSource::Attached(runtime) => runtime.session_view(),
        }
    }

    fn attachment_fence(&self) -> AttachmentFence {
        AttachmentFence {
            resource_id: self.resource_id,
            generation: self.generation,
        }
    }

    fn verify_attachment(&self) -> Result<(), TerminalError> {
        match &self.projection {
            ProjectionSource::Fixture(_) => Ok(()),
            ProjectionSource::Attached(runtime) => match runtime.current_attachment_fence() {
                Err(_) => Err(TerminalError::TeardownFenceMissing),
                Ok(current) => {
                    if current.resource_id != self.resource_id
                        || current.generation != self.generation
                    {
                        Err(TerminalError::StaleGeneration)
                    } else {
                        Ok(())
                    }
                }
            },
        }
    }

    fn drain_attached_output(&mut self, terminal_id: TerminalId) -> Result<(), TerminalError> {
        // Lifecycle notifications are emitted by the attached reader/waiter
        // before managed teardown may retire its fence. Drain them first so
        // EOF/exit settlement remains observable even after the PTY fence is
        // gone. Output and all mutable operations still require the exact
        // live attachment below.
        self.drain_lifecycle(terminal_id)?;
        match self.verify_attachment() {
            Ok(()) => {}
            // The reader/waiter can publish EOF/exit immediately before the
            // managed teardown retires its fence. Preserve the final
            // read-only projection, while writes/resize/close remain fenced.
            Err(TerminalError::TeardownFenceMissing) if self.exit_summary.is_some() => {}
            Err(error) => return Err(error),
        }
        let drain = {
            let mut pending = self
                .pending_output
                .lock()
                .map_err(|_| TerminalError::CanonicalReaderPoisoned)?;
            pending.take()
        };
        match drain {
            PendingOutputDrain::Empty => {}
            PendingOutputDrain::Coalesced => {
                let screen = {
                    let ProjectionSource::Attached(runtime) = &self.projection else {
                        return Ok(());
                    };
                    runtime.screen_snapshot()
                };
                self.enforce_scrollback_bound_from_screen(&screen);
                self.output_pressure_coalesced = true;
                let sequence = self.bump_sequence()?;
                self.record_delta_from_screen(terminal_id, sequence, &screen)?;
            }
            PendingOutputDrain::Notifications(count) => {
                for _ in 0..count {
                    let screen = {
                        let ProjectionSource::Attached(runtime) = &self.projection else {
                            return Ok(());
                        };
                        runtime.screen_snapshot()
                    };
                    self.enforce_scrollback_bound_from_screen(&screen);
                    let sequence = self.bump_sequence()?;
                    self.record_delta_from_screen(terminal_id, sequence, &screen)?;
                }
            }
        }
        Ok(())
    }

    fn drain_lifecycle(&mut self, terminal_id: TerminalId) -> Result<(), TerminalError> {
        let events = {
            let mut pending = self
                .pending_lifecycle
                .lock()
                .map_err(|_| TerminalError::CanonicalReaderPoisoned)?;
            pending.drain(..).collect::<Vec<_>>()
        };
        for event in events {
            let summary = match event {
                TerminalLifecycleEvent::ReaderEof => String::from("PTY reader reached EOF"),
                TerminalLifecycleEvent::ReaderFailed { summary } => summary,
                TerminalLifecycleEvent::ChildExited { summary, .. } => summary,
            };
            self.exit_summary = Some(summary.clone());
            let sequence = self.bump_sequence()?;
            if self.deltas.len() >= MAX_RETAINED_DELTAS {
                self.deltas.pop_front();
            }
            self.deltas.push_back(TerminalDelta {
                terminal_id,
                generation: self.generation,
                sequence,
                ops: vec![TerminalDeltaOp::Exit { summary }],
            });
        }
        Ok(())
    }

    fn apply_fixture_reader_bytes(
        &mut self,
        terminal_id: TerminalId,
        bytes: &[u8],
    ) -> Result<TerminalSequence, TerminalError> {
        self.ensure_open()?;
        if bytes.is_empty() {
            return Ok(self.sequence);
        }
        if bytes.len() > MAX_INPUT_BYTES {
            return Err(TerminalError::BoundExceeded);
        }
        let ProjectionSource::Fixture(replica) = &self.projection else {
            return Err(TerminalError::FixtureOnly);
        };
        replica.apply_output_bytes(bytes);
        self.enforce_scrollback_bound();
        let sequence = self.bump_sequence()?;
        let screen = self.screen_snapshot();
        self.record_delta_from_screen(terminal_id, sequence, &screen)?;
        Ok(sequence)
    }

    fn enforce_scrollback_bound(&mut self) {
        let screen = self.screen_snapshot();
        self.enforce_scrollback_bound_from_screen(&screen);
    }

    fn enforce_scrollback_bound_from_screen(&mut self, screen: &TerminalScreenSnapshot) {
        let over_rows = screen.history_size > self.spec.max_scrollback_rows;
        let scrollback_bytes = match &self.projection {
            ProjectionSource::Fixture(replica) => replica.scrollback_text().len(),
            ProjectionSource::Attached(_) => screen
                .lines
                .iter()
                .map(|line| {
                    line.iter()
                        .map(|cell| cell.character.len_utf8())
                        .sum::<usize>()
                })
                .sum::<usize>()
                .saturating_add(
                    screen
                        .history_size
                        .saturating_mul(usize::from(screen.cols as u16)),
                ),
        };
        let over_bytes = scrollback_bytes > self.spec.max_scrollback_bytes;
        if over_rows || over_bytes {
            match &self.projection {
                ProjectionSource::Fixture(replica) => {
                    replica.bound_history(self.spec.max_scrollback_rows);
                }
                ProjectionSource::Attached(runtime) => {
                    runtime.bound_history(self.spec.max_scrollback_rows);
                }
            }
            self.truncated = true;
        }
    }

    fn record_delta_from_screen(
        &mut self,
        terminal_id: TerminalId,
        sequence: TerminalSequence,
        screen: &TerminalScreenSnapshot,
    ) -> Result<(), TerminalError> {
        let rows = rows_from_screen(screen);
        let title = self
            .session_view()
            .ok()
            .and_then(|view| view.runtime.title)
            .or_else(|| self.spec.title.clone());
        let mut ops = Vec::new();
        if self.truncated {
            ops.push(TerminalDeltaOp::Truncated {
                dropped_rows: screen.history_size,
            });
        }
        if rows != self.last_rows || ops.is_empty() {
            ops.push(TerminalDeltaOp::RowsChanged {
                start_row: 0,
                rows: rows.clone(),
            });
        }
        if screen.cursor != self.last_cursor {
            ops.push(TerminalDeltaOp::Cursor {
                cursor: screen.cursor,
            });
        }
        if screen.mode != self.last_modes {
            ops.push(TerminalDeltaOp::Mode { modes: screen.mode });
        }
        if title != self.last_title {
            ops.push(TerminalDeltaOp::Title {
                title: title.clone(),
            });
        }
        if screen.display_offset != 0 {
            ops.push(TerminalDeltaOp::Scroll {
                display_offset: screen.display_offset,
            });
        }
        self.last_rows = rows;
        self.last_cursor = screen.cursor;
        self.last_modes = screen.mode;
        self.last_title = title;
        if ops.is_empty() {
            return Ok(());
        }
        if self.deltas.len() >= MAX_RETAINED_DELTAS {
            self.deltas.pop_front();
        }
        self.deltas.push_back(TerminalDelta {
            terminal_id,
            generation: self.generation,
            sequence,
            ops,
        });
        Ok(())
    }

    fn snapshot(&mut self, terminal_id: TerminalId) -> Result<TerminalSnapshot, TerminalError> {
        self.drain_attached_output(terminal_id)?;
        let screen = self.screen_snapshot();
        let title = self
            .session_view()
            .ok()
            .and_then(|view| view.runtime.title)
            .or_else(|| self.spec.title.clone());
        Ok(TerminalSnapshot {
            terminal_id,
            generation: self.generation,
            sequence: self.sequence,
            size: self.spec.size,
            cursor: screen.cursor,
            modes: screen.mode,
            title,
            rows: rows_from_screen(&screen),
            history_rows: screen.history_size,
            truncated: self.truncated,
        })
    }

    fn updates_since(
        &mut self,
        terminal_id: TerminalId,
        client_id: ClientId,
        generation: TerminalGeneration,
        since: TerminalSequence,
    ) -> Result<ReplicaUpdate, TerminalError> {
        self.ensure_open()?;
        self.drain_attached_output(terminal_id)?;
        if generation != self.generation {
            return Ok(ReplicaUpdate::CoalescedSnapshot {
                snapshot: self.snapshot(terminal_id)?,
                reason: CoalesceReason::GenerationMismatch,
            });
        }
        if since.get() > self.sequence.get() {
            return Ok(ReplicaUpdate::CoalescedSnapshot {
                snapshot: self.snapshot(terminal_id)?,
                reason: CoalesceReason::SequenceGap,
            });
        }
        if since == self.sequence {
            self.remember_client_cursor(client_id, generation, since);
            return Ok(ReplicaUpdate::Empty);
        }
        let distance = self.sequence.saturating_distance(since);
        let oldest = self.deltas.front().map(|delta| delta.sequence);
        let contiguous = oldest.is_some_and(|oldest| since.get().saturating_add(1) >= oldest.get())
            && distance <= MAX_RETAINED_DELTAS as u64
            && !self.truncated
            && !self.output_pressure_coalesced;
        if !contiguous {
            let reason = if self.output_pressure_coalesced {
                CoalesceReason::SlowClient
            } else if self.truncated {
                CoalesceReason::ScrollbackTruncated
            } else if distance > MAX_RETAINED_DELTAS as u64 {
                CoalesceReason::SlowClient
            } else {
                CoalesceReason::SequenceGap
            };
            let snapshot = self.snapshot(terminal_id)?;
            self.output_pressure_coalesced = false;
            self.remember_client_cursor(client_id, generation, snapshot.sequence);
            return Ok(ReplicaUpdate::CoalescedSnapshot { snapshot, reason });
        }
        let deltas: Vec<TerminalDelta> = self
            .deltas
            .iter()
            .filter(|delta| delta.sequence.get() > since.get())
            .cloned()
            .collect();
        if let Some(last) = deltas.last() {
            if last.sequence != self.sequence {
                let snapshot = self.snapshot(terminal_id)?;
                self.remember_client_cursor(client_id, generation, snapshot.sequence);
                return Ok(ReplicaUpdate::CoalescedSnapshot {
                    snapshot,
                    reason: CoalesceReason::SequenceGap,
                });
            }
        }
        self.remember_client_cursor(client_id, generation, self.sequence);
        Ok(ReplicaUpdate::Deltas(deltas))
    }

    fn remember_client_cursor(
        &mut self,
        client_id: ClientId,
        generation: TerminalGeneration,
        sequence: TerminalSequence,
    ) {
        if let Some(client) = self.clients.get_mut(&client_id) {
            client.cursor = Some(ClientReplicaCursor {
                generation,
                sequence,
            });
        }
    }

    fn write(&mut self, envelope: &InputEnvelope) -> InputAck {
        if self.closed {
            return InputAck::Rejected {
                reason: InputRejectReason::Closed,
            };
        }
        if envelope.task_id != self.task_id {
            return InputAck::Rejected {
                reason: InputRejectReason::StaleTask,
            };
        }
        if envelope.session_id != self.session_id {
            return InputAck::Rejected {
                reason: InputRejectReason::StaleSession,
            };
        }
        if envelope.terminal_generation != self.generation {
            return InputAck::Rejected {
                reason: InputRejectReason::StaleGeneration,
            };
        }
        if envelope.focus_epoch != self.focus_epoch {
            return InputAck::Rejected {
                reason: InputRejectReason::StaleFocus,
            };
        }
        if envelope.bytes.is_empty() {
            return InputAck::Rejected {
                reason: InputRejectReason::Empty,
            };
        }
        if envelope.bytes.len() > MAX_INPUT_BYTES {
            return InputAck::Rejected {
                reason: InputRejectReason::BoundExceeded,
            };
        }
        if let Some(sequence) = self.accepted_input_ids.get(&envelope.input_id).copied() {
            return InputAck::Duplicate { sequence };
        }
        let grant = self
            .clients
            .get(&envelope.client_id)
            .map(|client| client.grant)
            .unwrap_or(ClientInputGrant::ReadOnly);
        if grant != ClientInputGrant::ReadWrite {
            return InputAck::Rejected {
                reason: InputRejectReason::ReadOnly,
            };
        }
        if self.accepted_input_ids.len() >= MAX_TRACKED_INPUT_IDS {
            return InputAck::Rejected {
                reason: InputRejectReason::BoundExceeded,
            };
        }
        let existing = self.accepted_bytes.len();
        let incoming = envelope.bytes.len();
        let Some(total) = existing.checked_add(incoming) else {
            return InputAck::Rejected {
                reason: InputRejectReason::BoundExceeded,
            };
        };
        if total > MAX_ACCEPTED_INPUT_HISTORY_BYTES {
            return InputAck::Rejected {
                reason: InputRejectReason::BoundExceeded,
            };
        }
        let Some(sequence) = self.accepted_input_sequence.checked_add(1) else {
            return InputAck::Rejected {
                reason: InputRejectReason::BoundExceeded,
            };
        };

        if matches!(&self.projection, ProjectionSource::Attached(_)) {
            if let Err(error) = self.verify_attachment() {
                return InputAck::Rejected {
                    reason: match error {
                        TerminalError::TeardownFenceMissing | TerminalError::StaleGeneration => {
                            InputRejectReason::StaleGeneration
                        }
                        _ => InputRejectReason::RuntimeForwardFailed,
                    },
                };
            }
            if let ProjectionSource::Attached(runtime) = &self.projection {
                if runtime
                    .write_bytes(&envelope.bytes, self.attachment_fence())
                    .is_err()
                {
                    return InputAck::Rejected {
                        reason: InputRejectReason::RuntimeForwardFailed,
                    };
                }
            }
        }

        self.accepted_input_sequence = sequence;
        self.accepted_input_ids.insert(envelope.input_id, sequence);
        self.accepted_bytes.extend_from_slice(&envelope.bytes);
        InputAck::Accepted { sequence }
    }

    fn resize(
        &mut self,
        terminal_id: TerminalId,
        size: TerminalSize,
        fence: Option<ResizeFence>,
    ) -> Result<(), TerminalError> {
        self.ensure_open()?;
        self.verify_attachment()?;
        self.drain_attached_output(terminal_id)?;
        let size = TerminalSize::new(size.cols, size.rows)?;
        let mut commit_view_sequence: Option<(ClientId, u64)> = None;
        if let Some(fence) = fence {
            if fence.generation != self.generation {
                return Err(TerminalError::StaleGeneration);
            }
            if fence.view_sequence == 0 {
                return Err(TerminalError::InvalidFence);
            }
            let client = self
                .clients
                .get(&fence.client_id)
                .ok_or(TerminalError::InvalidFence)?;
            if client
                .resize_sequence
                .is_some_and(|current| fence.view_sequence < current)
            {
                return Ok(());
            }
            commit_view_sequence = Some((fence.client_id, fence.view_sequence));
        }

        match &self.projection {
            ProjectionSource::Attached(runtime) => {
                runtime.resize(size, self.attachment_fence())?;
            }
            ProjectionSource::Fixture(replica) => {
                replica.apply_local_resize(session_dimensions(size));
            }
        }
        if let Some((client_id, view_sequence)) = commit_view_sequence {
            if let Some(client) = self.clients.get_mut(&client_id) {
                client.resize_sequence = Some(view_sequence);
            }
        }
        self.spec.size = size;
        let sequence = self.bump_sequence()?;
        let screen = self.screen_snapshot();
        self.record_delta_from_screen(terminal_id, sequence, &screen)?;
        Ok(())
    }

    fn close(&mut self, reason: CloseReason) -> Result<TeardownReport, TerminalError> {
        if self.closed {
            return Ok(TeardownReport {
                terminal_id: TerminalId::new(),
                generation: self.generation,
                closed: true,
                explicit: true,
                reason,
            });
        }
        if matches!(&self.projection, ProjectionSource::Attached(_)) {
            self.verify_attachment()?;
            let fence = self.attachment_fence();
            if let ProjectionSource::Attached(runtime) = &self.projection {
                runtime.close_exact(reason, fence)?;
            }
        }
        self.closed = true;
        self.clients.clear();
        Ok(TeardownReport {
            terminal_id: TerminalId::new(),
            generation: self.generation,
            closed: true,
            explicit: true,
            reason,
        })
    }

    fn replace_generation(
        &mut self,
        terminal_id: TerminalId,
    ) -> Result<TerminalGeneration, TerminalError> {
        self.ensure_open()?;
        let attached_runtime = match &self.projection {
            ProjectionSource::Fixture(_) => None,
            ProjectionSource::Attached(runtime) => Some(Arc::clone(runtime)),
        };
        if let Some(runtime) = attached_runtime {
            // A restart is owned by the Job/ProcessManager. This method only
            // rebinds the existing service to the exact fence that restart
            // published; it never invents a generation or process authority.
            let attachment = runtime.current_attachment_fence()?;
            if attachment.resource_id == self.resource_id
                && attachment.generation <= self.generation
            {
                return Err(TerminalError::StaleGeneration);
            }
            self.drain_lifecycle(terminal_id)?;
            if let Ok(mut pending) = self.pending_output.lock() {
                let _ = pending.take();
            } else {
                return Err(TerminalError::CanonicalReaderPoisoned);
            }
            let view = runtime.session_view()?;
            let screen = view.screen;
            if let Ok(size) =
                TerminalSize::new(view.runtime.dimensions.cols, view.runtime.dimensions.rows)
            {
                self.spec.size = size;
            }
            self.resource_id = attachment.resource_id;
            self.generation = attachment.generation;
            self.sequence = TerminalSequence::ZERO;
            self.exit_summary = None;
            self.truncated = false;
            self.output_pressure_coalesced = false;
            self.accepted_input_sequence = 0;
            self.accepted_input_ids.clear();
            self.accepted_bytes.clear();
            self.deltas.clear();
            self.last_rows = rows_from_screen(&screen);
            self.last_cursor = screen.cursor;
            self.last_modes = screen.mode;
            self.last_title = view.runtime.title.or_else(|| self.spec.title.clone());
            for client in self.clients.values_mut() {
                client.cursor = None;
                client.resize_sequence = None;
            }
            return Ok(self.generation);
        }

        let ProjectionSource::Fixture(_) = &self.projection else {
            return Err(TerminalError::FixtureOnly);
        };
        let next = self.generation.next()?;
        let mut runtime = SessionRuntimeState::new(
            terminal_id.to_string(),
            PathBuf::from("."),
            session_dimensions(self.spec.size),
            TerminalBackend::PortablePtyFeedingAlacritty,
        );
        runtime.status = SessionStatus::Running;
        runtime.interactive_shell = true;
        runtime.title = self.spec.title.clone();
        runtime.provider_session_id = self.provider_session_id.clone();
        runtime.pid = None;
        let replica = TerminalReplica::from_bootstrap(terminal_id.to_string(), runtime, &[]);
        replica.bound_history(self.spec.max_scrollback_rows);
        self.projection = ProjectionSource::Fixture(replica);
        self.generation = next;
        self.agent_session_id = None;
        self.runtime_generation = None;
        self.action_epoch = None;
        self.sequence = TerminalSequence::ZERO;
        self.truncated = false;
        self.accepted_input_sequence = 0;
        self.accepted_input_ids.clear();
        self.accepted_bytes.clear();
        self.deltas.clear();
        let screen = self.screen_snapshot();
        self.last_rows = rows_from_screen(&screen);
        self.last_cursor = screen.cursor;
        self.last_modes = screen.mode;
        self.last_title = self.spec.title.clone();
        for client in self.clients.values_mut() {
            client.cursor = None;
            client.resize_sequence = None;
        }
        Ok(next)
    }
}

pub struct TerminalService {
    terminals: Mutex<HashMap<TerminalId, HostedTerminal>>,
}

impl Default for TerminalService {
    fn default() -> Self {
        Self::new()
    }
}

impl TerminalService {
    pub fn new() -> Self {
        Self {
            terminals: Mutex::new(HashMap::new()),
        }
    }

    /// Fixture-only in-memory terminal. Does not open a PTY or Job-owned session.
    pub fn create_fixture(
        &self,
        owner: TaskId,
        spec: TerminalSpec,
    ) -> Result<TerminalId, TerminalError> {
        let spec = spec.validated()?;
        let terminal_id = TerminalId::new();
        let hosted = HostedTerminal::open_fixture(owner, spec, terminal_id)?;
        let mut terminals = self.lock()?;
        terminals.insert(terminal_id, hosted);
        Ok(terminal_id)
    }

    /// Backward-compatible alias for the explicit fixture path. Production
    /// callers must use [`Self::attach`] with the Job-owned session runtime.
    pub fn create(&self, owner: TaskId, spec: TerminalSpec) -> Result<TerminalId, TerminalError> {
        self.create_fixture(owner, spec)
    }

    /// Attach a real (or typed mock) Job-owned terminal runtime. One reader
    /// already owned by that runtime feeds this service's projections.
    pub fn attach(
        &self,
        owner: TaskId,
        spec: TerminalSpec,
        runtime: Arc<dyn AttachedTerminalRuntime>,
    ) -> Result<TerminalId, TerminalError> {
        let spec = spec.validated()?;
        let terminal_id = TerminalId::new();
        let hosted = HostedTerminal::open_attached(owner, spec, terminal_id, runtime)?;
        let mut terminals = self.lock()?;
        terminals.insert(terminal_id, hosted);
        Ok(terminal_id)
    }

    /// Bind durable task/agent admission to an already-attached terminal.
    /// Repeating the exact bind is idempotent; conflicting identity fails.
    pub fn bind_task_identity(
        &self,
        id: TerminalId,
        agent_session_id: crate::domain::AgentSessionId,
        runtime_generation: u64,
        action_epoch: u64,
    ) -> Result<(), TerminalError> {
        if runtime_generation == 0 || action_epoch == 0 {
            return Err(TerminalError::InvalidFence);
        }
        let mut terminals = self.lock()?;
        let hosted = terminals.get_mut(&id).ok_or(TerminalError::NotFound)?;
        hosted.ensure_open()?;
        match (
            hosted.agent_session_id,
            hosted.runtime_generation,
            hosted.action_epoch,
        ) {
            (Some(current_agent), Some(current_runtime), Some(current_action))
                if current_agent == agent_session_id
                    && current_runtime == runtime_generation
                    && current_action == action_epoch =>
            {
                Ok(())
            }
            (None, None, None) => {
                hosted.agent_session_id = Some(agent_session_id);
                hosted.runtime_generation = Some(runtime_generation);
                hosted.action_epoch = Some(action_epoch);
                Ok(())
            }
            _ => Err(TerminalError::InvalidFence),
        }
    }

    pub fn write(&self, id: TerminalId, input: InputEnvelope) -> Result<InputAck, TerminalError> {
        if input.terminal_id != id {
            return Err(TerminalError::InvalidFence);
        }
        let mut terminals = self.lock()?;
        let hosted = terminals.get_mut(&id).ok_or(TerminalError::NotFound)?;
        Ok(hosted.write(&input))
    }

    /// Admit raw bytes only to the exact already-bound terminal session. This
    /// path never launches a PTY and rejects stale/missing fences before the
    /// attached runtime writer is called.
    pub fn write_task_input(
        &self,
        request: TerminalInputRequest,
    ) -> Result<InputAck, TerminalError> {
        request.validate()?;
        let context = request.context;
        let mut terminals = self.lock()?;
        let hosted = terminals
            .get_mut(&request.terminal_id)
            .ok_or(TerminalError::NotFound)?;
        hosted.ensure_open()?;
        if hosted.task_id != context.task_id {
            return Ok(InputAck::Rejected {
                reason: InputRejectReason::StaleTask,
            });
        }
        if hosted.agent_session_id != Some(context.agent_session_id) {
            return Ok(InputAck::Rejected {
                reason: InputRejectReason::StaleAgent,
            });
        }
        if hosted.resource_id != context.resource_id {
            return Ok(InputAck::Rejected {
                reason: InputRejectReason::StaleResource,
            });
        }
        if hosted.runtime_generation != Some(context.runtime_generation) {
            return Ok(InputAck::Rejected {
                reason: InputRejectReason::StaleRuntimeGeneration,
            });
        }
        if hosted.generation != context.terminal_generation
            || hosted.generation.get() != context.resource_generation
        {
            return Ok(InputAck::Rejected {
                reason: InputRejectReason::StaleGeneration,
            });
        }
        if hosted.session_id != context.session_id {
            return Ok(InputAck::Rejected {
                reason: InputRejectReason::StaleSession,
            });
        }
        if hosted.focus_epoch != context.focus_epoch {
            return Ok(InputAck::Rejected {
                reason: InputRejectReason::StaleFocus,
            });
        }
        if hosted.action_epoch != Some(context.action_epoch) {
            return Ok(InputAck::Rejected {
                reason: InputRejectReason::StaleAction,
            });
        }
        let envelope = InputEnvelope {
            client_id: request.client_id,
            input_id: request.input_id,
            task_id: context.task_id,
            session_id: context.session_id,
            terminal_id: request.terminal_id,
            terminal_generation: context.terminal_generation,
            focus_epoch: context.focus_epoch,
            bytes: request.bytes,
        };
        if !hosted.accepted_input_ids.contains_key(&envelope.input_id)
            && context.input_sequence != hosted.accepted_input_sequence.saturating_add(1)
        {
            return Ok(InputAck::Rejected {
                reason: InputRejectReason::StaleInputSequence,
            });
        }
        Ok(hosted.write(&envelope))
    }

    pub fn resize(
        &self,
        id: TerminalId,
        size: TerminalSize,
        fence: Option<ResizeFence>,
    ) -> Result<(), TerminalError> {
        let mut terminals = self.lock()?;
        let hosted = terminals.get_mut(&id).ok_or(TerminalError::NotFound)?;
        hosted.resize(id, size, fence)
    }

    pub fn snapshot(&self, id: TerminalId) -> Result<TerminalSnapshot, TerminalError> {
        let mut terminals = self.lock()?;
        let hosted = terminals.get_mut(&id).ok_or(TerminalError::NotFound)?;
        hosted.ensure_open()?;
        hosted.snapshot(id)
    }

    pub fn close(
        &self,
        id: TerminalId,
        reason: CloseReason,
    ) -> Result<TeardownReport, TerminalError> {
        let mut terminals = self.lock()?;
        let hosted = terminals.get_mut(&id).ok_or(TerminalError::NotFound)?;
        let mut report = hosted.close(reason)?;
        report.terminal_id = id;
        Ok(report)
    }

    pub fn disconnect_view(
        &self,
        id: TerminalId,
        client_id: ClientId,
    ) -> Result<(), TerminalError> {
        let mut terminals = self.lock()?;
        let hosted = terminals.get_mut(&id).ok_or(TerminalError::NotFound)?;
        hosted.ensure_open()?;
        hosted.clients.remove(&client_id);
        Ok(())
    }

    pub fn grant_client(
        &self,
        id: TerminalId,
        client_id: ClientId,
        grant: ClientInputGrant,
    ) -> Result<(), TerminalError> {
        let mut terminals = self.lock()?;
        let hosted = terminals.get_mut(&id).ok_or(TerminalError::NotFound)?;
        hosted.ensure_open()?;
        hosted.clients.insert(
            client_id,
            ClientViewState {
                grant,
                cursor: None,
                resize_sequence: None,
            },
        );
        Ok(())
    }

    pub fn open_view(
        &self,
        id: TerminalId,
        kind: ViewKind,
    ) -> Result<TerminalViewHandle, TerminalError> {
        let mut terminals = self.lock()?;
        let hosted = terminals.get_mut(&id).ok_or(TerminalError::NotFound)?;
        hosted.ensure_open()?;
        hosted.view_count = hosted
            .view_count
            .checked_add(1)
            .ok_or(TerminalError::BoundExceeded)?;
        Ok(TerminalViewHandle {
            terminal_id: id,
            generation: hosted.generation,
            kind,
        })
    }

    pub fn raw_view(&self, id: TerminalId) -> Result<TerminalScreenSnapshot, TerminalError> {
        let mut terminals = self.lock()?;
        let hosted = terminals.get_mut(&id).ok_or(TerminalError::NotFound)?;
        hosted.ensure_open()?;
        hosted.drain_attached_output(id)?;
        Ok(hosted.screen_snapshot())
    }

    pub fn session_view(&self, id: TerminalId) -> Result<TerminalSessionView, TerminalError> {
        let mut terminals = self.lock()?;
        let hosted = terminals.get_mut(&id).ok_or(TerminalError::NotFound)?;
        hosted.ensure_open()?;
        hosted.drain_attached_output(id)?;
        hosted.session_view()
    }

    /// Fixture-only reader injection. Attached runtimes publish through their
    /// single PTY reader + service output sink instead.
    pub fn admit_reader_bytes(
        &self,
        id: TerminalId,
        bytes: &[u8],
    ) -> Result<TerminalSequence, TerminalError> {
        let mut terminals = self.lock()?;
        let hosted = terminals.get_mut(&id).ok_or(TerminalError::NotFound)?;
        hosted.apply_fixture_reader_bytes(id, bytes)
    }

    /// Drain pending attached-reader notifications into the service projection.
    pub fn pump_attached_output(&self, id: TerminalId) -> Result<(), TerminalError> {
        let mut terminals = self.lock()?;
        let hosted = terminals.get_mut(&id).ok_or(TerminalError::NotFound)?;
        hosted.ensure_open()?;
        hosted.drain_attached_output(id)
    }

    pub fn updates_since(
        &self,
        id: TerminalId,
        client_id: ClientId,
        generation: TerminalGeneration,
        since: TerminalSequence,
    ) -> Result<ReplicaUpdate, TerminalError> {
        let mut terminals = self.lock()?;
        let hosted = terminals.get_mut(&id).ok_or(TerminalError::NotFound)?;
        hosted.updates_since(id, client_id, generation, since)
    }

    pub fn canonical_reader_count(&self, id: TerminalId) -> Result<u32, TerminalError> {
        let terminals = self.lock()?;
        let hosted = terminals.get(&id).ok_or(TerminalError::NotFound)?;
        hosted.ensure_open()?;
        Ok(hosted.reader_count)
    }

    pub fn view_count(&self, id: TerminalId) -> Result<u32, TerminalError> {
        let terminals = self.lock()?;
        let hosted = terminals.get(&id).ok_or(TerminalError::NotFound)?;
        hosted.ensure_open()?;
        Ok(hosted.view_count)
    }

    pub fn is_open(&self, id: TerminalId) -> Result<bool, TerminalError> {
        let terminals = self.lock()?;
        let hosted = terminals.get(&id).ok_or(TerminalError::NotFound)?;
        Ok(!hosted.closed)
    }

    pub fn is_attached(&self, id: TerminalId) -> Result<bool, TerminalError> {
        let terminals = self.lock()?;
        let hosted = terminals.get(&id).ok_or(TerminalError::NotFound)?;
        Ok(hosted.is_attached())
    }

    pub fn fence(&self, id: TerminalId) -> Result<TerminalResourceFence, TerminalError> {
        let terminals = self.lock()?;
        let hosted = terminals.get(&id).ok_or(TerminalError::NotFound)?;
        hosted.ensure_open()?;
        Ok(hosted.fence(id))
    }

    pub fn current_task(&self, id: TerminalId) -> Result<TaskId, TerminalError> {
        let terminals = self.lock()?;
        let hosted = terminals.get(&id).ok_or(TerminalError::NotFound)?;
        hosted.ensure_open()?;
        Ok(hosted.task_id)
    }

    pub fn current_session(&self, id: TerminalId) -> Result<TerminalSessionId, TerminalError> {
        let terminals = self.lock()?;
        let hosted = terminals.get(&id).ok_or(TerminalError::NotFound)?;
        hosted.ensure_open()?;
        Ok(hosted.session_id)
    }

    pub fn current_generation(&self, id: TerminalId) -> Result<TerminalGeneration, TerminalError> {
        let terminals = self.lock()?;
        let hosted = terminals.get(&id).ok_or(TerminalError::NotFound)?;
        hosted.ensure_open()?;
        Ok(hosted.generation)
    }

    pub fn current_focus(&self, id: TerminalId) -> Result<FocusEpoch, TerminalError> {
        let terminals = self.lock()?;
        let hosted = terminals.get(&id).ok_or(TerminalError::NotFound)?;
        hosted.ensure_open()?;
        Ok(hosted.focus_epoch)
    }

    pub fn accepted_input_bytes(&self, id: TerminalId) -> Result<Vec<u8>, TerminalError> {
        let terminals = self.lock()?;
        let hosted = terminals.get(&id).ok_or(TerminalError::NotFound)?;
        Ok(hosted.accepted_bytes.clone())
    }

    /// Reader/child exit summary. Distinct from explicit Closed.
    pub fn exit_summary(&self, id: TerminalId) -> Result<Option<String>, TerminalError> {
        let terminals = self.lock()?;
        let hosted = terminals.get(&id).ok_or(TerminalError::NotFound)?;
        Ok(hosted.exit_summary.clone())
    }

    pub fn set_provider_session_id(
        &self,
        id: TerminalId,
        provider_session_id: impl Into<String>,
    ) -> Result<(), TerminalError> {
        let mut terminals = self.lock()?;
        let hosted = terminals.get_mut(&id).ok_or(TerminalError::NotFound)?;
        hosted.ensure_open()?;
        let provider_session_id = provider_session_id.into();
        hosted.provider_session_id = Some(provider_session_id.clone());
        if let ProjectionSource::Fixture(replica) = &hosted.projection {
            if let Some(mut view) = replica.view() {
                view.runtime.provider_session_id = Some(provider_session_id);
                replica.apply_runtime(view.runtime);
            }
        }
        Ok(())
    }

    pub fn provider_session_id(&self, id: TerminalId) -> Result<Option<String>, TerminalError> {
        let terminals = self.lock()?;
        let hosted = terminals.get(&id).ok_or(TerminalError::NotFound)?;
        hosted.ensure_open()?;
        Ok(hosted.provider_session_id.clone())
    }

    pub fn retarget_task(&self, id: TerminalId, task_id: TaskId) -> Result<(), TerminalError> {
        let mut terminals = self.lock()?;
        let hosted = terminals.get_mut(&id).ok_or(TerminalError::NotFound)?;
        hosted.ensure_open()?;
        hosted.task_id = task_id;
        Ok(())
    }

    pub fn retarget_session(
        &self,
        id: TerminalId,
        session_id: TerminalSessionId,
    ) -> Result<(), TerminalError> {
        let mut terminals = self.lock()?;
        let hosted = terminals.get_mut(&id).ok_or(TerminalError::NotFound)?;
        hosted.ensure_open()?;
        hosted.session_id = session_id;
        Ok(())
    }

    pub fn advance_focus(&self, id: TerminalId) -> Result<FocusEpoch, TerminalError> {
        let mut terminals = self.lock()?;
        let hosted = terminals.get_mut(&id).ok_or(TerminalError::NotFound)?;
        hosted.ensure_open()?;
        hosted.focus_epoch = hosted.focus_epoch.next()?;
        Ok(hosted.focus_epoch)
    }

    pub fn replace_generation(&self, id: TerminalId) -> Result<TerminalGeneration, TerminalError> {
        let mut terminals = self.lock()?;
        let hosted = terminals.get_mut(&id).ok_or(TerminalError::NotFound)?;
        hosted.replace_generation(id)
    }

    fn lock(
        &self,
    ) -> Result<std::sync::MutexGuard<'_, HashMap<TerminalId, HostedTerminal>>, TerminalError> {
        self.terminals
            .lock()
            .map_err(|_| TerminalError::CanonicalReaderPoisoned)
    }
}

fn session_dimensions(size: TerminalSize) -> SessionDimensions {
    SessionDimensions {
        cols: size.cols,
        rows: size.rows,
        cell_width: 8,
        cell_height: 18,
    }
}

fn rows_from_screen(screen: &TerminalScreenSnapshot) -> Vec<String> {
    screen
        .lines
        .iter()
        .map(|line| line.iter().map(|cell| cell.character).collect::<String>())
        .collect()
}

/// Deterministic typed runtime for portable terminal-service tests.
/// Never uses raw PIDs; close can fail closed on a missing fence.
pub struct MockAttachedRuntime {
    written: Mutex<Vec<u8>>,
    size: Mutex<TerminalSize>,
    rows: Mutex<Vec<String>>,
    sink: Mutex<Option<TerminalOutputSink>>,
    lifecycle_sink: Mutex<Option<TerminalLifecycleSink>>,
    fail_write: std::sync::atomic::AtomicBool,
    fail_resize: std::sync::atomic::AtomicBool,
    fail_close: std::sync::atomic::AtomicBool,
    missing_fence: std::sync::atomic::AtomicBool,
    fence: Mutex<AttachmentFence>,
    title: Mutex<Option<String>>,
    history_bound: Mutex<usize>,
}

impl MockAttachedRuntime {
    pub fn new(size: TerminalSize) -> Arc<Self> {
        let rows = usize::from(size.rows);
        let cols = usize::from(size.cols);
        Arc::new(Self {
            written: Mutex::new(Vec::new()),
            size: Mutex::new(size),
            rows: Mutex::new(vec![" ".repeat(cols); rows]),
            sink: Mutex::new(None),
            lifecycle_sink: Mutex::new(None),
            fail_write: std::sync::atomic::AtomicBool::new(false),
            fail_resize: std::sync::atomic::AtomicBool::new(false),
            fail_close: std::sync::atomic::AtomicBool::new(false),
            missing_fence: std::sync::atomic::AtomicBool::new(false),
            fence: Mutex::new(AttachmentFence {
                resource_id: ResourceId::new(),
                generation: TerminalGeneration::initial(),
            }),
            title: Mutex::new(None),
            history_bound: Mutex::new(10_000),
        })
    }

    pub fn written_bytes(&self) -> Vec<u8> {
        self.written
            .lock()
            .map(|bytes| bytes.clone())
            .unwrap_or_default()
    }

    pub fn current_size(&self) -> TerminalSize {
        *self.size.lock().expect("size")
    }

    pub fn history_bound(&self) -> usize {
        *self.history_bound.lock().expect("history")
    }

    pub fn set_fail_write(&self, fail: bool) {
        self.fail_write
            .store(fail, std::sync::atomic::Ordering::SeqCst);
    }

    pub fn set_fail_resize(&self, fail: bool) {
        self.fail_resize
            .store(fail, std::sync::atomic::Ordering::SeqCst);
    }

    pub fn set_fail_close(&self, fail: bool) {
        self.fail_close
            .store(fail, std::sync::atomic::Ordering::SeqCst);
    }

    pub fn set_missing_fence(&self, missing: bool) {
        self.missing_fence
            .store(missing, std::sync::atomic::Ordering::SeqCst);
    }

    pub fn bump_attachment_generation(&self) {
        let mut fence = self.fence.lock().expect("fence");
        fence.generation = fence.generation.next().expect("next generation");
    }

    /// Simulate the single PTY reader publishing already-parsed output.
    pub fn inject_reader_output(&self, text: &str) {
        let mut rows = self.rows.lock().expect("rows");
        if let Some(first) = rows.first_mut() {
            let cols = first.len();
            let mut line = text.to_string();
            while line.len() < cols {
                line.push(' ');
            }
            line.truncate(cols);
            *first = line;
        }
        drop(rows);
        if let Some(sink) = self.sink.lock().expect("sink").as_ref() {
            sink(text.as_bytes().to_vec(), TerminalModeSnapshot::default());
        }
    }

    pub fn inject_reader_eof(&self) {
        if let Some(sink) = self.lifecycle_sink.lock().expect("lifecycle").as_ref() {
            sink(TerminalLifecycleEvent::ReaderEof);
        }
    }
}

impl AttachedTerminalRuntime for MockAttachedRuntime {
    fn write_bytes(&self, bytes: &[u8], expected: AttachmentFence) -> Result<(), TerminalError> {
        self.matches_attachment(expected)?;
        if self.fail_write.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(TerminalError::RuntimeIo);
        }
        self.written
            .lock()
            .map_err(|_| TerminalError::CanonicalReaderPoisoned)?
            .extend_from_slice(bytes);
        Ok(())
    }

    fn resize(&self, size: TerminalSize, expected: AttachmentFence) -> Result<(), TerminalError> {
        self.matches_attachment(expected)?;
        if self.fail_resize.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(TerminalError::RuntimeIo);
        }
        *self
            .size
            .lock()
            .map_err(|_| TerminalError::CanonicalReaderPoisoned)? = size;
        Ok(())
    }

    fn close_exact(
        &self,
        _reason: CloseReason,
        expected: AttachmentFence,
    ) -> Result<(), TerminalError> {
        self.matches_attachment(expected)?;
        if self.fail_close.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(TerminalError::TeardownFailed);
        }
        Ok(())
    }

    fn screen_snapshot(&self) -> TerminalScreenSnapshot {
        let rows = self.rows.lock().expect("rows").clone();
        let size = *self.size.lock().expect("size");
        let lines = rows
            .iter()
            .map(|row| {
                row.chars()
                    .map(|character| crate::terminal::session::TerminalCellSnapshot {
                        character,
                        zero_width: Vec::new(),
                        foreground: 0,
                        background: 0,
                        bold: false,
                        dim: false,
                        italic: false,
                        underline: false,
                        undercurl: false,
                        strike: false,
                        hidden: false,
                        has_hyperlink: false,
                        default_background: true,
                    })
                    .collect()
            })
            .collect();
        TerminalScreenSnapshot {
            cells: Vec::new(),
            lines,
            cursor: None,
            display_offset: 0,
            history_size: 0,
            total_lines: rows.len(),
            rows: usize::from(size.rows),
            cols: usize::from(size.cols),
            mode: TerminalModeSnapshot::default(),
        }
    }

    fn session_view(&self) -> Result<TerminalSessionView, TerminalError> {
        let size = *self.size.lock().expect("size");
        let mut runtime = SessionRuntimeState::new(
            "mock-attached",
            PathBuf::from("."),
            session_dimensions(size),
            TerminalBackend::PortablePtyFeedingAlacritty,
        );
        runtime.status = SessionStatus::Running;
        runtime.pid = None;
        runtime.title = self.title.lock().expect("title").clone();
        Ok(TerminalSessionView {
            runtime,
            screen: self.screen_snapshot(),
        })
    }

    fn bound_history(&self, max_lines: usize) {
        *self.history_bound.lock().expect("history") = max_lines.max(1);
    }

    fn install_output_sink(&self, sink: TerminalOutputSink) -> Result<(), TerminalError> {
        *self
            .sink
            .lock()
            .map_err(|_| TerminalError::CanonicalReaderPoisoned)? = Some(sink);
        Ok(())
    }

    fn install_lifecycle_sink(&self, sink: TerminalLifecycleSink) -> Result<(), TerminalError> {
        *self
            .lifecycle_sink
            .lock()
            .map_err(|_| TerminalError::CanonicalReaderPoisoned)? = Some(sink);
        Ok(())
    }

    fn current_attachment_fence(&self) -> Result<AttachmentFence, TerminalError> {
        if self.missing_fence.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(TerminalError::TeardownFenceMissing);
        }
        Ok(*self
            .fence
            .lock()
            .map_err(|_| TerminalError::CanonicalReaderPoisoned)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::terminal::protocol::InputId;

    #[test]
    fn fixture_reader_survives_two_view_openings() {
        let service = TerminalService::new();
        let task = TaskId::new();
        let session = TerminalSessionId::new();
        let spec =
            TerminalSpec::new(session, TerminalSize::new(40, 10).expect("size")).expect("spec");
        let id = service.create_fixture(task, spec).expect("create");
        service.open_view(id, ViewKind::Raw).expect("raw view");
        service
            .open_view(id, ViewKind::Session)
            .expect("session view");
        assert_eq!(service.canonical_reader_count(id).expect("reader"), 1);
        assert_eq!(service.view_count(id).expect("views"), 2);
        assert!(!service.is_attached(id).expect("fixture"));
    }

    #[test]
    fn write_requires_matching_fences_on_fixture() {
        let service = TerminalService::new();
        let task = TaskId::new();
        let session = TerminalSessionId::new();
        let spec =
            TerminalSpec::new(session, TerminalSize::new(40, 10).expect("size")).expect("spec");
        let id = service.create_fixture(task, spec).expect("create");
        let client = ClientId::new();
        service
            .grant_client(id, client, ClientInputGrant::ReadWrite)
            .expect("grant");
        let ack = service
            .write(
                id,
                InputEnvelope {
                    client_id: client,
                    input_id: InputId::new(),
                    task_id: task,
                    session_id: session,
                    terminal_id: id,
                    terminal_generation: service.current_generation(id).expect("gen"),
                    focus_epoch: service.current_focus(id).expect("focus"),
                    bytes: b"hi".to_vec(),
                },
            )
            .expect("write");
        assert_eq!(ack, InputAck::Accepted { sequence: 1 });
    }

    #[test]
    fn task_input_checks_exact_identity_sequence_and_raw_bytes() {
        let service = TerminalService::new();
        let task = TaskId::new();
        let agent = crate::domain::AgentSessionId::new();
        let session = TerminalSessionId::new();
        let spec =
            TerminalSpec::new(session, TerminalSize::new(40, 10).expect("size")).expect("spec");
        let terminal_id = service.create_fixture(task, spec).expect("fixture");
        let client = ClientId::new();
        service
            .grant_client(terminal_id, client, ClientInputGrant::ReadWrite)
            .expect("grant");
        service
            .bind_task_identity(terminal_id, agent, 9, 11)
            .expect("bind");
        let fence = service.fence(terminal_id).expect("fence");
        let context = TerminalInputContext {
            task_id: task,
            agent_session_id: agent,
            resource_id: fence.resource_id,
            runtime_generation: 9,
            resource_generation: fence.generation.get(),
            session_id: fence.session_id,
            terminal_generation: fence.generation,
            focus_epoch: service.current_focus(terminal_id).expect("focus"),
            action_epoch: 11,
            input_sequence: 1,
        };
        let bytes = vec![0x03, 0x1b, b'[', b'2', b'0', b'~', 0x00];
        assert_eq!(
            service
                .write_task_input(TerminalInputRequest {
                    client_id: client,
                    input_id: InputId::new(),
                    terminal_id,
                    context,
                    bytes: bytes.clone(),
                })
                .expect("write"),
            InputAck::Accepted { sequence: 1 }
        );
        assert_eq!(
            service.accepted_input_bytes(terminal_id).expect("history"),
            bytes
        );
        let mut stale = context;
        stale.input_sequence = 3;
        assert_eq!(
            service
                .write_task_input(TerminalInputRequest {
                    client_id: client,
                    input_id: InputId::new(),
                    terminal_id,
                    context: stale,
                    bytes: b"not forwarded".to_vec(),
                })
                .expect("typed stale ack"),
            InputAck::Rejected {
                reason: InputRejectReason::StaleInputSequence
            }
        );
        assert!(!service
            .is_attached(terminal_id)
            .expect("fixture has no PTY"));
    }

    #[test]
    fn stale_attachment_fence_rejects_write_resize_and_close() {
        let service = TerminalService::new();
        let runtime = MockAttachedRuntime::new(TerminalSize::new(40, 8).expect("size"));
        let task = TaskId::new();
        let session = TerminalSessionId::new();
        let id = service
            .attach(
                task,
                TerminalSpec::new(session, TerminalSize::new(40, 8).expect("size")).expect("spec"),
                runtime.clone(),
            )
            .expect("attach");
        let client = ClientId::new();
        service
            .grant_client(id, client, ClientInputGrant::ReadWrite)
            .expect("grant");
        runtime.bump_attachment_generation();
        let rejected = service
            .write(
                id,
                InputEnvelope {
                    client_id: client,
                    input_id: InputId::new(),
                    task_id: task,
                    session_id: session,
                    terminal_id: id,
                    terminal_generation: service.current_generation(id).expect("gen"),
                    focus_epoch: service.current_focus(id).expect("focus"),
                    bytes: b"stale".to_vec(),
                },
            )
            .expect("write");
        assert_eq!(
            rejected,
            InputAck::Rejected {
                reason: InputRejectReason::StaleGeneration
            }
        );
        assert!(runtime.written_bytes().is_empty());
        assert_eq!(
            service.resize(
                id,
                TerminalSize::new(20, 8).expect("size"),
                Some(ResizeFence {
                    generation: service.current_generation(id).expect("gen"),
                    client_id: client,
                    view_sequence: 1,
                }),
            ),
            Err(TerminalError::StaleGeneration)
        );
        assert_eq!(
            service.close(id, CloseReason::ExplicitServiceClose),
            Err(TerminalError::StaleGeneration)
        );
        assert!(service.is_open(id).expect("still open"));
    }

    #[test]
    fn failed_resize_does_not_commit_view_sequence() {
        let service = TerminalService::new();
        let runtime = MockAttachedRuntime::new(TerminalSize::new(40, 8).expect("size"));
        let task = TaskId::new();
        let session = TerminalSessionId::new();
        let id = service
            .attach(
                task,
                TerminalSpec::new(session, TerminalSize::new(40, 8).expect("size")).expect("spec"),
                runtime.clone(),
            )
            .expect("attach");
        let client = ClientId::new();
        service
            .grant_client(id, client, ClientInputGrant::ReadWrite)
            .expect("grant");
        runtime.set_fail_resize(true);
        let fence = ResizeFence {
            generation: service.current_generation(id).expect("gen"),
            client_id: client,
            view_sequence: 7,
        };
        assert_eq!(
            service.resize(id, TerminalSize::new(12, 8).expect("size"), Some(fence)),
            Err(TerminalError::RuntimeIo)
        );
        assert_eq!(runtime.current_size().cols, 40);
        runtime.set_fail_resize(false);
        service
            .resize(id, TerminalSize::new(12, 8).expect("size"), Some(fence))
            .expect("retry same view sequence");
        assert_eq!(runtime.current_size().cols, 12);
    }

    #[test]
    fn reader_eof_sets_exit_without_closed() {
        let service = TerminalService::new();
        let runtime = MockAttachedRuntime::new(TerminalSize::new(40, 8).expect("size"));
        let id = service
            .attach(
                TaskId::new(),
                TerminalSpec::new(
                    TerminalSessionId::new(),
                    TerminalSize::new(40, 8).expect("size"),
                )
                .expect("spec"),
                runtime.clone(),
            )
            .expect("attach");
        runtime.inject_reader_eof();
        service.pump_attached_output(id).expect("pump");
        assert_eq!(
            service.exit_summary(id).expect("exit"),
            Some(String::from("PTY reader reached EOF"))
        );
        assert!(service.is_open(id).expect("still open after eof"));
    }

    #[test]
    fn bound_history_honors_small_max() {
        let runtime = MockAttachedRuntime::new(TerminalSize::new(40, 8).expect("size"));
        runtime.bound_history(3);
        assert_eq!(runtime.history_bound(), 3);
    }
}
