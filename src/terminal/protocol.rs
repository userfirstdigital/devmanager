//! Typed Phase 3 terminal identity, replica, and input-routing contracts.
//!
//! These types carry host-owned fences only. PID and process handles are never
//! authorization. Provider conversation identity stays off this boundary.

use std::fmt;
use std::num::NonZeroU64;

use serde::{Deserialize, Serialize};
use uuid::{Uuid, Variant};

use crate::domain::id::{AgentSessionId, ClientId, IdError, ResourceId, TaskId, TerminalId};
use crate::terminal::session::{TerminalCursorSnapshot, TerminalModeSnapshot};

pub const MAX_TERMINAL_COLS: u16 = 512;
pub const MAX_TERMINAL_ROWS: u16 = 256;
pub const MAX_SCROLLBACK_ROWS: usize = 10_000;
pub const MAX_SCROLLBACK_BYTES: usize = 1024 * 1024;
pub const MAX_RETAINED_DELTAS: usize = 64;
pub const MAX_PENDING_OUTPUT_CHUNKS: usize = 64;
pub const MAX_PENDING_OUTPUT_BYTES: usize = 256 * 1024;
pub const MAX_INPUT_BYTES: usize = 4 * 1024 * 1024;
pub const MAX_ACCEPTED_INPUT_HISTORY_BYTES: usize = 4 * 1024 * 1024;
pub const MAX_TRACKED_INPUT_IDS: usize = 4_096;
pub const MAX_TITLE_BYTES: usize = 1_024;

/// Disposable PTY generation. Never interchangeable with provider session identity.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct TerminalGeneration(NonZeroU64);

impl TerminalGeneration {
    pub fn initial() -> Self {
        Self(NonZeroU64::MIN)
    }

    pub fn from_raw(raw: u64) -> Result<Self, TerminalError> {
        NonZeroU64::new(raw)
            .map(Self)
            .ok_or(TerminalError::InvalidFence)
    }

    pub fn get(self) -> u64 {
        self.0.get()
    }

    pub fn next(self) -> Result<Self, TerminalError> {
        self.0
            .get()
            .checked_add(1)
            .and_then(NonZeroU64::new)
            .map(Self)
            .ok_or(TerminalError::GenerationOverflow)
    }
}

/// Monotonic output/view sequence for one terminal generation.
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
pub struct TerminalSequence(u64);

impl TerminalSequence {
    pub const ZERO: Self = Self(0);

    pub fn from_raw(raw: u64) -> Self {
        Self(raw)
    }

    pub fn get(self) -> u64 {
        self.0
    }

    pub fn next(self) -> Result<Self, TerminalError> {
        self.0
            .checked_add(1)
            .map(Self)
            .ok_or(TerminalError::SequenceOverflow)
    }

    pub fn saturating_distance(self, earlier: Self) -> u64 {
        self.0.saturating_sub(earlier.0)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct FocusEpoch(NonZeroU64);

impl FocusEpoch {
    pub fn initial() -> Self {
        Self(NonZeroU64::MIN)
    }

    pub fn from_raw(raw: u64) -> Result<Self, TerminalError> {
        NonZeroU64::new(raw)
            .map(Self)
            .ok_or(TerminalError::InvalidFence)
    }

    pub fn get(self) -> u64 {
        self.0.get()
    }

    pub fn next(self) -> Result<Self, TerminalError> {
        self.0
            .get()
            .checked_add(1)
            .and_then(NonZeroU64::new)
            .map(Self)
            .ok_or(TerminalError::FocusOverflow)
    }
}

/// Host-owned terminal session fence. Distinct from [`crate::domain::AgentSessionId`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TerminalSessionId(Uuid);

impl TerminalSessionId {
    pub fn new() -> Self {
        Self(Uuid::now_v7())
    }

    pub fn parse(input: &str) -> Result<Self, IdError> {
        let uuid = Uuid::parse_str(input).map_err(|_| IdError::InvalidFormat)?;
        if uuid.get_version_num() != 7 {
            return Err(IdError::InvalidVersion);
        }
        if uuid.get_variant() != Variant::RFC4122 {
            return Err(IdError::InvalidVariant);
        }
        Ok(Self(uuid))
    }
}

impl Default for TerminalSessionId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for TerminalSessionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct InputId(Uuid);

impl InputId {
    pub fn new() -> Self {
        Self(Uuid::now_v7())
    }

    pub fn parse(input: &str) -> Result<Self, IdError> {
        let uuid = Uuid::parse_str(input).map_err(|_| IdError::InvalidFormat)?;
        if uuid.get_version_num() != 7 {
            return Err(IdError::InvalidVersion);
        }
        if uuid.get_variant() != Variant::RFC4122 {
            return Err(IdError::InvalidVariant);
        }
        Ok(Self(uuid))
    }

    pub fn from_bytes(bytes: [u8; 16]) -> Result<Self, IdError> {
        let uuid = Uuid::from_bytes(bytes);
        if uuid.get_version_num() != 7 {
            return Err(IdError::InvalidVersion);
        }
        if uuid.get_variant() != Variant::RFC4122 {
            return Err(IdError::InvalidVariant);
        }
        Ok(Self(uuid))
    }

    pub fn as_bytes(&self) -> &[u8; 16] {
        self.0.as_bytes()
    }
}

impl Default for InputId {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TerminalSize {
    pub cols: u16,
    pub rows: u16,
}

impl TerminalSize {
    pub fn new(cols: u16, rows: u16) -> Result<Self, TerminalError> {
        if cols == 0 || rows == 0 || cols > MAX_TERMINAL_COLS || rows > MAX_TERMINAL_ROWS {
            return Err(TerminalError::InvalidSize);
        }
        Ok(Self { cols, rows })
    }
}

impl Default for TerminalSize {
    fn default() -> Self {
        Self { cols: 80, rows: 24 }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TerminalSpec {
    pub session_id: TerminalSessionId,
    pub size: TerminalSize,
    pub max_scrollback_rows: usize,
    pub max_scrollback_bytes: usize,
    pub title: Option<String>,
}

impl TerminalSpec {
    pub fn new(session_id: TerminalSessionId, size: TerminalSize) -> Result<Self, TerminalError> {
        Self {
            session_id,
            size,
            max_scrollback_rows: 1_000,
            max_scrollback_bytes: 64 * 1024,
            title: None,
        }
        .validated()
    }

    pub fn validated(self) -> Result<Self, TerminalError> {
        let _ = TerminalSize::new(self.size.cols, self.size.rows)?;
        if self.max_scrollback_rows == 0 || self.max_scrollback_rows > MAX_SCROLLBACK_ROWS {
            return Err(TerminalError::BoundExceeded);
        }
        if self.max_scrollback_bytes == 0 || self.max_scrollback_bytes > MAX_SCROLLBACK_BYTES {
            return Err(TerminalError::BoundExceeded);
        }
        if let Some(title) = &self.title {
            if title.len() > MAX_TITLE_BYTES {
                return Err(TerminalError::BoundExceeded);
            }
        }
        Ok(self)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalSnapshot {
    pub terminal_id: TerminalId,
    pub generation: TerminalGeneration,
    pub sequence: TerminalSequence,
    pub size: TerminalSize,
    pub cursor: Option<TerminalCursorSnapshot>,
    pub modes: TerminalModeSnapshot,
    pub title: Option<String>,
    pub rows: Vec<String>,
    pub history_rows: usize,
    pub truncated: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum TerminalDeltaOp {
    RowsChanged {
        start_row: u16,
        rows: Vec<String>,
    },
    Scroll {
        display_offset: usize,
    },
    Cursor {
        cursor: Option<TerminalCursorSnapshot>,
    },
    Mode {
        modes: TerminalModeSnapshot,
    },
    Title {
        title: Option<String>,
    },
    Truncated {
        dropped_rows: usize,
    },
    Exit {
        summary: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalDelta {
    pub terminal_id: TerminalId,
    pub generation: TerminalGeneration,
    pub sequence: TerminalSequence,
    pub ops: Vec<TerminalDeltaOp>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReplicaApplyResult {
    Applied,
    NeedSnapshot,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReplicaUpdate {
    Empty,
    Deltas(Vec<TerminalDelta>),
    Snapshot(TerminalSnapshot),
    CoalescedSnapshot {
        snapshot: TerminalSnapshot,
        reason: CoalesceReason,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CoalesceReason {
    SequenceGap,
    GenerationMismatch,
    SlowClient,
    ScrollbackTruncated,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InputEnvelope {
    pub client_id: ClientId,
    pub input_id: InputId,
    pub task_id: TaskId,
    pub session_id: TerminalSessionId,
    pub terminal_id: TerminalId,
    pub terminal_generation: TerminalGeneration,
    pub focus_epoch: FocusEpoch,
    pub bytes: Vec<u8>,
}

/// Exact identity captured by the native task cockpit for one raw input
/// gesture. Every durable/runtime fence is checked before bytes reach the
/// already-owned terminal session.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalInputContext {
    pub task_id: TaskId,
    pub agent_session_id: AgentSessionId,
    pub resource_id: ResourceId,
    pub runtime_generation: u64,
    pub resource_generation: u64,
    pub session_id: TerminalSessionId,
    pub terminal_generation: TerminalGeneration,
    pub focus_epoch: FocusEpoch,
    pub action_epoch: u64,
    pub input_sequence: u64,
}

impl TerminalInputContext {
    /// Whether this context carries the plain-shell sentinels rather than a
    /// provider fence.
    ///
    /// A shell has no agent session, no provider runtime generation and no
    /// launch action epoch, so the host sends the documented zeros for all
    /// three. Requiring the exact triple keeps this a shape test rather than a
    /// hole: a context that zeroes only one of them is still a malformed
    /// provider fence and is refused by `validate`, and neither shape can
    /// address the other kind of terminal because the host compares the
    /// hosted terminal's own agent session against it.
    pub fn is_plain_shell_fence(self) -> bool {
        self.agent_session_id.is_nil() && self.runtime_generation == 0 && self.action_epoch == 0
    }

    pub fn validate(self) -> Result<(), TerminalError> {
        // Required of every terminal, shell or provider: the durable resource
        // generation this input is aimed at, and a real input sequence.
        if self.resource_generation == 0 || self.input_sequence == 0 {
            return Err(TerminalError::InvalidFence);
        }
        if self.is_plain_shell_fence() {
            return Ok(());
        }
        if self.runtime_generation == 0 || self.action_epoch == 0 {
            return Err(TerminalError::InvalidFence);
        }
        Ok(())
    }
}

/// Host request for raw terminal bytes. The payload is opaque so control
/// characters and paste bytes are preserved exactly; this request contains no
/// launch operation and can only write to an existing attached session.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalInputRequest {
    pub client_id: ClientId,
    pub input_id: InputId,
    pub terminal_id: TerminalId,
    pub context: TerminalInputContext,
    pub bytes: Vec<u8>,
}

impl TerminalInputRequest {
    pub fn validate(&self) -> Result<(), TerminalError> {
        self.context.validate()?;
        if self.bytes.is_empty() || self.bytes.len() > MAX_INPUT_BYTES {
            return Err(TerminalError::BoundExceeded);
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum InputAck {
    Accepted { sequence: u64 },
    Duplicate { sequence: u64 },
    Rejected { reason: InputRejectReason },
}

/// Correlates a host response with the exact idempotency key submitted by the
/// client. Keeping the wire id beside (rather than inside) the admission
/// result preserves the existing service API while allowing concurrent input.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalInputAck {
    pub input_id: InputId,
    pub ack: InputAck,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum InputRejectReason {
    StaleTask,
    StaleAgent,
    StaleResource,
    StaleRuntimeGeneration,
    StaleSession,
    StaleGeneration,
    StaleFocus,
    StaleAction,
    StaleInputSequence,
    ReadOnly,
    Closed,
    Empty,
    BoundExceeded,
    RuntimeForwardFailed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClientInputGrant {
    ReadWrite,
    ReadOnly,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CloseReason {
    ExplicitServiceClose,
    TaskClose,
    HostQuit,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TeardownReport {
    pub terminal_id: TerminalId,
    pub generation: TerminalGeneration,
    pub closed: bool,
    pub explicit: bool,
    pub reason: CloseReason,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ResizeFence {
    pub generation: TerminalGeneration,
    pub client_id: ClientId,
    pub view_sequence: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ViewKind {
    Raw,
    Session,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TerminalViewHandle {
    pub terminal_id: TerminalId,
    pub generation: TerminalGeneration,
    pub kind: ViewKind,
}

/// Opaque host-owned handle. Never a PID or process handle.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct TerminalResourceFence {
    pub terminal_id: TerminalId,
    pub session_id: TerminalSessionId,
    pub resource_id: ResourceId,
    pub generation: TerminalGeneration,
}

/// Exact attached-runtime fence captured at `attach` time.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct AttachmentFence {
    pub resource_id: ResourceId,
    pub generation: TerminalGeneration,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TerminalError {
    NotFound,
    Closed,
    StaleGeneration,
    StaleSession,
    StaleTask,
    StaleFocus,
    SequenceOverflow,
    GenerationOverflow,
    FocusOverflow,
    BoundExceeded,
    InvalidSize,
    InvalidFence,
    CanonicalReaderPoisoned,
    RuntimeIo,
    TeardownFenceMissing,
    TeardownFailed,
    FixtureOnly,
}

impl fmt::Display for TerminalError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let label = match self {
            Self::NotFound => "terminal not found",
            Self::Closed => "terminal is closed",
            Self::StaleGeneration => "stale terminal generation",
            Self::StaleSession => "stale terminal session fence",
            Self::StaleTask => "stale task fence",
            Self::StaleFocus => "stale focus epoch",
            Self::SequenceOverflow => "terminal sequence overflow",
            Self::GenerationOverflow => "terminal generation overflow",
            Self::FocusOverflow => "focus epoch overflow",
            Self::BoundExceeded => "terminal bound exceeded",
            Self::InvalidSize => "invalid terminal size",
            Self::InvalidFence => "invalid terminal fence",
            Self::CanonicalReaderPoisoned => "canonical terminal reader poisoned",
            Self::RuntimeIo => "attached terminal runtime I/O failed",
            Self::TeardownFenceMissing => "managed terminal teardown fence missing",
            Self::TeardownFailed => "managed terminal teardown failed",
            Self::FixtureOnly => "operation requires an attached terminal runtime",
        };
        f.write_str(label)
    }
}

impl std::error::Error for TerminalError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identities_reject_zero_and_overflow_fail_closed() {
        assert!(TerminalGeneration::from_raw(0).is_err());
        assert!(FocusEpoch::from_raw(0).is_err());
        let max_seq = TerminalSequence::from_raw(u64::MAX);
        assert_eq!(max_seq.next(), Err(TerminalError::SequenceOverflow));
        let max_gen = TerminalGeneration::from_raw(u64::MAX).expect("nonzero");
        assert_eq!(max_gen.next(), Err(TerminalError::GenerationOverflow));
    }

    #[test]
    fn terminal_size_and_spec_bounds_are_explicit() {
        assert_eq!(TerminalSize::new(0, 24), Err(TerminalError::InvalidSize));
        assert_eq!(
            TerminalSize::new(80, MAX_TERMINAL_ROWS + 1),
            Err(TerminalError::InvalidSize)
        );
        let size = TerminalSize::new(80, 24).expect("valid size");
        let mut spec = TerminalSpec::new(TerminalSessionId::new(), size).expect("valid spec");
        spec.max_scrollback_rows = MAX_SCROLLBACK_ROWS + 1;
        assert_eq!(spec.validated(), Err(TerminalError::BoundExceeded));
    }

    #[test]
    fn provider_conversation_identity_is_not_a_pty_generation() {
        let generation = TerminalGeneration::initial();
        let next = generation.next().expect("next generation");
        assert_ne!(generation, next);
        assert_ne!(generation.get(), 0);
    }
}
