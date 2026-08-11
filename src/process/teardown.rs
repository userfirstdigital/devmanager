//! Generation-fenced process-tree teardown orchestration.
//!
//! This module owns the coordinator plus the sealed native-terminal adapter
//! that binds exact Job authority to the production PTY lifecycle. It keeps
//! admission ordering, exact-fence validation, escalation, bounded
//! concurrency, and waiter lifetime behind one authority boundary.

use std::collections::VecDeque;
use std::future::Future;
#[cfg(windows)]
use std::io::Write;
use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use futures_util::FutureExt;
#[cfg(windows)]
use portable_pty::MasterPty;
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use tokio::sync::watch;

use crate::domain::id::{OperationId, ResourceId, TaskId};
#[cfg(test)]
use crate::domain::operation::ResourceFence;
use crate::process::identity::ProcessOwner;
use crate::process::registry::{ManagedProcessFence, ManagedProcessState, ProcessRegistry};
#[cfg(test)]
use crate::process::registry::{ProcessDisplayLabel, RegisteredProcess};

#[cfg(windows)]
use crate::process::job::ManagedProcessJob;
#[cfg(windows)]
use crate::process::launcher::{
    ManagedPtyChild, PendingManagedLaunch, RegisteredPendingManagedLaunch,
};

#[cfg(windows)]
use std::ffi::c_void;
#[cfg(windows)]
use std::os::windows::io::AsRawHandle;

#[cfg(windows)]
#[link(name = "kernel32")]
extern "system" {
    fn CancelSynchronousIo(thread: *mut c_void) -> i32;
}

pub const DEFAULT_CONFIGURED_CAPACITY: usize = 4;
pub const DEFAULT_COMPLETED_OPERATION_CAPACITY: usize = 256;
pub const DEFAULT_EXECUTOR_QUEUE_CAPACITY: usize = 256;
pub(crate) const MAX_MANAGED_TERMINAL_PORTS: usize = 64;
/// The largest request accepted by the atomic batch API.  This is deliberately
/// the same size as the fixed executor mailbox: a caller must never be able to
/// turn a rejected request into an unbounded ticket/receipt allocation.
pub const MAX_TEARDOWN_BATCH_ITEMS: usize = DEFAULT_EXECUTOR_QUEUE_CAPACITY;
const MAX_TEARDOWN_RETENTION_ITEMS: usize = DEFAULT_EXECUTOR_QUEUE_CAPACITY;
const MAX_TEARDOWN_ERRORS: usize = 32;

/// A boxed asynchronous operation used by the crate-owned runtime adapters.
///
/// This stays crate-private so callers cannot inject an arbitrary future whose
/// construction or polling can outlive the teardown authority boundary.
pub(crate) type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TeardownScope {
    Task(TaskId),
    Host,
}

impl TeardownScope {
    fn matches_owner(self, owner: ProcessOwner) -> bool {
        match (self, owner) {
            (Self::Task(expected), ProcessOwner::Task(actual)) => expected == actual,
            (Self::Host, ProcessOwner::Host) => true,
            _ => false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TeardownTicketError {
    ScopeOwnerMismatch,
}

/// Exact authority for one cleanup branch.
///
/// The operation identity is the owner of the cleanup, while the scope and
/// action epoch fence admission. The complete process fence is carried
/// through every effect so a reused PID cannot settle a replacement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TeardownTicket {
    operation_id: OperationId,
    scope: TeardownScope,
    action_epoch: u64,
    fence: ManagedProcessFence,
}

/// Opaque authority for the final release of one exact teardown operation.
///
/// This value is minted only by the authoritative process registry after the
/// receiver-owned zero proof and an empty membership query have both matched
/// the exact ticket. It intentionally carries the teardown action epoch here,
/// at settlement time, rather than in the long-lived Job completion stream.
#[derive(Debug, PartialEq, Eq)]
pub struct TeardownReleaseAuthority {
    action_epoch: u64,
    fence: ManagedProcessFence,
    nonce: u64,
}

impl TeardownReleaseAuthority {
    pub(crate) fn from_registry(ticket: &TeardownTicket, nonce: u64) -> Self {
        Self {
            action_epoch: ticket.action_epoch(),
            fence: ticket.fence().clone(),
            nonce,
        }
    }

    pub(crate) fn matches(&self, ticket: &TeardownTicket, nonce: u64) -> bool {
        self.action_epoch == ticket.action_epoch()
            && self.fence == *ticket.fence()
            && self.nonce == nonce
    }
}

impl TeardownTicket {
    pub fn new(
        operation_id: OperationId,
        scope: TeardownScope,
        action_epoch: u64,
        fence: ManagedProcessFence,
    ) -> Result<Self, TeardownTicketError> {
        if !scope.matches_owner(fence.owner()) {
            return Err(TeardownTicketError::ScopeOwnerMismatch);
        }
        Ok(Self {
            operation_id,
            scope,
            action_epoch,
            fence,
        })
    }

    pub fn operation_id(&self) -> OperationId {
        self.operation_id
    }

    pub fn scope(&self) -> TeardownScope {
        self.scope
    }

    pub fn action_epoch(&self) -> u64 {
        self.action_epoch
    }

    pub fn fence(&self) -> &ManagedProcessFence {
        &self.fence
    }

    pub fn resource_id(&self) -> ResourceId {
        self.fence.resource().resource_id
    }

    pub fn owner(&self) -> ProcessOwner {
        self.fence.owner()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdmissionState {
    Open,
    Closing,
    Closed,
}

/// State/fence returned by the admission barrier that won cleanup ownership.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdmissionReceipt {
    scope: TeardownScope,
    state: AdmissionState,
    action_epoch: u64,
    fence: ManagedProcessFence,
}

impl AdmissionReceipt {
    pub fn new(
        scope: TeardownScope,
        state: AdmissionState,
        action_epoch: u64,
        fence: ManagedProcessFence,
    ) -> Self {
        Self {
            scope,
            state,
            action_epoch,
            fence,
        }
    }

    pub fn scope(&self) -> TeardownScope {
        self.scope
    }

    pub fn state(&self) -> AdmissionState {
        self.state
    }

    pub fn action_epoch(&self) -> u64 {
        self.action_epoch
    }

    pub fn fence(&self) -> &ManagedProcessFence {
        &self.fence
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TeardownAdmissionError {
    StaleEpoch { expected: u64, actual: u64 },
    FenceMismatch,
    Timeout { detail: String },
    Other { detail: String },
}

/// The only admission/state operation the coordinator needs from the host.
///
/// Implementations transition the requested scope to Closing and return the
/// exact state/fence they admitted. The coordinator verifies the receipt
/// before it invokes any process or terminal effect.
///
/// The host-issued implementation must complete each call using its bounded
/// admission primitive and the supplied absolute deadline. It must not spawn
/// an unkillable helper thread or perform arbitrary external blocking work.
///
/// External adapters must use the host-issued [`TeardownHostAdapters`] value
/// instead of implementing this private seam.
///
/// ```compile_fail
/// use devmanager::process::teardown::TeardownAdmission;
/// struct ExternalAdmission;
/// impl TeardownAdmission for ExternalAdmission {}
/// ```
#[allow(dead_code)]
pub(crate) trait TeardownAdmission: Send + Sync + 'static {
    fn close_admission(
        &self,
        ticket: &TeardownTicket,
        deadline: CleanupDeadline,
    ) -> Result<AdmissionReceipt, TeardownAdmissionError>;

    /// Atomically closes a complete scope before any branch is scheduled.
    ///
    /// Implementations must validate every ticket and receipt before changing
    /// any admission state. A failure therefore leaves the whole batch open.
    fn close_admission_batch(
        &self,
        tickets: &[TeardownTicket],
        deadline: CleanupDeadline,
    ) -> Result<Vec<AdmissionReceipt>, TeardownAdmissionError>;

    /// Reopens exactly the admissions returned by a successful batch when no
    /// cleanup work could be submitted. Implementations must fence every
    /// ticket/receipt pair and leave the whole batch unchanged on failure.
    fn rollback_admission_batch(
        &self,
        tickets: &[TeardownTicket],
        receipts: &[AdmissionReceipt],
        deadline: CleanupDeadline,
    ) -> Result<(), TeardownAdmissionError>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct TeardownDeadline(u128);

impl TeardownDeadline {
    pub fn new(value: u64) -> Self {
        Self(value.into())
    }

    /// Returns the exact monotonic deadline value supplied by the host clock.
    /// Production clocks use nanoseconds; the public constructor remains
    /// integer-compatible for deterministic in-crate fixtures.
    pub fn value(self) -> u128 {
        self.0
    }
}

/// Clock injection keeps all teardown tests deterministic and sleep-free.
///
/// External callers must not implement this seam: a clock can panic or return
/// an overflowing deadline while the coordinator is settling a waiter. The
/// production host supplies the concrete monotonic clock instead.
///
/// ```compile_fail
/// use std::time::Duration;
/// use devmanager::process::teardown::{TeardownClock, TeardownDeadline};
///
/// struct ExternalClock;
///
/// impl TeardownClock for ExternalClock {
///     fn deadline(&self, _timeout: Duration) -> TeardownDeadline {
///         TeardownDeadline::new(1)
///     }
/// }
/// ```
pub(crate) trait TeardownClock: Send + Sync + 'static {
    fn deadline(&self, timeout: Duration) -> TeardownDeadline;
}

#[derive(Debug, Clone)]
pub struct MonotonicTeardownClock {
    started: Instant,
}

impl Default for MonotonicTeardownClock {
    fn default() -> Self {
        Self {
            started: Instant::now(),
        }
    }
}

impl TeardownClock for MonotonicTeardownClock {
    fn deadline(&self, timeout: Duration) -> TeardownDeadline {
        let now = self.started.elapsed().as_nanos();
        let timeout = timeout.as_nanos();
        TeardownDeadline(now.checked_add(timeout).unwrap_or(u128::MAX))
    }
}

impl MonotonicTeardownClock {
    fn remaining_until(&self, deadline: TeardownDeadline) -> Duration {
        let now = self.started.elapsed().as_nanos();
        duration_from_nanos(deadline.value().saturating_sub(now))
    }
}

/// Converts an exact nanosecond count without truncating sub-millisecond
/// precision.  `Duration` itself has a `u64` seconds field, so values beyond
/// its representable range are explicitly treated as an unbounded duration.
fn duration_from_nanos(nanos: u128) -> Duration {
    let seconds = nanos / 1_000_000_000;
    let nanoseconds = nanos % 1_000_000_000;
    if seconds > u64::MAX as u128 {
        Duration::MAX
    } else {
        Duration::new(seconds as u64, nanoseconds as u32)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TeardownBudgets {
    pub cooperative_grace: Duration,
    pub interrupt_grace: Duration,
    pub termination: Duration,
}

impl Default for TeardownBudgets {
    fn default() -> Self {
        Self {
            cooperative_grace: Duration::from_millis(250),
            interrupt_grace: Duration::from_millis(250),
            termination: Duration::from_millis(500),
        }
    }
}

impl TeardownBudgets {
    pub fn new(
        cooperative_grace: Duration,
        interrupt_grace: Duration,
        termination: Duration,
    ) -> Self {
        Self {
            cooperative_grace,
            interrupt_grace,
            termination,
        }
    }

    fn checked_total(self) -> Result<Duration, String> {
        self.cooperative_grace
            .checked_add(self.interrupt_grace)
            .and_then(|total| total.checked_add(self.termination))
            .ok_or_else(|| "teardown duration budget overflow".to_string())
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct CleanupDeadline {
    effect: TeardownDeadline,
    absolute: Instant,
}

impl CleanupDeadline {
    fn new(clock: &dyn TeardownClock, budgets: TeardownBudgets) -> Result<Self, String> {
        let total = budgets.checked_total()?;
        let effect = std::panic::catch_unwind(AssertUnwindSafe(|| clock.deadline(total)))
            .map_err(|payload| format!("teardown clock panicked: {}", panic_detail(payload)))?;
        let now = std::panic::catch_unwind(AssertUnwindSafe(Instant::now)).map_err(|payload| {
            format!(
                "teardown monotonic clock panicked: {}",
                panic_detail(payload)
            )
        })?;
        let absolute = now
            .checked_add(total)
            .ok_or_else(|| "teardown absolute deadline overflow".to_string())?;
        Ok(Self { effect, absolute })
    }

    fn effect(self) -> TeardownDeadline {
        self.effect
    }

    /// Checks the one absolute monotonic deadline shared by every admission,
    /// executor, effect, and settlement operation.  Callers use this both
    /// immediately before and immediately after crossing an adapter boundary.
    fn check(self, operation: &str) -> Result<Duration, String> {
        checked_remaining_until(self.absolute, operation)
    }

    #[cfg(test)]
    pub(crate) fn remaining(self) -> Result<Duration, String> {
        let now = std::panic::catch_unwind(AssertUnwindSafe(Instant::now)).map_err(|payload| {
            format!(
                "teardown monotonic clock panicked: {}",
                panic_detail(payload)
            )
        })?;
        Ok(self.absolute.saturating_duration_since(now))
    }
}

fn remaining_until(deadline: Instant) -> Result<Duration, String> {
    let now = std::panic::catch_unwind(AssertUnwindSafe(Instant::now)).map_err(|payload| {
        format!(
            "teardown monotonic clock panicked: {}",
            panic_detail(payload)
        )
    })?;
    Ok(deadline.saturating_duration_since(now))
}

/// Crosses an operation boundary only while the shared absolute deadline is
/// still live. Callers that merely need a duration for a condition wait use
/// `remaining_until`; every lookup, persistence, capacity, submission, and
/// effect boundary uses this checked form before and after the operation.
fn checked_remaining_until(deadline: Instant, operation: &str) -> Result<Duration, String> {
    let remaining = remaining_until(deadline)?;
    if remaining.is_zero() {
        Err(format!("{operation} exceeded teardown absolute deadline"))
    } else {
        Ok(remaining)
    }
}

fn bounded_close_admission_batch(
    admission: &dyn TeardownAdmission,
    tickets: &[TeardownTicket],
    deadline: CleanupDeadline,
) -> Result<Vec<AdmissionReceipt>, TeardownAdmissionError> {
    if tickets.len() > MAX_TEARDOWN_BATCH_ITEMS {
        return Err(TeardownAdmissionError::Other {
            detail: "teardown admission batch exceeds the fixed authority bound".to_string(),
        });
    }
    deadline
        .check("admission close")
        .map_err(|detail| TeardownAdmissionError::Timeout { detail })?;
    let receipts = admission.close_admission_batch(tickets, deadline)?;
    deadline
        .check("admission close")
        .map_err(|detail| TeardownAdmissionError::Timeout { detail })?;
    if receipts.len() > MAX_TEARDOWN_BATCH_ITEMS {
        return Err(TeardownAdmissionError::Other {
            detail: "teardown admission returned too many receipts".to_string(),
        });
    }
    Ok(receipts)
}

fn bounded_rollback_admission(
    admission: &dyn TeardownAdmission,
    tickets: &[TeardownTicket],
    receipts: &[AdmissionReceipt],
    deadline: CleanupDeadline,
) -> Result<(), TeardownAdmissionError> {
    if tickets.len() > MAX_TEARDOWN_BATCH_ITEMS || receipts.len() > MAX_TEARDOWN_BATCH_ITEMS {
        return Err(TeardownAdmissionError::Other {
            detail: "teardown admission rollback exceeds the fixed authority bound".to_string(),
        });
    }
    deadline
        .check("admission rollback")
        .map_err(|detail| TeardownAdmissionError::Timeout { detail })?;
    admission.rollback_admission_batch(tickets, receipts, deadline)?;
    deadline
        .check("admission rollback")
        .map(|_| ())
        .map_err(|detail| TeardownAdmissionError::Timeout { detail })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaitStage {
    CooperativeGrace,
    InterruptGrace,
    Termination,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TeardownStage {
    Drain,
    CooperativeClose,
    CooperativeWait,
    InterruptOrSafeClose,
    InterruptWait,
    TerminateTree,
    TerminationWait,
    SettleActiveProcessZero,
    DetachAfterZero,
    ReconcilePorts,
    PersistSettlement,
    ReleaseStoppedExact,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StageResult {
    Completed,
    Failed { detail: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WaitResult {
    Zero,
    TimedOut,
    Failed { detail: String },
}

/// Small adapter surface for terminal/provider close and exact Job operations.
///
/// `terminate_tree` receives no PID. A production adapter must retain its
/// owned Job/completion/PTY handles until the matching completion receiver has
/// issued a registry-owned zero proof, that proof has been settled against the
/// exact fence and an authoritative empty membership query, all post-zero
/// effects have completed, and `release_stopped_exact` is called.
///
/// The native terminal adapter sources zero from the registry completion path
/// and makes `settle_active_process_zero` perform the final exact-fence plus
/// authoritative membership check. The coordinator's `WaitResult::Zero` is
/// deliberately not itself a proof-bearing constructor.
///
/// Every method must construct its future without blocking, and polling must
/// be cancellation-safe at the supplied deadline. Production implementations
/// use bounded Job/IOCP operations; an untrusted blocking operation belongs in
/// a killable helper process or Job rather than a detached Rust thread.
///
/// External adapters must use the host-issued [`TeardownHostAdapters`] value
/// instead of implementing this private seam.
///
/// ```compile_fail
/// use devmanager::process::teardown::TeardownEffects;
/// struct ExternalEffects;
/// impl TeardownEffects for ExternalEffects {}
/// ```
pub(crate) trait TeardownEffects: Send + Sync + 'static {
    fn drain<'a>(&'a self, ticket: &'a TeardownTicket) -> BoxFuture<'a, StageResult>;

    fn cooperative_close<'a>(&'a self, ticket: &'a TeardownTicket) -> BoxFuture<'a, StageResult>;

    fn interrupt_or_safe_close<'a>(
        &'a self,
        ticket: &'a TeardownTicket,
    ) -> BoxFuture<'a, StageResult>;

    fn terminate_tree<'a>(&'a self, ticket: &'a TeardownTicket) -> BoxFuture<'a, StageResult>;

    fn wait_for_zero<'a>(
        &'a self,
        ticket: &'a TeardownTicket,
        stage: WaitStage,
        deadline: TeardownDeadline,
    ) -> BoxFuture<'a, WaitResult>;

    fn settle_active_process_zero<'a>(
        &'a self,
        ticket: &'a TeardownTicket,
    ) -> BoxFuture<'a, StageResult>;

    fn detach_after_zero<'a>(&'a self, ticket: &'a TeardownTicket) -> BoxFuture<'a, StageResult>;

    fn reconcile_ports<'a>(&'a self, ticket: &'a TeardownTicket) -> BoxFuture<'a, StageResult>;

    fn persist_settlement<'a>(&'a self, ticket: &'a TeardownTicket) -> BoxFuture<'a, StageResult>;

    fn residue<'a>(&'a self, ticket: &'a TeardownTicket) -> BoxFuture<'a, Option<ResidueEvidence>>;

    fn release_stopped_exact<'a>(
        &'a self,
        ticket: &'a TeardownTicket,
    ) -> BoxFuture<'a, StageResult>;
}

const MAX_EVIDENCE_TEXT_BYTES: usize = 256;
const MAX_SANITIZE_INPUT_BYTES: usize = 4096;
const MAX_RESIDUE_STAGES: usize = 32;
const UNAVAILABLE_JOB_IDENTITY: &str = "<unavailable: managed Job identity>";
const UNAVAILABLE_ROOT_EXECUTABLE: &str = "<unavailable: root executable>";
const UNAVAILABLE_ROOT_COMMAND: &str = "<unavailable: root command>";
const UNAVAILABLE_LIFECYCLE_EVENT: &str = "<unavailable: lifecycle event>";

fn sanitize_text(value: &str) -> String {
    let mut input_end = value.len().min(MAX_SANITIZE_INPUT_BYTES);
    while input_end > 0 && !value.is_char_boundary(input_end) {
        input_end -= 1;
    }
    let bounded = &value[..input_end];
    let normalized: String = bounded
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect();
    let parts: Vec<&str> = normalized.split_whitespace().collect();
    let mut redacted = Vec::with_capacity(parts.len());
    let mut index = 0usize;
    while index < parts.len() {
        let part = parts[index];
        if let Some((key, value)) = part.split_once('=') {
            if is_sensitive_key(key) {
                redacted.push(format!("{key}=<redacted>"));
                index += 1;
                if key
                    .trim_start_matches('-')
                    .trim_end_matches(':')
                    .eq_ignore_ascii_case("authorization")
                    && value.eq_ignore_ascii_case("bearer")
                    && index < parts.len()
                {
                    index += 1;
                }
            } else {
                redacted.push(part.to_string());
                index += 1;
            }
            continue;
        }

        if let Some((key, value)) = part.split_once(':') {
            if is_sensitive_key(key) {
                let separator = if key
                    .trim_start_matches('-')
                    .eq_ignore_ascii_case("authorization")
                {
                    ": "
                } else {
                    ":"
                };
                redacted.push(format!("{key}{separator}<redacted>"));
                index += 1;
                if key
                    .trim_start_matches('-')
                    .eq_ignore_ascii_case("authorization")
                    && value.eq_ignore_ascii_case("bearer")
                    && index < parts.len()
                {
                    index += 1;
                } else if key
                    .trim_start_matches('-')
                    .eq_ignore_ascii_case("authorization")
                    && value.is_empty()
                    && index < parts.len()
                    && parts[index].eq_ignore_ascii_case("bearer")
                {
                    index += 1;
                    if index < parts.len() {
                        index += 1;
                    }
                }
                continue;
            }
        }

        let key = part.trim_start_matches('-');
        if is_sensitive_key(key) {
            redacted.push(part.to_string());
            redacted.push("<redacted>".to_string());
            index += 1;
            if index < parts.len()
                && key
                    .trim_end_matches(':')
                    .eq_ignore_ascii_case("authorization")
            {
                if parts[index].eq_ignore_ascii_case("bearer") {
                    index += 1;
                    if index < parts.len() {
                        index += 1;
                    }
                } else if parts[index] != "<redacted>" {
                    index += 1;
                }
            } else if index < parts.len() && parts[index] != "<redacted>" {
                index += 1;
            } else if index < parts.len() {
                index += 1;
            }
            continue;
        }

        redacted.push(part.to_string());
        index += 1;
    }
    let redacted = redacted.join(" ");
    truncate_utf8(redacted, MAX_EVIDENCE_TEXT_BYTES)
}

fn is_sensitive_key(key: &str) -> bool {
    let lower = key
        .trim_start_matches('-')
        .trim_end_matches(':')
        .to_ascii_lowercase();
    matches!(
        lower.as_str(),
        "token"
            | "password"
            | "passwd"
            | "secret"
            | "api_key"
            | "api-key"
            | "apikey"
            | "authorization"
    ) || lower.ends_with("_token")
        || lower.ends_with("_password")
        || lower.ends_with("_secret")
}

fn sanitized_or_marker(value: &str, marker: &str) -> String {
    let sanitized = sanitize_text(value);
    if sanitized.is_empty() {
        marker.to_string()
    } else {
        sanitized
    }
}

fn truncate_utf8(mut value: String, max_bytes: usize) -> String {
    if value.len() > max_bytes {
        let mut end = max_bytes;
        while !value.is_char_boundary(end) {
            end -= 1;
        }
        value.truncate(end);
    }
    value
}

/// Bounded, control-free evidence retained when a branch cannot be closed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResidueEvidence {
    job_name: String,
    pid: u32,
    creation_time_100ns: u64,
    executable: String,
    command_label: String,
    last_lifecycle_event: String,
    attempted_stages: Vec<TeardownStage>,
}

impl ResidueEvidence {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        job_name: impl AsRef<str>,
        pid: u32,
        creation_time_100ns: u64,
        executable: impl AsRef<str>,
        command_label: impl AsRef<str>,
        last_lifecycle_event: impl AsRef<str>,
        attempted_stages: Vec<TeardownStage>,
    ) -> Self {
        Self {
            job_name: sanitized_or_marker(job_name.as_ref(), UNAVAILABLE_JOB_IDENTITY),
            pid,
            creation_time_100ns,
            executable: sanitized_or_marker(executable.as_ref(), UNAVAILABLE_ROOT_EXECUTABLE),
            command_label: sanitized_or_marker(command_label.as_ref(), UNAVAILABLE_ROOT_COMMAND),
            last_lifecycle_event: sanitized_or_marker(
                last_lifecycle_event.as_ref(),
                UNAVAILABLE_LIFECYCLE_EVENT,
            ),
            attempted_stages: attempted_stages
                .into_iter()
                .take(MAX_RESIDUE_STAGES)
                .collect(),
        }
    }

    pub fn job_name(&self) -> &str {
        &self.job_name
    }

    pub fn pid(&self) -> u32 {
        self.pid
    }

    pub fn creation_time_100ns(&self) -> u64 {
        self.creation_time_100ns
    }

    pub fn executable(&self) -> &str {
        &self.executable
    }

    pub fn command_label(&self) -> &str {
        &self.command_label
    }

    pub fn last_lifecycle_event(&self) -> &str {
        &self.last_lifecycle_event
    }

    pub fn attempted_stages(&self) -> &[TeardownStage] {
        &self.attempted_stages
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TeardownOutcome {
    Closed,
    Leaked,
    CleanupFailed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TeardownReport {
    ticket: TeardownTicket,
    outcome: TeardownOutcome,
    attempted_stages: Vec<TeardownStage>,
    errors: Vec<String>,
    residue: Option<ResidueEvidence>,
}

impl TeardownReport {
    pub fn operation_id(&self) -> OperationId {
        self.ticket.operation_id()
    }

    pub fn scope(&self) -> TeardownScope {
        self.ticket.scope()
    }

    pub fn action_epoch(&self) -> u64 {
        self.ticket.action_epoch()
    }

    pub fn fence(&self) -> &ManagedProcessFence {
        self.ticket.fence()
    }

    pub fn resource_id(&self) -> ResourceId {
        self.ticket.resource_id()
    }

    pub fn outcome(&self) -> TeardownOutcome {
        self.outcome
    }

    pub fn attempted_stages(&self) -> &[TeardownStage] {
        &self.attempted_stages
    }

    pub fn errors(&self) -> &[String] {
        &self.errors
    }

    pub fn residue(&self) -> Option<&ResidueEvidence> {
        self.residue.as_ref()
    }

    fn with_handoff_error(mut self, detail: String) -> Self {
        push_bounded_error(&mut self.errors, detail);
        self.outcome = TeardownOutcome::CleanupFailed;
        let residue = self.residue.take();
        self.residue = required_residue(
            &self.ticket,
            &self.attempted_stages,
            &self.errors,
            self.outcome,
            residue,
        );
        self
    }
}

/// Canonical identity used for retry and host-to-task joins.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TeardownCompletionKey {
    action_epoch: u64,
    fence: ManagedProcessFence,
}

impl TeardownCompletionKey {
    pub fn new(action_epoch: u64, fence: ManagedProcessFence) -> Self {
        Self {
            action_epoch,
            fence,
        }
    }

    pub fn action_epoch(&self) -> u64 {
        self.action_epoch
    }

    pub fn fence(&self) -> &ManagedProcessFence {
        &self.fence
    }
}

/// The concrete durable-idempotency boundary for completed teardown
/// operations.
///
/// The coordinator never accepts an arbitrary caller-provided blocking trait
/// here. Both lookup and persistence are invoked only by its fixed-capacity
/// dispatch boundary. Production terminal authorities use the bounded SQLite
/// journal; deterministic tests may use the in-memory implementation.
#[derive(Debug, Clone, Default)]
pub struct TeardownCompletionStore {
    inner: Arc<CompletionStoreInner>,
}

/// Host-issued set of bounded teardown operations.
///
/// The contained seams are deliberately private: only the host implementation
/// in this crate can mint this value. External callers receive a concrete
/// adapter handle and cannot install an arbitrary blocking admission, clock,
/// or effect implementation.
#[derive(Clone)]
struct TeardownHostAdapters {
    admission: Arc<dyn TeardownAdmission>,
    effects: Arc<dyn TeardownEffects>,
    clock: Arc<dyn TeardownClock>,
}

impl std::fmt::Debug for TeardownHostAdapters {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("TeardownHostAdapters")
            .finish_non_exhaustive()
    }
}

impl TeardownHostAdapters {
    /// The production adapter constructor accepts only the concrete terminal
    /// host implementation declared in this module. This is stronger than a
    /// marker trait: no other crate module can install a synchronously
    /// blocking future constructor in the shutdown worker boundary.
    #[cfg(windows)]
    fn terminal(
        admission: Arc<TerminalTeardownAdmission>,
        effects: Arc<TerminalTeardownEffects>,
        clock: Arc<MonotonicTeardownClock>,
    ) -> Self {
        Self {
            admission: admission as Arc<dyn TeardownAdmission>,
            effects: effects as Arc<dyn TeardownEffects>,
            clock: clock as Arc<dyn TeardownClock>,
        }
    }
}

/// Host-owned teardown bridge used by the native terminal session on
/// Windows.  The managed Job is moved into the process registry at creation;
/// terminal close/restart/drop can therefore never fall back to PID- or
/// `ChildKiller`-selected termination once a session is live.
#[cfg(windows)]
#[derive(Default)]
pub(crate) struct ManagedTerminalActorHandles {
    pub(crate) reader: Option<JoinHandle<()>>,
    pub(crate) waiter: Option<JoinHandle<()>>,
}

/// Concrete native-terminal resources detached only after receiver-owned
/// ACTIVE_PROCESS_ZERO.  The slots are created before the suspended process
/// is resumed, so every setup-failure path is covered by the same adapter.
#[cfg(windows)]
pub(crate) struct ManagedTerminalIo {
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    master: Arc<Mutex<Option<Box<dyn MasterPty + Send>>>>,
    actors: Arc<Mutex<ManagedTerminalActorHandles>>,
    detached: AtomicBool,
}

#[cfg(windows)]
impl ManagedTerminalIo {
    pub(crate) fn new(
        writer: Arc<Mutex<Box<dyn Write + Send>>>,
        master: Arc<Mutex<Option<Box<dyn MasterPty + Send>>>>,
        actors: Arc<Mutex<ManagedTerminalActorHandles>>,
    ) -> Arc<Self> {
        Arc::new(Self {
            writer,
            master,
            actors,
            detached: AtomicBool::new(false),
        })
    }

    #[cfg(test)]
    fn empty() -> Arc<Self> {
        Self::new(
            Arc::new(Mutex::new(Box::new(std::io::sink()))),
            Arc::new(Mutex::new(None)),
            Arc::new(Mutex::new(ManagedTerminalActorHandles::default())),
        )
    }

    async fn detach_after_zero(&self) -> Result<(), String> {
        if self.detached.load(Ordering::Acquire) {
            return Ok(());
        }
        {
            let mut writer = self
                .writer
                .lock()
                .map_err(|_| "terminal PTY writer poisoned".to_string())?;
            let old = std::mem::replace(&mut *writer, Box::new(std::io::sink()));
            drop(old);
        }
        self.master
            .lock()
            .map_err(|_| "terminal PTY master poisoned".to_string())?
            .take();

        let current = thread::current().id();
        loop {
            let all_finished = {
                let actors = self
                    .actors
                    .lock()
                    .map_err(|_| "terminal actor handles poisoned".to_string())?;
                for handle in [&actors.reader, &actors.waiter].into_iter().flatten() {
                    if handle.thread().id() == current {
                        return Err(
                            "terminal teardown actor attempted to synchronously join itself"
                                .to_string(),
                        );
                    }
                    if !handle.is_finished() {
                        // SAFETY: JoinHandle owns a live OS thread handle and
                        // cancellation targets synchronous I/O issued only by
                        // that exact actor. The subsequent loop still requires
                        // the actor to acknowledge cancellation before join.
                        unsafe {
                            let _ = CancelSynchronousIo(handle.as_raw_handle());
                        }
                    }
                }
                let finished = [&actors.reader, &actors.waiter]
                    .into_iter()
                    .flatten()
                    .all(JoinHandle::is_finished);
                finished
            };
            if all_finished {
                break;
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }

        let mut actors = self
            .actors
            .lock()
            .map_err(|_| "terminal actor handles poisoned".to_string())?;
        if let Some(reader) = actors.reader.take() {
            reader
                .join()
                .map_err(|_| "terminal reader actor panicked".to_string())?;
        }
        if let Some(waiter) = actors.waiter.take() {
            waiter
                .join()
                .map_err(|_| "terminal wait actor panicked".to_string())?;
        }
        self.detached.store(true, Ordering::Release);
        Ok(())
    }
}

#[cfg(windows)]
fn validate_terminal_teardown_inputs(
    session_id: &str,
    action_epoch: u64,
    ports: &[u16],
) -> Result<Vec<u16>, String> {
    if action_epoch == 0 {
        return Err("terminal teardown action epoch must be greater than zero".to_string());
    }
    if session_id.trim().is_empty() || session_id.len() > 256 {
        return Err("terminal teardown session identity is invalid".to_string());
    }
    if ports.len() > MAX_MANAGED_TERMINAL_PORTS {
        return Err(format!(
            "terminal teardown port set exceeds {MAX_MANAGED_TERMINAL_PORTS} entries"
        ));
    }
    let mut normalized = ports.to_vec();
    normalized.sort_unstable();
    normalized.dedup();
    Ok(normalized)
}

#[cfg(windows)]
pub(crate) struct ManagedTerminalTeardown {
    coordinator: Arc<TeardownCoordinator>,
    ticket: TeardownTicket,
    waiter: Mutex<Option<TeardownWaiter>>,
    report: Mutex<Option<TeardownReport>>,
    state: Arc<Mutex<ManagedTerminalTeardownState>>,
    /// Armed immediately before the registered suspended root is resumed.
    /// Once armed, dropping this owner without a Closed report is a process
    /// safety invariant violation: returning would abandon Job/actor authority.
    armed: AtomicBool,
}

#[cfg(windows)]
struct ManagedTerminalTeardownState {
    registry: ProcessRegistry<ManagedProcessJob>,
    fence: ManagedProcessFence,
    release_authority: Option<TeardownReleaseAuthority>,
    session_id: String,
    ports: Vec<u16>,
    io: Arc<ManagedTerminalIo>,
    settlement_persisted: bool,
}

#[cfg(windows)]
impl ManagedTerminalTeardown {
    /// Completes the one safe production PTY handoff: the suspended root is
    /// registered in the exact Job-backed registry and only then resumed.
    /// The returned child and teardown adapter share that same registry/fence;
    /// there is no post-resume PID attachment window.
    pub(crate) fn from_pending_launch(
        pending: PendingManagedLaunch,
        operation_id: OperationId,
        action_epoch: u64,
        completion_store: TeardownCompletionStore,
        session_id: String,
        ports: Vec<u16>,
        io: Arc<ManagedTerminalIo>,
    ) -> Result<(Arc<Self>, ManagedPtyChild), String> {
        // All fallible host input is checked while the root is still only a
        // PendingChild. Once it is registered, constructing the teardown
        // authority is infallible and the next operation is the one-way
        // resume handoff.
        let ports = validate_terminal_teardown_inputs(&session_id, action_epoch, &ports)?;
        let mut registry = ProcessRegistry::new();
        let pending = pending
            .register_suspended(&mut registry)
            .map_err(|error| error.to_string())?;
        let fence = pending.fence().clone();
        let teardown = Self::from_registered(
            registry,
            fence,
            operation_id,
            action_epoch,
            completion_store,
            session_id,
            ports,
            io,
        );
        teardown.armed.store(true, Ordering::Release);
        let child = match Self::resume_registered_launch(&teardown, pending) {
            Ok(child) => child,
            Err(error) => {
                // `resume` rolls the exact Starting registry entry back before
                // returning. This root never became live, so disarm the drop
                // guard and synchronously join the unused coordinator.
                teardown.armed.store(false, Ordering::Release);
                teardown.coordinator.shutdown();
                return Err(error);
            }
        };
        Ok((teardown, child))
    }

    fn resume_registered_launch(
        teardown: &Arc<Self>,
        pending: RegisteredPendingManagedLaunch,
    ) -> Result<ManagedPtyChild, String> {
        let mut state = teardown
            .state
            .lock()
            .map_err(|_| "terminal process registry poisoned before resume".to_string())?;
        pending
            .resume(&mut state.registry)
            .map_err(|error| error.to_string())
    }

    fn from_registered(
        registry: ProcessRegistry<ManagedProcessJob>,
        fence: ManagedProcessFence,
        operation_id: OperationId,
        action_epoch: u64,
        completion_store: TeardownCompletionStore,
        session_id: String,
        ports: Vec<u16>,
        io: Arc<ManagedTerminalIo>,
    ) -> Arc<Self> {
        let scope = match fence.owner() {
            ProcessOwner::Task(task_id) => TeardownScope::Task(task_id),
            ProcessOwner::Host => TeardownScope::Host,
        };
        let ticket = TeardownTicket::new(operation_id, scope, action_epoch, fence.clone())
            .expect("terminal teardown scope is derived from its exact Job owner");
        let state = Arc::new(Mutex::new(ManagedTerminalTeardownState {
            registry,
            fence,
            release_authority: None,
            session_id,
            ports,
            io,
            settlement_persisted: false,
        }));
        let admission = Arc::new(TerminalTeardownAdmission::new(
            ticket.fence().clone(),
            ticket.scope(),
            ticket.action_epoch(),
        ));
        let clock = Arc::new(MonotonicTeardownClock::default());
        let effects = Arc::new(TerminalTeardownEffects {
            state: Arc::clone(&state),
            clock: Arc::clone(&clock),
        });
        let adapters = TeardownHostAdapters::terminal(admission, effects, Arc::clone(&clock));
        let coordinator = Arc::new(TeardownCoordinator::from_host(
            adapters,
            1,
            TeardownBudgets::default(),
            DEFAULT_COMPLETED_OPERATION_CAPACITY,
            completion_store,
        ));
        Arc::new(Self {
            coordinator,
            ticket,
            waiter: Mutex::new(None),
            report: Mutex::new(None),
            state,
            armed: AtomicBool::new(false),
        })
    }

    #[cfg(test)]
    pub(crate) fn new(
        pid: u32,
        job: ManagedProcessJob,
        display_label: impl Into<String>,
    ) -> Result<Arc<Self>, String> {
        let root = job.inspect_process(pid)?;
        let root_identity = root.identity().clone();
        let resource = ResourceFence::new(ResourceId::new(), 1);
        let label = ProcessDisplayLabel::new(display_label.into())
            .map_err(|error| format!("invalid terminal process label: {error}"))?;
        let mut registry = ProcessRegistry::new();
        let process =
            RegisteredProcess::new(resource, ProcessOwner::Host, root_identity, label, job);
        let fence = registry
            .register(process)
            .map_err(|failure| failure.to_string())?;
        let teardown = Self::from_registered(
            registry,
            fence,
            OperationId::new(),
            1,
            TeardownCompletionStore::default(),
            "teardown-windows-test".to_string(),
            Vec::new(),
            ManagedTerminalIo::empty(),
        );
        teardown.armed.store(true, Ordering::Release);
        Ok(teardown)
    }

    fn waiter(&self) -> Result<TeardownWaiter, String> {
        let mut slot = self
            .waiter
            .lock()
            .map_err(|_| "terminal teardown waiter poisoned".to_string())?;
        if let Some(waiter) = slot.as_ref() {
            return Ok(waiter.clone());
        }
        let waiter = self
            .coordinator
            .request(self.ticket.clone())
            .map_err(|error| format!("terminal teardown admission failed: {error:?}"))?;
        *slot = Some(waiter.clone());
        Ok(waiter)
    }

    /// Starts exact teardown without blocking the terminal reader/wait actor.
    /// The actor can then exit and be joined by the host's synchronous close.
    pub(crate) fn request_close(&self) -> Result<(), String> {
        self.waiter().map(|_| ())
    }

    pub(crate) fn matches_fence(&self, expected: &ManagedProcessFence) -> bool {
        self.ticket.fence() == expected
    }

    pub(crate) fn close(&self) -> Result<TeardownReport, String> {
        let waiter = self.waiter()?;
        if let Some(report) = self
            .report
            .lock()
            .map_err(|_| "terminal teardown report poisoned".to_string())?
            .clone()
        {
            return Ok(report);
        }

        // `TerminalSession::close` is synchronous and is also called by its
        // Drop implementation. The coordinator worker owns the async runtime;
        // this caller waits on the cell's bounded condition-variable bridge,
        // so closing a session never creates a second runtime thread whose
        // join could outlive the teardown authority.
        let wait_budget = TeardownBudgets::default()
            .checked_total()
            .map_err(|error| format!("terminal teardown wait budget: {error}"))?;
        let report = match waiter.wait_blocking(wait_budget) {
            Ok(report) => report,
            Err(wait_error) => {
                // A host adapter that violates its bounded contract must not
                // strand the process or a worker. Shutdown requests
                // cancellation and joins the fixed executor; the waiter then
                // has a deterministic failure report to return.
                self.coordinator.shutdown();
                waiter
                    .wait_blocking(Duration::from_millis(100))
                    .map_err(|settle_error| {
                        format!(
                            "terminal teardown wait failed: {wait_error}; settlement failed: {settle_error}"
                        )
                    })?
            }
        };
        *self
            .report
            .lock()
            .map_err(|_| "terminal teardown report poisoned".to_string())? = Some(report.clone());
        Ok(report)
    }

    #[cfg(test)]
    pub(crate) fn active_process_ids(&self) -> Result<Vec<u32>, String> {
        self.managed_process_snapshot()
            .map(|(_, active_process_ids)| active_process_ids)
    }

    /// Returns the exact generation/identity fence together with a membership
    /// snapshot from the same authoritative Job-backed registry entry.
    /// Callers may use this to validate a diagnostic PID selection, but the
    /// fence remains the only authority accepted by teardown.
    pub(crate) fn managed_process_snapshot(
        &self,
    ) -> Result<(ManagedProcessFence, Vec<u32>), String> {
        let state = self
            .state
            .lock()
            .map_err(|_| "terminal process registry poisoned".to_string())?;
        let active_process_ids = state
            .registry
            .current(state.fence.resource().resource_id)
            .map(|process| process.job().active_process_ids())
            .unwrap_or_else(|| Ok(Vec::new()))?;
        Ok((state.fence.clone(), active_process_ids))
    }
}

#[cfg(windows)]
impl Drop for ManagedTerminalTeardown {
    fn drop(&mut self) {
        if self.armed.load(Ordering::Acquire) {
            let already_closed = self
                .report
                .lock()
                .ok()
                .and_then(|report| report.clone())
                .is_some_and(|report| report.outcome() == TeardownOutcome::Closed);
            if !already_closed {
                match self.close() {
                    Ok(report) if report.outcome() == TeardownOutcome::Closed => {}
                    Ok(report) => {
                        eprintln!(
                            "managed terminal authority dropped before exact close: {:?}",
                            report.errors()
                        );
                        std::process::abort();
                    }
                    Err(error) => {
                        eprintln!("managed terminal authority drop failed exact close: {error}");
                        std::process::abort();
                    }
                }
            }
        }
        self.coordinator.shutdown();
    }
}

#[cfg(windows)]
struct TerminalTeardownAdmission {
    fence: ManagedProcessFence,
    scope: TeardownScope,
    action_epoch: u64,
    state: Mutex<AdmissionState>,
}

#[cfg(windows)]
impl TerminalTeardownAdmission {
    fn new(fence: ManagedProcessFence, scope: TeardownScope, action_epoch: u64) -> Self {
        Self {
            fence,
            scope,
            action_epoch,
            state: Mutex::new(AdmissionState::Open),
        }
    }

    fn validate_ticket(&self, ticket: &TeardownTicket) -> Result<(), TeardownAdmissionError> {
        if ticket.scope() != self.scope || ticket.action_epoch() != self.action_epoch {
            return Err(TeardownAdmissionError::StaleEpoch {
                expected: self.action_epoch,
                actual: ticket.action_epoch(),
            });
        }
        if ticket.fence() != &self.fence {
            return Err(TeardownAdmissionError::FenceMismatch);
        }
        Ok(())
    }
}

#[cfg(windows)]
impl TeardownAdmission for TerminalTeardownAdmission {
    fn close_admission(
        &self,
        ticket: &TeardownTicket,
        deadline: CleanupDeadline,
    ) -> Result<AdmissionReceipt, TeardownAdmissionError> {
        self.close_admission_batch(std::slice::from_ref(ticket), deadline)?
            .into_iter()
            .next()
            .ok_or_else(|| TeardownAdmissionError::Other {
                detail: "terminal admission returned no receipt".to_string(),
            })
    }

    fn close_admission_batch(
        &self,
        tickets: &[TeardownTicket],
        _deadline: CleanupDeadline,
    ) -> Result<Vec<AdmissionReceipt>, TeardownAdmissionError> {
        for ticket in tickets {
            self.validate_ticket(ticket)?;
        }
        let mut state = self
            .state
            .lock()
            .map_err(|_| TeardownAdmissionError::Other {
                detail: "terminal admission state poisoned".to_string(),
            })?;
        if *state == AdmissionState::Closed {
            return Err(TeardownAdmissionError::Other {
                detail: "terminal admission is already closed".to_string(),
            });
        }
        *state = AdmissionState::Closing;
        Ok(tickets
            .iter()
            .map(|ticket| {
                AdmissionReceipt::new(
                    ticket.scope(),
                    AdmissionState::Closing,
                    ticket.action_epoch(),
                    self.fence.clone(),
                )
            })
            .collect())
    }

    fn rollback_admission_batch(
        &self,
        tickets: &[TeardownTicket],
        receipts: &[AdmissionReceipt],
        _deadline: CleanupDeadline,
    ) -> Result<(), TeardownAdmissionError> {
        if tickets.len() != receipts.len() {
            return Err(TeardownAdmissionError::Other {
                detail: "terminal admission rollback receipt count mismatch".to_string(),
            });
        }
        for (ticket, receipt) in tickets.iter().zip(receipts) {
            self.validate_ticket(ticket)?;
            if receipt.state() != AdmissionState::Closing
                || receipt.scope() != ticket.scope()
                || receipt.action_epoch() != ticket.action_epoch()
                || receipt.fence() != ticket.fence()
            {
                return Err(TeardownAdmissionError::FenceMismatch);
            }
        }
        let mut state = self
            .state
            .lock()
            .map_err(|_| TeardownAdmissionError::Other {
                detail: "terminal admission state poisoned".to_string(),
            })?;
        if *state != AdmissionState::Closing {
            return Err(TeardownAdmissionError::Other {
                detail: "terminal admission was not closing during rollback".to_string(),
            });
        }
        *state = AdmissionState::Open;
        Ok(())
    }
}

#[cfg(windows)]
struct TerminalTeardownEffects {
    state: Arc<Mutex<ManagedTerminalTeardownState>>,
    clock: Arc<MonotonicTeardownClock>,
}

#[cfg(windows)]
impl TerminalTeardownEffects {
    fn validate_ticket(
        state: &ManagedTerminalTeardownState,
        ticket: &TeardownTicket,
    ) -> Result<(), String> {
        if ticket.fence() != &state.fence {
            Err("terminal teardown ticket fence no longer matches registry".to_string())
        } else {
            Ok(())
        }
    }

    fn zero_state(
        state: &mut ManagedTerminalTeardownState,
        ticket: &TeardownTicket,
    ) -> Result<bool, String> {
        Self::validate_ticket(state, ticket)?;
        state.registry.drain_job_completions(ticket.resource_id());
        state
            .registry
            .reconcile_membership(ticket.resource_id())
            .map_err(|error| error.to_string())?;
        Ok(state
            .registry
            .current(ticket.resource_id())
            .is_some_and(|process| {
                process.state() == ManagedProcessState::Stopped && process.member_count() == 0
            }))
    }
}

#[cfg(windows)]
impl TeardownEffects for TerminalTeardownEffects {
    fn drain<'a>(&'a self, ticket: &'a TeardownTicket) -> BoxFuture<'a, StageResult> {
        let state = Arc::clone(&self.state);
        let ticket = ticket.clone();
        Box::pin(async move {
            let mut state = match state.lock() {
                Ok(state) => state,
                Err(_) => {
                    return StageResult::Failed {
                        detail: "terminal process registry poisoned".to_string(),
                    }
                }
            };
            match Self::validate_ticket(&state, &ticket) {
                Ok(()) => {
                    state.registry.drain_job_completions(ticket.resource_id());
                    StageResult::Completed
                }
                Err(detail) => StageResult::Failed { detail },
            }
        })
    }

    fn cooperative_close<'a>(&'a self, _ticket: &'a TeardownTicket) -> BoxFuture<'a, StageResult> {
        Box::pin(async { StageResult::Completed })
    }

    fn interrupt_or_safe_close<'a>(
        &'a self,
        _ticket: &'a TeardownTicket,
    ) -> BoxFuture<'a, StageResult> {
        Box::pin(async { StageResult::Completed })
    }

    fn terminate_tree<'a>(&'a self, ticket: &'a TeardownTicket) -> BoxFuture<'a, StageResult> {
        let state = Arc::clone(&self.state);
        let ticket = ticket.clone();
        Box::pin(async move {
            let state = match state.lock() {
                Ok(state) => state,
                Err(_) => {
                    return StageResult::Failed {
                        detail: "terminal process registry poisoned".to_string(),
                    }
                }
            };
            if let Err(detail) = Self::validate_ticket(&state, &ticket) {
                return StageResult::Failed { detail };
            }
            match state
                .registry
                .current(ticket.resource_id())
                .map(|process| process.job().terminate_tree())
                .unwrap_or_else(|| Err("terminal process registry entry is missing".to_string()))
            {
                Ok(()) => StageResult::Completed,
                Err(detail) => StageResult::Failed { detail },
            }
        })
    }

    fn wait_for_zero<'a>(
        &'a self,
        ticket: &'a TeardownTicket,
        stage: WaitStage,
        deadline: TeardownDeadline,
    ) -> BoxFuture<'a, WaitResult> {
        if stage != WaitStage::Termination {
            return Box::pin(async { WaitResult::TimedOut });
        }
        let state = Arc::clone(&self.state);
        let clock = Arc::clone(&self.clock);
        let ticket = ticket.clone();
        Box::pin(async move {
            loop {
                let zero = match state.lock() {
                    Ok(mut state) => match Self::zero_state(&mut state, &ticket) {
                        Ok(zero) => zero,
                        Err(detail) => return WaitResult::Failed { detail },
                    },
                    Err(_) => {
                        return WaitResult::Failed {
                            detail: "terminal process registry poisoned".to_string(),
                        }
                    }
                };
                if zero {
                    return WaitResult::Zero;
                }
                let remaining = clock.remaining_until(deadline);
                if remaining.is_zero() {
                    return WaitResult::TimedOut;
                }
                tokio::time::sleep(remaining.min(Duration::from_millis(5))).await;
            }
        })
    }

    fn settle_active_process_zero<'a>(
        &'a self,
        ticket: &'a TeardownTicket,
    ) -> BoxFuture<'a, StageResult> {
        let state = Arc::clone(&self.state);
        let ticket = ticket.clone();
        Box::pin(async move {
            let mut state = match state.lock() {
                Ok(state) => state,
                Err(_) => {
                    return StageResult::Failed {
                        detail: "terminal process registry poisoned".to_string(),
                    }
                }
            };
            if let Err(detail) = Self::validate_ticket(&state, &ticket) {
                return StageResult::Failed { detail };
            }
            let proof = match state
                .registry
                .active_process_zero_proof_exact(ticket.fence())
            {
                Ok(proof) => proof,
                Err(error) => {
                    return StageResult::Failed {
                        detail: error.to_string(),
                    }
                }
            };
            match state
                .registry
                .mint_teardown_release_authority_exact(&ticket, proof)
            {
                Ok(authority) => {
                    state.release_authority = Some(authority);
                    StageResult::Completed
                }
                Err(error) => StageResult::Failed {
                    detail: error.to_string(),
                },
            }
        })
    }

    fn detach_after_zero<'a>(&'a self, ticket: &'a TeardownTicket) -> BoxFuture<'a, StageResult> {
        let state = Arc::clone(&self.state);
        let ticket = ticket.clone();
        Box::pin(async move {
            let io = {
                let state = match state.lock() {
                    Ok(state) => state,
                    Err(_) => {
                        return StageResult::Failed {
                            detail: "terminal process registry poisoned".to_string(),
                        }
                    }
                };
                if let Err(detail) = Self::validate_ticket(&state, &ticket) {
                    return StageResult::Failed { detail };
                }
                Arc::clone(&state.io)
            };
            match io.detach_after_zero().await {
                Ok(()) => StageResult::Completed,
                Err(detail) => StageResult::Failed { detail },
            }
        })
    }

    fn reconcile_ports<'a>(&'a self, ticket: &'a TeardownTicket) -> BoxFuture<'a, StageResult> {
        let state = Arc::clone(&self.state);
        let ticket = ticket.clone();
        Box::pin(async move {
            let state = match state.lock() {
                Ok(state) => state,
                Err(_) => {
                    return StageResult::Failed {
                        detail: "terminal process registry poisoned".to_string(),
                    }
                }
            };
            if let Err(detail) = Self::validate_ticket(&state, &ticket) {
                return StageResult::Failed { detail };
            }
            if state.ports.is_empty() {
                return StageResult::Completed;
            }
            let listeners =
                match crate::services::platform_service::snapshot_listener_pids(&state.ports) {
                    Ok(listeners) => listeners,
                    Err(detail) => return StageResult::Failed { detail },
                };
            let Some(process) = state.registry.current(ticket.resource_id()) else {
                return StageResult::Failed {
                    detail: "terminal process registry entry is missing".to_string(),
                };
            };
            for (port, pid) in listeners {
                if process.job().inspect_process(pid).is_ok() {
                    return StageResult::Failed {
                        detail: format!(
                            "managed listener on port {port} remained after ACTIVE_PROCESS_ZERO"
                        ),
                    };
                }
            }
            // A listener that is not an exact member is external. It remains
            // untouched and the normal background port inventory will expose
            // it as externally occupied (blue).
            StageResult::Completed
        })
    }

    fn persist_settlement<'a>(&'a self, ticket: &'a TeardownTicket) -> BoxFuture<'a, StageResult> {
        let state = Arc::clone(&self.state);
        let ticket = ticket.clone();
        Box::pin(async move {
            let (session_id, root_pid, already_persisted) = {
                let state = match state.lock() {
                    Ok(state) => state,
                    Err(_) => {
                        return StageResult::Failed {
                            detail: "terminal process registry poisoned".to_string(),
                        }
                    }
                };
                if let Err(detail) = Self::validate_ticket(&state, &ticket) {
                    return StageResult::Failed { detail };
                }
                (
                    state.session_id.clone(),
                    state.fence.root().id().pid(),
                    state.settlement_persisted,
                )
            };
            if !already_persisted {
                if let Err(detail) = crate::services::pid_file::release_session_root(
                    &session_id,
                    root_pid,
                    Vec::new(),
                ) {
                    return StageResult::Failed { detail };
                }
                let mut state = match state.lock() {
                    Ok(state) => state,
                    Err(_) => {
                        return StageResult::Failed {
                            detail: "terminal process registry poisoned".to_string(),
                        }
                    }
                };
                if let Err(detail) = Self::validate_ticket(&state, &ticket) {
                    return StageResult::Failed { detail };
                }
                state.settlement_persisted = true;
            }
            StageResult::Completed
        })
    }

    fn residue<'a>(&'a self, ticket: &'a TeardownTicket) -> BoxFuture<'a, Option<ResidueEvidence>> {
        let state = Arc::clone(&self.state);
        let ticket = ticket.clone();
        Box::pin(async move {
            let state = state.lock().ok()?;
            let process = state.registry.current(ticket.resource_id())?;
            let root = process.root();
            Some(ResidueEvidence::new(
                process.display_label(),
                root.id().pid(),
                root.id().creation_time_100ns(),
                root.canonical_executable().display().to_string(),
                process.display_label(),
                "terminal teardown retained managed Job",
                Vec::new(),
            ))
        })
    }

    fn release_stopped_exact<'a>(
        &'a self,
        ticket: &'a TeardownTicket,
    ) -> BoxFuture<'a, StageResult> {
        let state = Arc::clone(&self.state);
        let ticket = ticket.clone();
        Box::pin(async move {
            let mut state = match state.lock() {
                Ok(state) => state,
                Err(_) => {
                    return StageResult::Failed {
                        detail: "terminal process registry poisoned".to_string(),
                    }
                }
            };
            let ManagedTerminalTeardownState {
                registry,
                release_authority,
                ..
            } = &mut *state;
            let Some(authority) = release_authority.as_ref() else {
                return StageResult::Failed {
                    detail: "terminal teardown release authority was not minted".to_string(),
                };
            };
            match registry.release_stopped_with_authority(&ticket, authority) {
                Ok(crate::process::registry::UnregisterOutcome::Removed(_)) => {
                    *release_authority = None;
                    StageResult::Completed
                }
                Ok(crate::process::registry::UnregisterOutcome::Stale) => StageResult::Failed {
                    detail: "terminal teardown registry release became stale".to_string(),
                },
                Err(error) => StageResult::Failed {
                    detail: error.to_string(),
                },
            }
        })
    }
}

#[cfg(windows)]
#[derive(Debug, Default)]
struct CompletionStoreInner {
    reports: Mutex<VecDeque<(TeardownCompletionKey, TeardownReport)>>,
    durable_path: Option<PathBuf>,
    persist_error: Mutex<Option<String>>,
    lookup_blocked: AtomicBool,
    lookup_started: AtomicUsize,
    persist_blocked: AtomicBool,
    persist_active: AtomicUsize,
    persist_max_active: AtomicUsize,
}

impl TeardownCompletionStore {
    /// Opens the host-owned durable idempotency journal.  Production terminal
    /// launch authority must carry a store created through this constructor;
    /// pure coordinator tests use the in-memory `Default` implementation.
    pub(crate) fn durable(path: impl AsRef<Path>) -> Result<Self, String> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|error| {
                format!(
                    "create teardown completion journal directory {}: {error}",
                    parent.display()
                )
            })?;
        }
        let connection = Connection::open(&path).map_err(|error| {
            format!(
                "open teardown completion journal {}: {error}",
                path.display()
            )
        })?;
        initialize_completion_journal(&connection)?;
        Ok(Self {
            inner: Arc::new(CompletionStoreInner {
                durable_path: Some(path),
                ..CompletionStoreInner::default()
            }),
        })
    }

    #[cfg(windows)]
    pub(crate) fn for_terminal_host() -> Result<Self, String> {
        let root = crate::persistence::app_config_dir()
            .map_err(|error| format!("resolve teardown completion journal root: {error}"))?;
        Self::durable(root.join("teardown-completions.sqlite3"))
    }

    /// Test-only fault injection at the real store boundary. This is an
    /// explicit bounded-store control, not an alternate production backend.
    #[doc(hidden)]
    #[cfg(test)]
    pub(crate) fn fail_persist_for_test(&self, detail: impl Into<String>) {
        *self
            .inner
            .persist_error
            .lock()
            .expect("completion store persist error") = Some(detail.into());
    }

    #[doc(hidden)]
    #[cfg(test)]
    pub(crate) fn block_lookup_for_test(&self) {
        self.inner.lookup_blocked.store(true, Ordering::SeqCst);
    }

    #[doc(hidden)]
    #[cfg(test)]
    pub(crate) fn release_lookup_for_test(&self) {
        self.inner.lookup_blocked.store(false, Ordering::SeqCst);
    }

    #[doc(hidden)]
    #[cfg(test)]
    pub(crate) fn lookup_started_for_test(&self) -> usize {
        self.inner.lookup_started.load(Ordering::SeqCst)
    }

    #[doc(hidden)]
    #[cfg(test)]
    pub(crate) fn block_persist_for_test(&self) {
        self.inner.persist_blocked.store(true, Ordering::SeqCst);
    }

    #[doc(hidden)]
    #[cfg(test)]
    pub(crate) fn release_persist_for_test(&self) {
        self.inner.persist_blocked.store(false, Ordering::SeqCst);
    }

    #[doc(hidden)]
    #[cfg(test)]
    pub(crate) fn persist_max_active_for_test(&self) -> usize {
        self.inner.persist_max_active.load(Ordering::SeqCst)
    }

    #[doc(hidden)]
    #[cfg(test)]
    pub(crate) fn retained_count_for_test(&self) -> usize {
        self.inner
            .reports
            .lock()
            .expect("completion store reports")
            .len()
    }

    fn lookup(
        &self,
        key: &TeardownCompletionKey,
        absolute_deadline: Instant,
    ) -> Result<Option<TeardownReport>, String> {
        checked_remaining_until(absolute_deadline, "completion lookup")?;
        self.inner.lookup_started.fetch_add(1, Ordering::SeqCst);
        while self.inner.lookup_blocked.load(Ordering::SeqCst) {
            let remaining = match remaining_until(absolute_deadline) {
                Ok(remaining) if !remaining.is_zero() => remaining,
                Ok(_) => {
                    return Err("completion lookup timed out while store was blocked".to_string());
                }
                Err(detail) => return Err(detail),
            };
            thread::sleep(remaining.min(Duration::from_millis(1)));
        }
        let cached = self
            .inner
            .reports
            .lock()
            .expect("completion store reports")
            .iter()
            .find(|(stored_key, _)| stored_key == key)
            .map(|(_, report)| report.clone());
        let report = if cached.is_some() {
            cached
        } else if let Some(path) = self.inner.durable_path.as_ref() {
            let remaining = checked_remaining_until(absolute_deadline, "completion lookup")?;
            let connection = open_completion_journal(path, remaining)?;
            let key_json = durable_completion_key(key)?;
            let payload = connection
                .query_row(
                    "SELECT report_json FROM teardown_completions WHERE completion_key = ?1",
                    params![key_json],
                    |row| row.get::<_, String>(0),
                )
                .optional()
                .map_err(|error| format!("read teardown completion journal: {error}"))?;
            match payload {
                Some(payload) => Some(decode_durable_report(key, &payload)?),
                None => None,
            }
        } else {
            None
        };
        checked_remaining_until(absolute_deadline, "completion lookup")?;
        Ok(report)
    }

    fn persist(
        &self,
        key: &TeardownCompletionKey,
        report: &TeardownReport,
        absolute_deadline: Instant,
    ) -> Result<(), String> {
        checked_remaining_until(absolute_deadline, "completion persistence")?;
        let active = self.inner.persist_active.fetch_add(1, Ordering::SeqCst) + 1;
        self.inner
            .persist_max_active
            .fetch_max(active, Ordering::SeqCst);
        while self.inner.persist_blocked.load(Ordering::SeqCst) {
            let remaining = match remaining_until(absolute_deadline) {
                Ok(remaining) if !remaining.is_zero() => remaining,
                Ok(_) => {
                    self.inner.persist_active.fetch_sub(1, Ordering::SeqCst);
                    return Err(
                        "completion persistence timed out while store was blocked".to_string()
                    );
                }
                Err(detail) => {
                    self.inner.persist_active.fetch_sub(1, Ordering::SeqCst);
                    return Err(detail);
                }
            };
            thread::sleep(remaining.min(Duration::from_millis(1)));
        }
        self.inner.persist_active.fetch_sub(1, Ordering::SeqCst);

        if let Some(detail) = self
            .inner
            .persist_error
            .lock()
            .expect("completion store persist error")
            .clone()
        {
            return Err(detail);
        }
        if let Some(path) = self.inner.durable_path.as_ref() {
            let remaining = checked_remaining_until(absolute_deadline, "completion persistence")?;
            let mut connection = open_completion_journal(path, remaining)?;
            let key_json = durable_completion_key(key)?;
            let report_json = encode_durable_report(report)?;
            let transaction = connection
                .transaction()
                .map_err(|error| format!("begin teardown completion transaction: {error}"))?;
            transaction
                .execute(
                    "INSERT INTO teardown_completions(completion_key, report_json) VALUES (?1, ?2)
                     ON CONFLICT(completion_key) DO UPDATE SET report_json = excluded.report_json",
                    params![key_json, report_json],
                )
                .map_err(|error| format!("persist teardown completion: {error}"))?;
            transaction
                .execute(
                    "DELETE FROM teardown_completions
                     WHERE rowid NOT IN (
                       SELECT rowid FROM teardown_completions
                       ORDER BY rowid DESC LIMIT ?1
                     )",
                    params![DEFAULT_COMPLETED_OPERATION_CAPACITY as i64],
                )
                .map_err(|error| format!("bound teardown completion journal: {error}"))?;
            transaction
                .commit()
                .map_err(|error| format!("commit teardown completion: {error}"))?;
            checked_remaining_until(absolute_deadline, "completion persistence")?;
        }

        let mut reports = self.inner.reports.lock().expect("completion store reports");
        if let Some((_, stored_report)) =
            reports.iter_mut().find(|(stored_key, _)| stored_key == key)
        {
            *stored_report = report.clone();
        } else {
            if reports.len() >= DEFAULT_COMPLETED_OPERATION_CAPACITY {
                reports.pop_front();
            }
            reports.push_back((key.clone(), report.clone()));
        }
        drop(reports);
        checked_remaining_until(absolute_deadline, "completion persistence")?;
        Ok(())
    }
}

const COMPLETION_JOURNAL_SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS teardown_completions (
    completion_key TEXT PRIMARY KEY NOT NULL,
    report_json TEXT NOT NULL
) STRICT;
";

fn initialize_completion_journal(connection: &Connection) -> Result<(), String> {
    connection
        .execute_batch(COMPLETION_JOURNAL_SCHEMA)
        .map_err(|error| format!("initialize teardown completion journal: {error}"))
}

fn open_completion_journal(path: &Path, remaining: Duration) -> Result<Connection, String> {
    let connection = Connection::open(path).map_err(|error| {
        format!(
            "open teardown completion journal {}: {error}",
            path.display()
        )
    })?;
    connection
        .busy_timeout(remaining.min(Duration::from_millis(250)))
        .map_err(|error| format!("bound teardown completion journal lock wait: {error}"))?;
    initialize_completion_journal(&connection)?;
    Ok(connection)
}

#[derive(Serialize)]
struct DurableCompletionKey<'a> {
    action_epoch: u64,
    resource_id: String,
    runtime_generation: u64,
    owner_kind: &'static str,
    owner_id: Option<String>,
    root_pid: u32,
    root_creation_time_100ns: u64,
    root_executable: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    _reserved: Option<&'a str>,
}

fn durable_completion_key(key: &TeardownCompletionKey) -> Result<String, String> {
    let (owner_kind, owner_id) = match key.fence.owner() {
        ProcessOwner::Task(task_id) => ("task", Some(task_id.to_string())),
        ProcessOwner::Host => ("host", None),
    };
    let resource = key.fence.resource();
    let root = key.fence.root();
    serde_json::to_string(&DurableCompletionKey {
        action_epoch: key.action_epoch,
        resource_id: resource.resource_id.to_string(),
        runtime_generation: resource.runtime_generation,
        owner_kind,
        owner_id,
        root_pid: root.id().pid(),
        root_creation_time_100ns: root.id().creation_time_100ns(),
        root_executable: root.canonical_executable().display().to_string(),
        _reserved: None,
    })
    .map_err(|error| format!("encode teardown completion key: {error}"))
}

#[derive(Serialize, Deserialize)]
struct DurableResidue {
    job_name: String,
    pid: u32,
    creation_time_100ns: u64,
    executable: String,
    command_label: String,
    last_lifecycle_event: String,
    attempted_stages: Vec<String>,
}

#[derive(Serialize, Deserialize)]
struct DurableReport {
    operation_id: String,
    outcome: String,
    attempted_stages: Vec<String>,
    errors: Vec<String>,
    residue: Option<DurableResidue>,
}

fn teardown_stage_name(stage: TeardownStage) -> &'static str {
    match stage {
        TeardownStage::Drain => "drain",
        TeardownStage::CooperativeClose => "cooperative_close",
        TeardownStage::CooperativeWait => "cooperative_wait",
        TeardownStage::InterruptOrSafeClose => "interrupt_or_safe_close",
        TeardownStage::InterruptWait => "interrupt_wait",
        TeardownStage::TerminateTree => "terminate_tree",
        TeardownStage::TerminationWait => "termination_wait",
        TeardownStage::SettleActiveProcessZero => "settle_active_process_zero",
        TeardownStage::DetachAfterZero => "detach_after_zero",
        TeardownStage::ReconcilePorts => "reconcile_ports",
        TeardownStage::PersistSettlement => "persist_settlement",
        TeardownStage::ReleaseStoppedExact => "release_stopped_exact",
    }
}

fn parse_teardown_stage(value: &str) -> Result<TeardownStage, String> {
    match value {
        "drain" => Ok(TeardownStage::Drain),
        "cooperative_close" => Ok(TeardownStage::CooperativeClose),
        "cooperative_wait" => Ok(TeardownStage::CooperativeWait),
        "interrupt_or_safe_close" => Ok(TeardownStage::InterruptOrSafeClose),
        "interrupt_wait" => Ok(TeardownStage::InterruptWait),
        "terminate_tree" => Ok(TeardownStage::TerminateTree),
        "termination_wait" => Ok(TeardownStage::TerminationWait),
        "settle_active_process_zero" => Ok(TeardownStage::SettleActiveProcessZero),
        "detach_after_zero" => Ok(TeardownStage::DetachAfterZero),
        "reconcile_ports" => Ok(TeardownStage::ReconcilePorts),
        "persist_settlement" => Ok(TeardownStage::PersistSettlement),
        "release_stopped_exact" => Ok(TeardownStage::ReleaseStoppedExact),
        other => Err(format!("unknown durable teardown stage `{other}`")),
    }
}

fn encode_durable_report(report: &TeardownReport) -> Result<String, String> {
    let residue = report.residue.as_ref().map(|residue| DurableResidue {
        job_name: residue.job_name.clone(),
        pid: residue.pid,
        creation_time_100ns: residue.creation_time_100ns,
        executable: residue.executable.clone(),
        command_label: residue.command_label.clone(),
        last_lifecycle_event: residue.last_lifecycle_event.clone(),
        attempted_stages: residue
            .attempted_stages
            .iter()
            .copied()
            .map(teardown_stage_name)
            .map(str::to_string)
            .collect(),
    });
    serde_json::to_string(&DurableReport {
        operation_id: report.operation_id().to_string(),
        outcome: match report.outcome {
            TeardownOutcome::Closed => "closed",
            TeardownOutcome::Leaked => "leaked",
            TeardownOutcome::CleanupFailed => "cleanup_failed",
        }
        .to_string(),
        attempted_stages: report
            .attempted_stages
            .iter()
            .copied()
            .map(teardown_stage_name)
            .map(str::to_string)
            .collect(),
        errors: report
            .errors
            .iter()
            .take(MAX_TEARDOWN_ERRORS)
            .cloned()
            .collect(),
        residue,
    })
    .map_err(|error| format!("encode teardown completion report: {error}"))
}

fn decode_durable_report(
    key: &TeardownCompletionKey,
    payload: &str,
) -> Result<TeardownReport, String> {
    if payload.len() > 64 * 1024 {
        return Err("durable teardown completion report exceeds 64 KiB".to_string());
    }
    let durable: DurableReport = serde_json::from_str(payload)
        .map_err(|error| format!("decode teardown completion report: {error}"))?;
    if durable.attempted_stages.len() > MAX_RESIDUE_STAGES
        || durable.errors.len() > MAX_TEARDOWN_ERRORS
    {
        return Err("durable teardown completion report exceeds collection bounds".to_string());
    }
    let operation_id = OperationId::parse(&durable.operation_id)
        .map_err(|error| format!("decode teardown completion operation id: {error}"))?;
    let scope = match key.fence.owner() {
        ProcessOwner::Task(task_id) => TeardownScope::Task(task_id),
        ProcessOwner::Host => TeardownScope::Host,
    };
    let ticket = TeardownTicket::new(operation_id, scope, key.action_epoch, key.fence.clone())
        .map_err(|_| "durable teardown completion scope mismatch".to_string())?;
    let attempted_stages = durable
        .attempted_stages
        .iter()
        .map(|stage| parse_teardown_stage(stage))
        .collect::<Result<Vec<_>, _>>()?;
    let outcome = match durable.outcome.as_str() {
        "closed" => TeardownOutcome::Closed,
        "leaked" => TeardownOutcome::Leaked,
        "cleanup_failed" => TeardownOutcome::CleanupFailed,
        other => return Err(format!("unknown durable teardown outcome `{other}`")),
    };
    let residue = durable
        .residue
        .map(|residue| {
            if residue.attempted_stages.len() > MAX_RESIDUE_STAGES {
                return Err("durable teardown residue exceeds stage bound".to_string());
            }
            let stages = residue
                .attempted_stages
                .iter()
                .map(|stage| parse_teardown_stage(stage))
                .collect::<Result<Vec<_>, _>>()?;
            Ok(ResidueEvidence::new(
                residue.job_name,
                residue.pid,
                residue.creation_time_100ns,
                residue.executable,
                residue.command_label,
                residue.last_lifecycle_event,
                stages,
            ))
        })
        .transpose()?;
    Ok(TeardownReport {
        ticket,
        outcome,
        attempted_stages,
        errors: durable
            .errors
            .into_iter()
            .map(|error| sanitize_text(&error))
            .collect(),
        residue,
    })
}

/// Bounded authority retained when admission or executor capacity cannot be
/// settled before the operation deadline. The coordinator never discards a
/// receipt that it cannot safely roll back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TeardownRetention {
    tickets: Vec<TeardownTicket>,
    receipts: Vec<AdmissionReceipt>,
    detail: String,
}

impl TeardownRetention {
    fn new(
        tickets: Vec<TeardownTicket>,
        receipts: Vec<AdmissionReceipt>,
        detail: impl Into<String>,
    ) -> Self {
        let tickets = bounded_ticket_vec(tickets);
        let receipts = bounded_receipt_vec(receipts);
        Self {
            tickets,
            receipts,
            detail: sanitize_text(&detail.into()),
        }
    }

    pub fn tickets(&self) -> &[TeardownTicket] {
        &self.tickets
    }

    pub fn receipts(&self) -> &[AdmissionReceipt] {
        &self.receipts
    }

    pub fn detail(&self) -> &str {
        &self.detail
    }
}

fn bounded_ticket_vec(tickets: Vec<TeardownTicket>) -> Vec<TeardownTicket> {
    let mut bounded = Vec::with_capacity(tickets.len().min(MAX_TEARDOWN_RETENTION_ITEMS));
    bounded.extend(tickets.into_iter().take(MAX_TEARDOWN_RETENTION_ITEMS));
    bounded
}

fn bounded_receipt_vec(receipts: Vec<AdmissionReceipt>) -> Vec<AdmissionReceipt> {
    let mut bounded = Vec::with_capacity(receipts.len().min(MAX_TEARDOWN_RETENTION_ITEMS));
    bounded.extend(receipts.into_iter().take(MAX_TEARDOWN_RETENTION_ITEMS));
    bounded
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TeardownReject {
    StaleEpoch { expected: u64, actual: u64 },
    NonClosingScope,
    FenceMismatch,
    CompletionJournalFull,
    CompletionLookupFailed { detail: String },
    ExecutorClosed,
    NoMatchingCleanup,
    Admission(TeardownAdmissionError),
    AdmissionTimeout { retained: TeardownRetention },
    CapacityTimeout { retained: TeardownRetention },
    CleanupFailed { retained: TeardownRetention },
}

impl From<TeardownAdmissionError> for TeardownReject {
    fn from(error: TeardownAdmissionError) -> Self {
        match error {
            TeardownAdmissionError::StaleEpoch { expected, actual } => {
                Self::StaleEpoch { expected, actual }
            }
            TeardownAdmissionError::FenceMismatch => Self::FenceMismatch,
            TeardownAdmissionError::Timeout { detail } => Self::AdmissionTimeout {
                retained: TeardownRetention::new(Vec::new(), Vec::new(), detail),
            },
            other => Self::Admission(other),
        }
    }
}

#[derive(Debug)]
struct CleanupCell {
    result: Mutex<Option<TeardownReport>>,
    done: watch::Sender<bool>,
    blocking_done: Condvar,
    fallback: TeardownReport,
}

impl CleanupCell {
    fn new(ticket: &TeardownTicket) -> Self {
        let (done, _receiver) = watch::channel(false);
        Self {
            result: Mutex::new(None),
            done,
            blocking_done: Condvar::new(),
            fallback: waiter_failure_report(ticket.clone(), "teardown waiter channel closed"),
        }
    }

    fn finish(&self, report: TeardownReport) {
        let mut result = self.result.lock().expect("teardown result mutex poisoned");
        if result.is_none() {
            *result = Some(report);
            self.done.send_replace(true);
        }
        self.blocking_done.notify_all();
    }

    async fn wait(&self) -> TeardownReport {
        let mut done = self.done.subscribe();
        loop {
            if let Some(report) = self
                .result
                .lock()
                .expect("teardown result mutex poisoned")
                .clone()
            {
                return report;
            }
            if done.changed().await.is_err() {
                let fallback = self.fallback.clone();
                self.finish(fallback.clone());
                return fallback;
            }
        }
    }

    fn wait_blocking(&self, timeout: Duration) -> Result<TeardownReport, String> {
        let deadline = Instant::now()
            .checked_add(timeout)
            .ok_or_else(|| "teardown waiter deadline overflow".to_string())?;
        let mut result = self
            .result
            .lock()
            .map_err(|_| "teardown result mutex poisoned".to_string())?;
        loop {
            if let Some(report) = result.clone() {
                return Ok(report);
            }
            let remaining = checked_remaining_until(deadline, "teardown waiter")?;
            let (next, timeout_result) = self
                .blocking_done
                .wait_timeout(result, remaining)
                .map_err(|_| "teardown result mutex poisoned".to_string())?;
            result = next;
            if timeout_result.timed_out() && result.is_none() {
                return Err("teardown waiter exceeded its bounded wait".to_string());
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct TeardownWaiter {
    cell: Arc<CleanupCell>,
}

impl TeardownWaiter {
    /// Waiting is cancellable; dropping this waiter never cancels the owned
    /// cleanup task.
    pub async fn wait(&self) -> TeardownReport {
        self.cell.wait().await
    }

    pub(crate) fn wait_blocking(&self, timeout: Duration) -> Result<TeardownReport, String> {
        self.cell.wait_blocking(timeout)
    }
}

#[derive(Debug)]
pub struct TeardownBatchWaiter {
    waiters: Vec<TeardownWaiter>,
}

impl TeardownBatchWaiter {
    pub async fn wait(self) -> Vec<TeardownReport> {
        let mut reports = Vec::with_capacity(self.waiters.len());
        for waiter in self.waiters {
            reports.push(waiter.wait().await);
        }
        reports.sort_by(|left, right| {
            left.resource_id().cmp(&right.resource_id()).then_with(|| {
                left.fence()
                    .resource()
                    .runtime_generation
                    .cmp(&right.fence().resource().runtime_generation)
            })
        });
        reports
    }
}

#[derive(Debug)]
struct CancellationToken {
    requested: AtomicBool,
    changed: watch::Sender<bool>,
}

impl CancellationToken {
    fn new() -> Arc<Self> {
        let (changed, _receiver) = watch::channel(false);
        Arc::new(Self {
            requested: AtomicBool::new(false),
            changed,
        })
    }

    fn request(&self) {
        if !self.requested.swap(true, Ordering::SeqCst) {
            self.changed.send_replace(true);
        }
    }

    fn is_requested(&self) -> bool {
        self.requested.load(Ordering::SeqCst)
    }

    async fn cancelled(&self) {
        if self.is_requested() {
            return;
        }
        let mut receiver = self.changed.subscribe();
        if *receiver.borrow() {
            return;
        }
        let _ = receiver.changed().await;
    }
}

#[derive(Debug)]
struct ActiveCleanup {
    key: TeardownCompletionKey,
    cell: Arc<CleanupCell>,
}

#[derive(Debug, Clone)]
struct ActiveExecution {
    ticket: TeardownTicket,
    key: TeardownCompletionKey,
    cell: Arc<CleanupCell>,
    cancellation: Arc<CancellationToken>,
    coordinator_state: Arc<Mutex<CoordinatorState>>,
}

#[derive(Debug)]
struct CompletedCleanup {
    key: TeardownCompletionKey,
    cell: Arc<CleanupCell>,
}

#[derive(Debug, Default)]
struct CoordinatorState {
    active: Vec<ActiveCleanup>,
    completed: VecDeque<CompletedCleanup>,
}

struct CleanupWork {
    ticket: TeardownTicket,
    key: TeardownCompletionKey,
    cell: Arc<CleanupCell>,
    cancellation: Arc<CancellationToken>,
    effects: Arc<dyn TeardownEffects>,
    deadline: CleanupDeadline,
    state: Arc<Mutex<CoordinatorState>>,
    completion_store: TeardownCompletionStore,
    completed_operation_capacity: usize,
}

struct ExecutorQueueState {
    queue: VecDeque<CleanupWork>,
    occupied: usize,
    closed: bool,
    active: Vec<ActiveExecution>,
}

struct TeardownExecutorInner {
    state: Mutex<ExecutorQueueState>,
    changed: Condvar,
    worker_capacity: usize,
    queue_capacity: usize,
}

struct TeardownExecutor {
    keepalive: Arc<TeardownExecutorKeepalive>,
}

struct TeardownExecutorKeepalive {
    inner: Arc<TeardownExecutorInner>,
    workers: Mutex<Vec<JoinHandle<()>>>,
}

struct ExecutorReservation {
    inner: Arc<TeardownExecutorInner>,
    remaining: usize,
}

impl TeardownExecutor {
    fn new(worker_capacity: usize, queue_capacity: usize) -> Arc<Self> {
        let worker_capacity = worker_capacity.max(1);
        let queue_capacity = queue_capacity.max(1);
        let inner = Arc::new(TeardownExecutorInner {
            state: Mutex::new(ExecutorQueueState {
                queue: VecDeque::with_capacity(queue_capacity),
                occupied: 0,
                closed: false,
                active: Vec::new(),
            }),
            changed: Condvar::new(),
            worker_capacity,
            queue_capacity,
        });
        let keepalive = Arc::new(TeardownExecutorKeepalive {
            inner: Arc::clone(&inner),
            workers: Mutex::new(Vec::with_capacity(worker_capacity)),
        });
        let mut workers = Vec::with_capacity(worker_capacity);
        for index in 0..worker_capacity {
            let worker_inner = Arc::clone(&inner);
            workers.push(
                thread::Builder::new()
                    .name(format!("devmanager-teardown-worker-{index}"))
                    .spawn(move || teardown_worker(worker_inner))
                    .expect("spawn bounded teardown worker"),
            );
        }
        *keepalive
            .workers
            .lock()
            .expect("teardown worker handles mutex poisoned") = workers;
        Arc::new(Self { keepalive })
    }

    fn inner(&self) -> &Arc<TeardownExecutorInner> {
        &self.keepalive.inner
    }

    fn shutdown(&self) {
        let work = {
            let mut state = self
                .keepalive
                .inner
                .state
                .lock()
                .expect("teardown executor state mutex poisoned");
            if state.closed {
                None
            } else {
                state.closed = true;
                let queued: Vec<_> = state.queue.drain(..).collect();
                let active = state.active.clone();
                state.active.clear();
                state.occupied = state.occupied.saturating_sub(queued.len());
                self.keepalive.inner.changed.notify_all();
                Some((queued, active))
            }
        };
        let Some((queued, active)) = work else {
            self.keepalive.join_workers();
            return;
        };
        for work in queued {
            cancel_queued_cleanup(work);
        }
        for execution in active {
            execution.cancellation.request();
            execution.cell.finish(waiter_failure_report(
                execution.ticket.clone(),
                "teardown coordinator dropped while cleanup was active; cancellation requested",
            ));
            execution
                .coordinator_state
                .lock()
                .expect("teardown coordinator state mutex poisoned")
                .active
                .retain(|entry| entry.key != execution.key);
        }

        // Waiters are settled above, but the worker may still be inside an
        // effect or persistence adapter. Join the fixed executor workers so
        // no cleanup can mutate state after shutdown returns.
        self.keepalive.join_workers();
    }

    fn is_closed(&self) -> bool {
        self.keepalive
            .inner
            .state
            .lock()
            .expect("teardown executor state mutex poisoned")
            .closed
    }

    fn reserve_many(
        &self,
        count: usize,
        absolute_deadline: Instant,
    ) -> Result<ExecutorReservation, TeardownReject> {
        if count == 0 {
            return Ok(ExecutorReservation {
                inner: Arc::clone(self.inner()),
                remaining: 0,
            });
        }
        if count > self.inner().queue_capacity {
            return Err(TeardownReject::CompletionJournalFull);
        }
        let capacity = self
            .keepalive
            .inner
            .worker_capacity
            .checked_add(self.inner().queue_capacity)
            .ok_or(TeardownReject::CompletionJournalFull)?;
        if count > capacity {
            return Err(TeardownReject::CompletionJournalFull);
        }
        if checked_remaining_until(absolute_deadline, "executor capacity reservation").is_err() {
            return Err(TeardownReject::CapacityTimeout {
                retained: TeardownRetention::new(
                    Vec::new(),
                    Vec::new(),
                    "executor capacity reservation deadline expired",
                ),
            });
        }
        let mut state = self
            .keepalive
            .inner
            .state
            .lock()
            .expect("teardown executor state mutex poisoned");
        loop {
            if state.closed {
                return Err(TeardownReject::ExecutorClosed);
            }
            if count <= capacity.saturating_sub(state.occupied) {
                state.occupied += count;
                if checked_remaining_until(absolute_deadline, "executor capacity reservation")
                    .is_err()
                {
                    state.occupied = state.occupied.saturating_sub(count);
                    self.keepalive.inner.changed.notify_all();
                    return Err(TeardownReject::CapacityTimeout {
                        retained: TeardownRetention::new(
                            Vec::new(),
                            Vec::new(),
                            "executor capacity reservation deadline expired",
                        ),
                    });
                }
                return Ok(ExecutorReservation {
                    inner: Arc::clone(self.inner()),
                    remaining: count,
                });
            }
            let remaining = match remaining_until(absolute_deadline) {
                Ok(remaining) if !remaining.is_zero() => remaining,
                Ok(_) => {
                    return Err(TeardownReject::CapacityTimeout {
                        retained: TeardownRetention::new(
                            Vec::new(),
                            Vec::new(),
                            "executor capacity reservation deadline expired",
                        ),
                    });
                }
                Err(detail) => {
                    return Err(TeardownReject::CleanupFailed {
                        retained: TeardownRetention::new(Vec::new(), Vec::new(), detail),
                    });
                }
            };
            let (next_state, _) = self
                .keepalive
                .inner
                .changed
                .wait_timeout(state, remaining)
                .expect("teardown executor state mutex poisoned");
            state = next_state;
            if checked_remaining_until(absolute_deadline, "executor capacity reservation").is_err()
            {
                return Err(TeardownReject::CapacityTimeout {
                    retained: TeardownRetention::new(
                        Vec::new(),
                        Vec::new(),
                        "executor capacity reservation deadline expired",
                    ),
                });
            }
        }
    }
}

impl Drop for TeardownExecutor {
    fn drop(&mut self) {
        self.shutdown();
    }
}

impl Drop for TeardownExecutorKeepalive {
    fn drop(&mut self) {
        self.join_workers();
    }
}

impl TeardownExecutorKeepalive {
    fn join_workers(&self) {
        let current = thread::current().id();
        let mut workers = self
            .workers
            .lock()
            .expect("teardown worker handles mutex poisoned");
        let handles = std::mem::take(&mut *workers);
        drop(workers);
        let wait_started = Instant::now();
        while handles.iter().any(|handle| !handle.is_finished())
            && wait_started.elapsed() < Duration::from_secs(2)
        {
            thread::yield_now();
        }
        for handle in handles {
            if handle.thread().id() == current {
                // Cleanup work never owns the executor keepalive; reaching
                // this path would otherwise detach the current worker.
                std::process::abort();
            }
            if !handle.is_finished() {
                // A private production adapter has violated its cancellable
                // construction/polling contract. Returning would orphan a
                // mutating teardown actor, which is worse than failing closed.
                std::process::abort();
            }
            let _ = handle.join();
        }
    }
}

fn cancel_queued_cleanup(work: CleanupWork) {
    let CleanupWork {
        ticket,
        key,
        cell,
        cancellation,
        state,
        ..
    } = work;
    cancellation.request();
    cell.finish(waiter_failure_report(
        ticket,
        "teardown executor shut down before cleanup started; cancellation requested",
    ));
    state
        .lock()
        .expect("teardown coordinator state mutex poisoned")
        .active
        .retain(|active| active.key != key);
}

enum ExecutorSubmitError {
    Closed(Vec<CleanupWork>),
    Timeout(Vec<CleanupWork>),
    ClockFailure(Vec<CleanupWork>, String),
}

impl ExecutorReservation {
    fn submit_all(
        &mut self,
        works: Vec<CleanupWork>,
        absolute_deadline: Instant,
    ) -> Result<(), ExecutorSubmitError> {
        if works.is_empty() {
            return Ok(());
        }
        debug_assert!(works.len() <= self.remaining);
        let count = works.len();
        let mut state = self
            .inner
            .state
            .lock()
            .expect("teardown executor state mutex poisoned");
        loop {
            if state.closed {
                return Err(ExecutorSubmitError::Closed(works));
            }
            if count <= self.inner.queue_capacity.saturating_sub(state.queue.len()) {
                if let Err(detail) =
                    checked_remaining_until(absolute_deadline, "executor submission")
                {
                    return Err(if detail.contains("exceeded teardown absolute deadline") {
                        ExecutorSubmitError::Timeout(works)
                    } else {
                        ExecutorSubmitError::ClockFailure(works, detail)
                    });
                }
                let queue_start = state.queue.len();
                state.queue.extend(works);
                self.remaining = self.remaining.saturating_sub(count);
                self.inner.changed.notify_all();
                match checked_remaining_until(absolute_deadline, "executor submission") {
                    Ok(_) => return Ok(()),
                    Err(detail) if detail.contains("exceeded teardown absolute deadline") => {
                        let submitted = state.queue.split_off(queue_start);
                        self.remaining = self.remaining.saturating_add(count);
                        self.inner.changed.notify_all();
                        return Err(ExecutorSubmitError::Timeout(submitted.into()));
                    }
                    Err(detail) => {
                        let submitted = state.queue.split_off(queue_start);
                        self.remaining = self.remaining.saturating_add(count);
                        self.inner.changed.notify_all();
                        return Err(ExecutorSubmitError::ClockFailure(submitted.into(), detail));
                    }
                }
            }
            let remaining = match remaining_until(absolute_deadline) {
                Ok(remaining) if !remaining.is_zero() => remaining,
                Ok(_) => return Err(ExecutorSubmitError::Timeout(works)),
                Err(detail) => return Err(ExecutorSubmitError::ClockFailure(works, detail)),
            };
            let (next_state, _) = self
                .inner
                .changed
                .wait_timeout(state, remaining)
                .expect("teardown executor state mutex poisoned");
            state = next_state;
            if checked_remaining_until(absolute_deadline, "executor submission").is_err() {
                return Err(ExecutorSubmitError::Timeout(works));
            }
        }
    }
}

impl Drop for ExecutorReservation {
    fn drop(&mut self) {
        if self.remaining == 0 {
            return;
        }
        let mut state = self
            .inner
            .state
            .lock()
            .expect("teardown executor state mutex poisoned");
        state.occupied = state.occupied.saturating_sub(self.remaining);
        self.inner.changed.notify_all();
    }
}

fn teardown_worker(inner: Arc<TeardownExecutorInner>) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("teardown worker runtime");
    loop {
        let work = {
            let mut state = inner
                .state
                .lock()
                .expect("teardown executor state mutex poisoned");
            loop {
                if let Some(work) = state.queue.pop_front() {
                    state.active.push(ActiveExecution {
                        ticket: work.ticket.clone(),
                        key: work.key.clone(),
                        cell: Arc::clone(&work.cell),
                        cancellation: Arc::clone(&work.cancellation),
                        coordinator_state: Arc::clone(&work.state),
                    });
                    inner.changed.notify_all();
                    break Some(work);
                }
                if state.closed {
                    break None;
                }
                state = inner
                    .changed
                    .wait(state)
                    .expect("teardown executor state mutex poisoned");
            }
        };
        let Some(work) = work else {
            return;
        };
        let completed_key = work.key.clone();
        runtime.block_on(run_cleanup(work));
        let mut state = inner
            .state
            .lock()
            .expect("teardown executor state mutex poisoned");
        state.occupied = state.occupied.saturating_sub(1);
        state.active.retain(|active| active.key != completed_key);
        inner.changed.notify_all();
    }
}

pub struct TeardownCoordinator {
    admission: Arc<dyn TeardownAdmission>,
    effects: Arc<dyn TeardownEffects>,
    clock: Arc<dyn TeardownClock>,
    budgets: TeardownBudgets,
    configured_capacity: usize,
    completed_operation_capacity: usize,
    completion_store: TeardownCompletionStore,
    state: Arc<Mutex<CoordinatorState>>,
    admission_serial: Mutex<()>,
    shutdown_started: AtomicBool,
    executor: Arc<TeardownExecutor>,
}

impl TeardownCoordinator {
    /// Build a coordinator from the host-issued concrete adapter set.
    ///
    /// The adapter set cannot be constructed by external crates; the host
    /// controls which bounded Job/IOCP operations cross this authority
    /// boundary.
    fn from_host(
        adapters: TeardownHostAdapters,
        configured_capacity: usize,
        budgets: TeardownBudgets,
        completed_operation_capacity: usize,
        completion_store: TeardownCompletionStore,
    ) -> Self {
        Self::build(
            adapters.admission,
            adapters.effects,
            adapters.clock,
            configured_capacity,
            budgets,
            completed_operation_capacity,
            completion_store,
        )
    }

    #[cfg(test)]
    #[allow(dead_code)]
    pub(crate) fn new(
        admission: Arc<dyn TeardownAdmission>,
        effects: Arc<dyn TeardownEffects>,
        clock: Arc<dyn TeardownClock>,
        completion_store: TeardownCompletionStore,
    ) -> Self {
        Self::with_configuration(
            admission,
            effects,
            clock,
            DEFAULT_CONFIGURED_CAPACITY,
            TeardownBudgets::default(),
            completion_store,
        )
    }

    #[cfg(test)]
    #[allow(dead_code)]
    pub(crate) fn with_capacity(
        admission: Arc<dyn TeardownAdmission>,
        effects: Arc<dyn TeardownEffects>,
        clock: Arc<dyn TeardownClock>,
        configured_capacity: usize,
        completion_store: TeardownCompletionStore,
    ) -> Self {
        Self::with_configuration(
            admission,
            effects,
            clock,
            configured_capacity,
            TeardownBudgets::default(),
            completion_store,
        )
    }

    #[cfg(test)]
    #[allow(dead_code)]
    pub(crate) fn with_configuration(
        admission: Arc<dyn TeardownAdmission>,
        effects: Arc<dyn TeardownEffects>,
        clock: Arc<dyn TeardownClock>,
        configured_capacity: usize,
        budgets: TeardownBudgets,
        completion_store: TeardownCompletionStore,
    ) -> Self {
        Self::with_configuration_and_completion_store(
            admission,
            effects,
            clock,
            configured_capacity,
            budgets,
            DEFAULT_COMPLETED_OPERATION_CAPACITY,
            completion_store,
        )
    }

    #[cfg(test)]
    pub(crate) fn with_configuration_and_completion_store(
        admission: Arc<dyn TeardownAdmission>,
        effects: Arc<dyn TeardownEffects>,
        clock: Arc<dyn TeardownClock>,
        configured_capacity: usize,
        budgets: TeardownBudgets,
        completed_operation_capacity: usize,
        completion_store: TeardownCompletionStore,
    ) -> Self {
        Self::build(
            admission,
            effects,
            clock,
            configured_capacity,
            budgets,
            completed_operation_capacity,
            completion_store,
        )
    }

    /// The sole production constructor. Its dynamic seams have already been
    /// sealed by the module-private `TeardownHostAdapters::terminal` factory;
    /// arbitrary future constructors exist only in cfg(test) fixtures.
    fn build(
        admission: Arc<dyn TeardownAdmission>,
        effects: Arc<dyn TeardownEffects>,
        clock: Arc<dyn TeardownClock>,
        configured_capacity: usize,
        budgets: TeardownBudgets,
        completed_operation_capacity: usize,
        completion_store: TeardownCompletionStore,
    ) -> Self {
        let configured_capacity = configured_capacity.max(1);
        let completed_operation_capacity =
            completed_operation_capacity.clamp(1, DEFAULT_COMPLETED_OPERATION_CAPACITY);
        Self {
            admission,
            effects,
            clock,
            budgets,
            configured_capacity,
            completed_operation_capacity,
            completion_store,
            state: Arc::new(Mutex::new(CoordinatorState::default())),
            admission_serial: Mutex::new(()),
            shutdown_started: AtomicBool::new(false),
            executor: TeardownExecutor::new(configured_capacity, DEFAULT_EXECUTOR_QUEUE_CAPACITY),
        }
    }

    /// Closes fresh admission and settles every queued or active waiter with
    /// a typed fail-closed report, then settles every fixed executor worker.
    /// Production adapters are sealed and deadline-bounded, so no effect or
    /// persistence mutation can outlive this shutdown boundary.
    pub fn shutdown(&self) {
        // Publish the lifecycle fence first, then cross the same serialization
        // gate as admission.  An admission already inside the gate observes
        // the fence at its post-admission barrier and performs exact rollback;
        // shutdown waits for that rollback before returning.  A later request
        // observes the fence before lookup, persistence, capacity, submission,
        // or effect work.
        self.shutdown_started.store(true, Ordering::SeqCst);
        let _admission_serial = self
            .admission_serial
            .lock()
            .expect("teardown admission serial mutex poisoned");
        self.executor.shutdown();
    }

    pub fn configured_capacity(&self) -> usize {
        self.configured_capacity
    }

    pub fn active_operation_count(&self) -> usize {
        self.state
            .lock()
            .expect("teardown coordinator state mutex poisoned")
            .active
            .len()
    }

    pub fn completed_operation_count(&self) -> usize {
        self.state
            .lock()
            .expect("teardown coordinator state mutex poisoned")
            .completed
            .len()
    }

    fn rollback_rejection(
        &self,
        tickets: &[TeardownTicket],
        receipts: &[AdmissionReceipt],
        deadline: CleanupDeadline,
        rejection: TeardownReject,
    ) -> TeardownReject {
        if !receipts
            .iter()
            .any(|receipt| receipt.state() == AdmissionState::Closing)
        {
            return rejection;
        }
        match std::panic::catch_unwind(AssertUnwindSafe(|| {
            bounded_rollback_admission(self.admission.as_ref(), tickets, receipts, deadline)
        })) {
            Ok(Ok(())) => rejection,
            Ok(Err(error)) => TeardownReject::CleanupFailed {
                retained: TeardownRetention::new(
                    tickets.to_vec(),
                    receipts.to_vec(),
                    format!("admission rollback failed: {error:?}"),
                ),
            },
            Err(payload) => TeardownReject::CleanupFailed {
                retained: TeardownRetention::new(
                    tickets.to_vec(),
                    receipts.to_vec(),
                    format!("admission rollback panicked: {}", panic_detail(payload)),
                ),
            },
        }
    }

    fn cleanup_deadline(&self) -> Result<CleanupDeadline, TeardownReject> {
        match std::panic::catch_unwind(AssertUnwindSafe(|| {
            CleanupDeadline::new(self.clock.as_ref(), self.budgets)
        })) {
            Ok(Ok(deadline)) => Ok(deadline),
            Ok(Err(detail)) => Err(TeardownReject::CleanupFailed {
                retained: TeardownRetention::new(Vec::new(), Vec::new(), detail),
            }),
            Err(payload) => Err(TeardownReject::CleanupFailed {
                retained: TeardownRetention::new(
                    Vec::new(),
                    Vec::new(),
                    format!(
                        "teardown deadline construction panicked: {}",
                        panic_detail(payload)
                    ),
                ),
            }),
        }
    }

    pub fn request(&self, ticket: TeardownTicket) -> Result<TeardownWaiter, TeardownReject> {
        let deadline = match std::panic::catch_unwind(AssertUnwindSafe(|| {
            CleanupDeadline::new(self.clock.as_ref(), self.budgets)
        })) {
            Ok(Ok(deadline)) => deadline,
            Ok(Err(detail)) => {
                return Err(TeardownReject::CleanupFailed {
                    retained: TeardownRetention::new(vec![ticket], Vec::new(), detail),
                });
            }
            Err(payload) => {
                return Err(TeardownReject::CleanupFailed {
                    retained: TeardownRetention::new(
                        vec![ticket],
                        Vec::new(),
                        format!(
                            "teardown deadline construction panicked: {}",
                            panic_detail(payload)
                        ),
                    ),
                });
            }
        };
        let absolute_deadline = deadline.absolute;
        let _admission_serial = self
            .admission_serial
            .lock()
            .expect("teardown admission serial mutex poisoned");
        if self.shutdown_started.load(Ordering::Acquire) {
            return Err(TeardownReject::ExecutorClosed);
        }
        let key = completion_key(&ticket);
        if let Some(waiter) = self.find_existing_waiter(&key) {
            return Ok(waiter);
        }
        if self.executor.is_closed() {
            return Err(TeardownReject::ExecutorClosed);
        }
        if let Some(waiter) = self.lookup_completed(&ticket, &key, deadline)? {
            return Ok(waiter);
        }

        let mut reservation = match self.executor.reserve_many(1, absolute_deadline) {
            Ok(reservation) => reservation,
            Err(TeardownReject::CapacityTimeout { .. }) => {
                return Err(TeardownReject::CapacityTimeout {
                    retained: TeardownRetention::new(
                        vec![ticket],
                        Vec::new(),
                        "executor capacity reservation deadline expired",
                    ),
                });
            }
            Err(TeardownReject::CleanupFailed { .. }) => {
                return Err(TeardownReject::CleanupFailed {
                    retained: TeardownRetention::new(
                        vec![ticket],
                        Vec::new(),
                        "executor capacity reservation clock failed",
                    ),
                });
            }
            Err(other) => return Err(other),
        };
        let rollback_tickets = [ticket.clone()];
        let receipts = match std::panic::catch_unwind(AssertUnwindSafe(|| {
            bounded_close_admission_batch(
                self.admission.as_ref(),
                std::slice::from_ref(&ticket),
                deadline,
            )
        })) {
            Ok(Ok(receipts)) => receipts,
            Ok(Err(TeardownAdmissionError::Timeout { detail })) => {
                return Err(TeardownReject::AdmissionTimeout {
                    retained: TeardownRetention::new(vec![ticket], Vec::new(), detail),
                });
            }
            Ok(Err(error)) => return Err(TeardownReject::from(error)),
            Err(payload) => {
                return Err(TeardownReject::CleanupFailed {
                    retained: TeardownRetention::new(
                        vec![ticket],
                        Vec::new(),
                        format!("admission close panicked: {}", panic_detail(payload)),
                    ),
                });
            }
        };
        if receipts.len() != 1 {
            return Err(self.rollback_rejection(
                &rollback_tickets,
                &receipts,
                deadline,
                TeardownReject::Admission(TeardownAdmissionError::Other {
                    detail: "admission returned an invalid single-ticket receipt batch".to_string(),
                }),
            ));
        }
        let receipt = match receipts.first() {
            Some(receipt) => receipt,
            None => {
                return Err(self.rollback_rejection(
                    &rollback_tickets,
                    &receipts,
                    deadline,
                    TeardownReject::Admission(TeardownAdmissionError::Other {
                        detail: "admission returned no receipt".to_string(),
                    }),
                ));
            }
        };
        if let Err(rejection) = validate_receipt(&ticket, receipt) {
            return Err(self.rollback_rejection(&rollback_tickets, &receipts, deadline, rejection));
        }
        let rollback_receipts = receipts.clone();

        // Shutdown may have closed the executor while the bounded admission
        // call was in flight.  Roll the exact receipt back before publishing
        // any active cleanup, so admission and executor ownership are one
        // linearized transition.
        if self.shutdown_started.load(Ordering::Acquire) {
            return Err(self.rollback_rejection(
                &rollback_tickets,
                &rollback_receipts,
                deadline,
                TeardownReject::ExecutorClosed,
            ));
        }

        let cell = Arc::new(CleanupCell::new(&ticket));
        self.state
            .lock()
            .expect("teardown coordinator state mutex poisoned")
            .active
            .push(ActiveCleanup {
                key: key.clone(),
                cell: Arc::clone(&cell),
            });
        let work = self.cleanup_work(ticket, key.clone(), Arc::clone(&cell), deadline);
        if let Err(error) = reservation.submit_all(vec![work], absolute_deadline) {
            let (works, rejection) = match error {
                ExecutorSubmitError::Closed(works) => (works, TeardownReject::ExecutorClosed),
                ExecutorSubmitError::Timeout(works) => (
                    works,
                    TeardownReject::CapacityTimeout {
                        retained: TeardownRetention::new(
                            Vec::new(),
                            Vec::new(),
                            "executor submission deadline expired",
                        ),
                    },
                ),
                ExecutorSubmitError::ClockFailure(works, detail) => (
                    works,
                    TeardownReject::CleanupFailed {
                        retained: TeardownRetention::new(Vec::new(), Vec::new(), detail),
                    },
                ),
            };
            for work in works {
                cancel_queued_cleanup(work);
            }
            self.remove_active(&key);
            return Err(self.rollback_rejection(
                &rollback_tickets,
                &rollback_receipts,
                deadline,
                rejection,
            ));
        }
        Ok(TeardownWaiter { cell })
    }

    /// Join an already-admitted cleanup by its exact action/fence key.
    ///
    /// Host shutdown uses this path for Task-owned work; it never constructs a
    /// Host ticket around a Task fence and therefore cannot receive a
    /// mismatched scope report.
    pub fn join(
        &self,
        action_epoch: u64,
        fence: &ManagedProcessFence,
    ) -> Result<TeardownWaiter, TeardownReject> {
        let _admission_serial = self
            .admission_serial
            .lock()
            .expect("teardown admission serial mutex poisoned");
        let key = TeardownCompletionKey::new(action_epoch, fence.clone());
        if let Some(waiter) = self.find_existing_waiter(&key) {
            return Ok(waiter);
        }
        let deadline = self.cleanup_deadline()?;
        if let Some(waiter) = self.lookup_completed_by_key(&key, deadline)? {
            return Ok(waiter);
        }
        Err(TeardownReject::NoMatchingCleanup)
    }

    pub fn request_batch(
        &self,
        tickets: Vec<TeardownTicket>,
    ) -> Result<TeardownBatchWaiter, TeardownReject> {
        if tickets.len() > MAX_TEARDOWN_BATCH_ITEMS {
            return Err(TeardownReject::CompletionJournalFull);
        }
        let deadline = match std::panic::catch_unwind(AssertUnwindSafe(|| {
            CleanupDeadline::new(self.clock.as_ref(), self.budgets)
        })) {
            Ok(Ok(deadline)) => deadline,
            Ok(Err(detail)) => {
                return Err(TeardownReject::CleanupFailed {
                    retained: TeardownRetention::new(tickets, Vec::new(), detail),
                });
            }
            Err(payload) => {
                return Err(TeardownReject::CleanupFailed {
                    retained: TeardownRetention::new(
                        tickets,
                        Vec::new(),
                        format!(
                            "teardown deadline construction panicked: {}",
                            panic_detail(payload)
                        ),
                    ),
                });
            }
        };
        let absolute_deadline = deadline.absolute;
        let _admission_serial = self
            .admission_serial
            .lock()
            .expect("teardown admission serial mutex poisoned");
        if self.shutdown_started.load(Ordering::Acquire) {
            return Err(TeardownReject::ExecutorClosed);
        }
        let mut waiters = Vec::with_capacity(tickets.len());
        let mut fresh = Vec::new();
        let mut fresh_duplicates = Vec::new();
        if self.executor.is_closed() {
            return Err(TeardownReject::ExecutorClosed);
        }
        for ticket in tickets {
            let key = completion_key(&ticket);
            if let Some(waiter) = self.find_existing_waiter(&key) {
                waiters.push(waiter);
            } else if let Some(waiter) = self.lookup_completed(&ticket, &key, deadline)? {
                waiters.push(waiter);
            } else if fresh.iter().any(|(fresh_key, _)| fresh_key == &key) {
                fresh_duplicates.push(key);
            } else {
                fresh.push((key, ticket));
            }
        }
        if fresh.is_empty() {
            return Ok(TeardownBatchWaiter { waiters });
        }

        let fresh_tickets: Vec<TeardownTicket> =
            fresh.iter().map(|(_, ticket)| ticket.clone()).collect();
        let mut reservation = match self.executor.reserve_many(fresh.len(), absolute_deadline) {
            Ok(reservation) => reservation,
            Err(TeardownReject::CapacityTimeout { .. }) => {
                return Err(TeardownReject::CapacityTimeout {
                    retained: TeardownRetention::new(
                        fresh_tickets,
                        Vec::new(),
                        "executor capacity reservation deadline expired",
                    ),
                });
            }
            Err(TeardownReject::CleanupFailed { .. }) => {
                return Err(TeardownReject::CleanupFailed {
                    retained: TeardownRetention::new(
                        fresh_tickets,
                        Vec::new(),
                        "executor capacity reservation clock failed",
                    ),
                });
            }
            Err(other) => return Err(other),
        };
        let receipts = match std::panic::catch_unwind(AssertUnwindSafe(|| {
            bounded_close_admission_batch(self.admission.as_ref(), &fresh_tickets, deadline)
        })) {
            Ok(Ok(receipts)) => receipts,
            Ok(Err(TeardownAdmissionError::Timeout { detail })) => {
                return Err(TeardownReject::AdmissionTimeout {
                    retained: TeardownRetention::new(fresh_tickets, Vec::new(), detail),
                });
            }
            Ok(Err(error)) => return Err(TeardownReject::from(error)),
            Err(payload) => {
                return Err(TeardownReject::CleanupFailed {
                    retained: TeardownRetention::new(
                        fresh_tickets,
                        Vec::new(),
                        format!("admission close panicked: {}", panic_detail(payload)),
                    ),
                });
            }
        };
        if receipts.len() != fresh.len() {
            return Err(self.rollback_rejection(
                &fresh_tickets,
                &receipts,
                deadline,
                TeardownReject::Admission(TeardownAdmissionError::Other {
                    detail: "admission returned an incomplete receipt batch".to_string(),
                }),
            ));
        }
        for ((_, ticket), receipt) in fresh.iter().zip(receipts.iter()) {
            if let Err(rejection) = validate_receipt(ticket, receipt) {
                return Err(self.rollback_rejection(
                    &fresh_tickets,
                    &receipts,
                    deadline,
                    rejection,
                ));
            }
        }
        let rollback_receipts = receipts.clone();

        if self.shutdown_started.load(Ordering::Acquire) {
            return Err(self.rollback_rejection(
                &fresh_tickets,
                &rollback_receipts,
                deadline,
                TeardownReject::ExecutorClosed,
            ));
        }

        let mut works = Vec::with_capacity(fresh.len());
        let mut created = Vec::with_capacity(fresh.len());
        for (key, ticket) in fresh {
            let cell = Arc::new(CleanupCell::new(&ticket));
            self.state
                .lock()
                .expect("teardown coordinator state mutex poisoned")
                .active
                .push(ActiveCleanup {
                    key: key.clone(),
                    cell: Arc::clone(&cell),
                });
            waiters.push(TeardownWaiter {
                cell: Arc::clone(&cell),
            });
            created.push((key.clone(), Arc::clone(&cell)));
            works.push(self.cleanup_work(ticket, key, cell, deadline));
        }
        for key in fresh_duplicates {
            if let Some((_, cell)) = created.iter().find(|(created_key, _)| *created_key == key) {
                waiters.push(TeardownWaiter {
                    cell: Arc::clone(cell),
                });
            }
        }
        if let Err(error) = reservation.submit_all(works, absolute_deadline) {
            let (works, rejection) = match error {
                ExecutorSubmitError::Closed(works) => (works, TeardownReject::ExecutorClosed),
                ExecutorSubmitError::Timeout(works) => (
                    works,
                    TeardownReject::CapacityTimeout {
                        retained: TeardownRetention::new(
                            Vec::new(),
                            Vec::new(),
                            "executor submission deadline expired",
                        ),
                    },
                ),
                ExecutorSubmitError::ClockFailure(works, detail) => (
                    works,
                    TeardownReject::CleanupFailed {
                        retained: TeardownRetention::new(Vec::new(), Vec::new(), detail),
                    },
                ),
            };
            for work in works {
                cancel_queued_cleanup(work);
            }
            for (key, _) in &created {
                self.remove_active(key);
            }
            return Err(self.rollback_rejection(
                &fresh_tickets,
                &rollback_receipts,
                deadline,
                rejection,
            ));
        }
        Ok(TeardownBatchWaiter { waiters })
    }

    fn cleanup_work(
        &self,
        ticket: TeardownTicket,
        key: TeardownCompletionKey,
        cell: Arc<CleanupCell>,
        deadline: CleanupDeadline,
    ) -> CleanupWork {
        CleanupWork {
            ticket,
            key,
            cell,
            effects: Arc::clone(&self.effects),
            deadline,
            state: Arc::clone(&self.state),
            completion_store: self.completion_store.clone(),
            completed_operation_capacity: self.completed_operation_capacity,
            cancellation: CancellationToken::new(),
        }
    }

    fn find_existing_waiter(&self, key: &TeardownCompletionKey) -> Option<TeardownWaiter> {
        let state = self
            .state
            .lock()
            .expect("teardown coordinator state mutex poisoned");
        if let Some(existing) = state.active.iter().find(|existing| existing.key == *key) {
            return Some(TeardownWaiter {
                cell: Arc::clone(&existing.cell),
            });
        }
        state
            .completed
            .iter()
            .find(|existing| existing.key == *key)
            .map(|existing| TeardownWaiter {
                cell: Arc::clone(&existing.cell),
            })
    }

    fn lookup_completed(
        &self,
        ticket: &TeardownTicket,
        key: &TeardownCompletionKey,
        deadline: CleanupDeadline,
    ) -> Result<Option<TeardownWaiter>, TeardownReject> {
        self.lookup_completed_by_key_with_ticket(key, ticket, deadline)
    }

    fn lookup_completed_by_key(
        &self,
        key: &TeardownCompletionKey,
        deadline: CleanupDeadline,
    ) -> Result<Option<TeardownWaiter>, TeardownReject> {
        match self.lookup_report(key, deadline)? {
            Some(report) => {
                if report.action_epoch() != key.action_epoch || report.fence() != key.fence() {
                    return Err(TeardownReject::CompletionLookupFailed {
                        detail: "completion store returned a mismatched report".to_string(),
                    });
                }
                let cell = Arc::new(CleanupCell::new(&report.ticket));
                cell.finish(report);
                Ok(Some(TeardownWaiter { cell }))
            }
            None => Ok(None),
        }
    }

    fn lookup_completed_by_key_with_ticket(
        &self,
        key: &TeardownCompletionKey,
        ticket: &TeardownTicket,
        deadline: CleanupDeadline,
    ) -> Result<Option<TeardownWaiter>, TeardownReject> {
        match self.lookup_report(key, deadline)? {
            Some(report) => {
                if report.action_epoch() != key.action_epoch || report.fence() != key.fence() {
                    return Err(TeardownReject::CompletionLookupFailed {
                        detail: "completion store returned a mismatched report".to_string(),
                    });
                }
                let cell = Arc::new(CleanupCell::new(ticket));
                cell.finish(report);
                Ok(Some(TeardownWaiter { cell }))
            }
            None => Ok(None),
        }
    }

    fn lookup_report(
        &self,
        key: &TeardownCompletionKey,
        deadline: CleanupDeadline,
    ) -> Result<Option<TeardownReport>, TeardownReject> {
        deadline
            .check("completion lookup")
            .map_err(|detail| TeardownReject::CompletionLookupFailed { detail })?;
        let store = self.completion_store.clone();
        let lookup_key = key.clone();
        let report = std::panic::catch_unwind(AssertUnwindSafe(|| {
            store.lookup(&lookup_key, deadline.absolute)
        }))
        .map_err(|payload| TeardownReject::CompletionLookupFailed {
            detail: format!("completion lookup panicked: {}", panic_detail(payload)),
        })?
        .map_err(|detail| TeardownReject::CompletionLookupFailed { detail })?;
        deadline
            .check("completion lookup")
            .map_err(|detail| TeardownReject::CompletionLookupFailed { detail })?;
        Ok(report)
    }

    fn remove_active(&self, key: &TeardownCompletionKey) {
        self.state
            .lock()
            .expect("teardown coordinator state mutex poisoned")
            .active
            .retain(|active| active.key != *key);
    }
}

impl Drop for TeardownCoordinator {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn completion_key(ticket: &TeardownTicket) -> TeardownCompletionKey {
    TeardownCompletionKey::new(ticket.action_epoch(), ticket.fence().clone())
}

async fn run_cleanup(work: CleanupWork) {
    let CleanupWork {
        ticket,
        key,
        cell,
        cancellation,
        effects,
        deadline,
        state,
        completion_store,
        completed_operation_capacity,
    } = work;
    let fallback_ticket = ticket.clone();
    let report = match AssertUnwindSafe(execute_cleanup(
        ticket,
        effects,
        deadline,
        Arc::clone(&cancellation),
    ))
    .catch_unwind()
    .await
    {
        Ok(report) => report,
        Err(payload) => panic_report(
            fallback_ticket.clone(),
            format!("teardown worker panicked: {}", panic_detail(payload)),
        ),
    };
    let original_report = report.clone();
    let handoff = AssertUnwindSafe(handoff_completed_cleanup(
        &state,
        completion_store,
        completed_operation_capacity,
        key.clone(),
        Arc::clone(&cell),
        report,
        deadline.absolute,
    ))
    .catch_unwind()
    .await;
    let report = match handoff {
        Ok(report) => report,
        Err(payload) => {
            let report = original_report.with_handoff_error(format!(
                "completed teardown handoff panicked: {}",
                panic_detail(payload),
            ));
            retain_completed_cleanup(&state, completed_operation_capacity, key, Arc::clone(&cell));
            report
        }
    };
    cell.finish(report);
}

async fn persist_completion(
    completion_store: TeardownCompletionStore,
    key: &TeardownCompletionKey,
    report: &TeardownReport,
    absolute_deadline: Instant,
) -> Result<(), String> {
    checked_remaining_until(absolute_deadline, "completion persistence")?;
    let key = key.clone();
    let report = report.clone();
    let result = std::panic::catch_unwind(AssertUnwindSafe(|| {
        completion_store.persist(&key, &report, absolute_deadline)
    }))
    .map_err(|payload| format!("completion persistence panicked: {}", panic_detail(payload)))?;
    result?;
    checked_remaining_until(absolute_deadline, "completion persistence")?;
    Ok(())
}

async fn handoff_completed_cleanup(
    state: &Arc<Mutex<CoordinatorState>>,
    completion_store: TeardownCompletionStore,
    completed_operation_capacity: usize,
    key: TeardownCompletionKey,
    cell: Arc<CleanupCell>,
    report: TeardownReport,
    absolute_deadline: Instant,
) -> TeardownReport {
    if let Err(detail) =
        persist_completion(completion_store, &key, &report, absolute_deadline).await
    {
        let report = report.with_handoff_error(format!(
            "completed teardown handoff failed: {}",
            sanitize_text(&detail)
        ));
        retain_completed_cleanup(state, completed_operation_capacity, key, cell);
        return report;
    }

    retain_completed_cleanup(state, completed_operation_capacity, key, cell);
    report
}

fn retain_completed_cleanup(
    state: &Arc<Mutex<CoordinatorState>>,
    completed_operation_capacity: usize,
    key: TeardownCompletionKey,
    cell: Arc<CleanupCell>,
) {
    let mut state = state
        .lock()
        .expect("teardown coordinator state mutex poisoned");
    state.active.retain(|active| active.key != key);
    if state.completed.iter().any(|completed| completed.key == key) {
        return;
    }
    if state.completed.len() >= completed_operation_capacity {
        state.completed.pop_front();
    }
    state.completed.push_back(CompletedCleanup { key, cell });
}

fn validate_receipt(
    ticket: &TeardownTicket,
    receipt: &AdmissionReceipt,
) -> Result<(), TeardownReject> {
    if receipt.state() != AdmissionState::Closing || receipt.scope() != ticket.scope() {
        return Err(TeardownReject::NonClosingScope);
    }
    if receipt.action_epoch() != ticket.action_epoch() {
        return Err(TeardownReject::StaleEpoch {
            expected: receipt.action_epoch(),
            actual: ticket.action_epoch(),
        });
    }
    if receipt.fence() != ticket.fence() {
        return Err(TeardownReject::FenceMismatch);
    }
    Ok(())
}

#[derive(Debug, Clone, Copy)]
enum EffectCall {
    Drain,
    CooperativeClose,
    InterruptOrSafeClose,
    TerminateTree,
    SettleActiveProcessZero,
    DetachAfterZero,
    ReconcilePorts,
    PersistSettlement,
    ReleaseStoppedExact,
}

async fn execute_cleanup(
    ticket: TeardownTicket,
    effects: Arc<dyn TeardownEffects>,
    deadline: CleanupDeadline,
    cancellation: Arc<CancellationToken>,
) -> TeardownReport {
    let mut attempted_stages = Vec::new();
    let mut errors = Vec::new();

    if cancellation.is_requested() {
        return waiter_failure_report(
            ticket,
            "teardown cleanup cancellation requested before execution",
        );
    }

    attempted_stages.push(TeardownStage::Drain);
    collect_stage_result(
        bounded_stage(
            TeardownStage::Drain,
            deadline,
            Arc::clone(&effects),
            &ticket,
            EffectCall::Drain,
            Arc::clone(&cancellation),
        )
        .await,
        &mut errors,
        TeardownStage::Drain,
    );

    attempted_stages.push(TeardownStage::CooperativeClose);
    collect_stage_result(
        bounded_stage(
            TeardownStage::CooperativeClose,
            deadline,
            Arc::clone(&effects),
            &ticket,
            EffectCall::CooperativeClose,
            Arc::clone(&cancellation),
        )
        .await,
        &mut errors,
        TeardownStage::CooperativeClose,
    );

    attempted_stages.push(TeardownStage::CooperativeWait);
    let cooperative = bounded_wait(
        TeardownStage::CooperativeWait,
        deadline,
        Arc::clone(&effects),
        &ticket,
        WaitStage::CooperativeGrace,
        Arc::clone(&cancellation),
    )
    .await;
    if try_settle_after_wait(
        &ticket,
        cooperative,
        &effects,
        deadline,
        &mut attempted_stages,
        &mut errors,
        Arc::clone(&cancellation),
    )
    .await
    {
        return settle_after_zero(
            ticket,
            effects,
            deadline,
            attempted_stages,
            errors,
            cancellation,
        )
        .await;
    }

    attempted_stages.push(TeardownStage::InterruptOrSafeClose);
    collect_stage_result(
        bounded_stage(
            TeardownStage::InterruptOrSafeClose,
            deadline,
            Arc::clone(&effects),
            &ticket,
            EffectCall::InterruptOrSafeClose,
            Arc::clone(&cancellation),
        )
        .await,
        &mut errors,
        TeardownStage::InterruptOrSafeClose,
    );

    attempted_stages.push(TeardownStage::InterruptWait);
    let interrupted = bounded_wait(
        TeardownStage::InterruptWait,
        deadline,
        Arc::clone(&effects),
        &ticket,
        WaitStage::InterruptGrace,
        Arc::clone(&cancellation),
    )
    .await;
    if try_settle_after_wait(
        &ticket,
        interrupted,
        &effects,
        deadline,
        &mut attempted_stages,
        &mut errors,
        Arc::clone(&cancellation),
    )
    .await
    {
        return settle_after_zero(
            ticket,
            effects,
            deadline,
            attempted_stages,
            errors,
            cancellation,
        )
        .await;
    }

    attempted_stages.push(TeardownStage::TerminateTree);
    collect_stage_result(
        bounded_stage(
            TeardownStage::TerminateTree,
            deadline,
            Arc::clone(&effects),
            &ticket,
            EffectCall::TerminateTree,
            Arc::clone(&cancellation),
        )
        .await,
        &mut errors,
        TeardownStage::TerminateTree,
    );

    attempted_stages.push(TeardownStage::TerminationWait);
    let terminated = bounded_wait(
        TeardownStage::TerminationWait,
        deadline,
        Arc::clone(&effects),
        &ticket,
        WaitStage::Termination,
        Arc::clone(&cancellation),
    )
    .await;
    if try_settle_after_wait(
        &ticket,
        terminated,
        &effects,
        deadline,
        &mut attempted_stages,
        &mut errors,
        Arc::clone(&cancellation),
    )
    .await
    {
        return settle_after_zero(
            ticket,
            effects,
            deadline,
            attempted_stages,
            errors,
            cancellation,
        )
        .await;
    }

    let adapter_residue = bounded_residue(
        Arc::clone(&effects),
        &ticket,
        deadline,
        &mut errors,
        Arc::clone(&cancellation),
    )
    .await;
    let outcome = if errors.is_empty() {
        TeardownOutcome::Leaked
    } else {
        TeardownOutcome::CleanupFailed
    };
    let residue = required_residue(
        &ticket,
        &attempted_stages,
        &errors,
        outcome,
        adapter_residue,
    );
    TeardownReport {
        ticket,
        outcome,
        attempted_stages,
        errors,
        residue,
    }
}

async fn bounded_stage(
    stage: TeardownStage,
    deadline: CleanupDeadline,
    effects: Arc<dyn TeardownEffects>,
    ticket: &TeardownTicket,
    call: EffectCall,
    cancellation: Arc<CancellationToken>,
) -> StageResult {
    if cancellation.is_requested() {
        return StageResult::Failed {
            detail: format!("{stage:?}: teardown cleanup cancellation requested"),
        };
    }
    let remaining = match deadline.check(&format!("{stage:?}")) {
        Ok(remaining) => remaining,
        Err(detail) => return StageResult::Failed { detail },
    };
    let future = match std::panic::catch_unwind(AssertUnwindSafe(|| {
        stage_future(effects.as_ref(), ticket, call)
    })) {
        Ok(Ok(future)) => future,
        Ok(Err(detail)) => return StageResult::Failed { detail },
        Err(payload) => {
            return StageResult::Failed {
                detail: format!("{stage:?} panicked: {}", panic_detail(payload)),
            };
        }
    };
    tokio::select! {
        _ = cancellation.cancelled() => StageResult::Failed {
            detail: format!("{stage:?}: teardown cleanup cancellation requested"),
        },
        result = tokio::time::timeout(remaining, AssertUnwindSafe(future).catch_unwind()) => {
            match result {
                Ok(Ok(result)) => match deadline.check(&format!("{stage:?}")) {
                    Ok(_) => result,
                    Err(detail) => StageResult::Failed { detail },
                },
                Ok(Err(payload)) => StageResult::Failed {
                    detail: format!("{stage:?} panicked: {}", panic_detail(payload)),
                },
                Err(_) => StageResult::Failed {
                    detail: format!("{stage:?} timeout after {remaining:?}"),
                },
            }
        }
    }
}

async fn bounded_wait(
    stage: TeardownStage,
    deadline: CleanupDeadline,
    effects: Arc<dyn TeardownEffects>,
    ticket: &TeardownTicket,
    wait_stage: WaitStage,
    cancellation: Arc<CancellationToken>,
) -> WaitResult {
    if cancellation.is_requested() {
        return WaitResult::Failed {
            detail: format!("{stage:?}: teardown cleanup cancellation requested"),
        };
    }
    let remaining = match deadline.check(&format!("{stage:?}")) {
        Ok(remaining) => remaining,
        Err(detail) => return WaitResult::Failed { detail },
    };
    let future = match std::panic::catch_unwind(AssertUnwindSafe(|| {
        effects.wait_for_zero(ticket, wait_stage, deadline.effect())
    })) {
        Ok(future) => future,
        Err(payload) => {
            return WaitResult::Failed {
                detail: format!("{stage:?} panicked: {}", panic_detail(payload)),
            };
        }
    };
    tokio::select! {
        _ = cancellation.cancelled() => WaitResult::Failed {
            detail: format!("{stage:?}: teardown cleanup cancellation requested"),
        },
        result = tokio::time::timeout(remaining, AssertUnwindSafe(future).catch_unwind()) => {
            match result {
                Ok(Ok(result)) => match deadline.check(&format!("{stage:?}")) {
                    Ok(_) => result,
                    Err(detail) => WaitResult::Failed { detail },
                },
                Ok(Err(payload)) => WaitResult::Failed {
                    detail: format!("{stage:?} panicked: {}", panic_detail(payload)),
                },
                Err(_) => WaitResult::Failed {
                    detail: format!("{stage:?} timeout after {remaining:?}"),
                },
            }
        }
    }
}

async fn bounded_residue(
    effects: Arc<dyn TeardownEffects>,
    ticket: &TeardownTicket,
    deadline: CleanupDeadline,
    errors: &mut Vec<String>,
    cancellation: Arc<CancellationToken>,
) -> Option<ResidueEvidence> {
    if cancellation.is_requested() {
        push_bounded_error(
            errors,
            "Residue: teardown cleanup cancellation requested".to_string(),
        );
        return None;
    }
    let remaining = match deadline.check("Residue") {
        Ok(remaining) => remaining,
        Err(detail) => {
            push_bounded_error(errors, format!("Residue: {detail}"));
            return None;
        }
    };
    let future = match std::panic::catch_unwind(AssertUnwindSafe(|| effects.residue(ticket))) {
        Ok(future) => future,
        Err(payload) => {
            push_bounded_error(
                errors,
                format!("Residue panicked: {}", panic_detail(payload)),
            );
            return None;
        }
    };
    tokio::select! {
        _ = cancellation.cancelled() => {
            push_bounded_error(
                errors,
                "Residue: teardown cleanup cancellation requested".to_string(),
            );
            None
        }
        result = tokio::time::timeout(remaining, AssertUnwindSafe(future).catch_unwind()) => {
            match result {
                Ok(Ok(residue)) => match deadline.check("Residue") {
                    Ok(_) => residue,
                    Err(detail) => {
                        push_bounded_error(errors, format!("Residue: {detail}"));
                        None
                    }
                },
                Ok(Err(payload)) => {
                    push_bounded_error(
                        errors,
                        format!("Residue panicked: {}", panic_detail(payload)),
                    );
                    None
                }
                Err(_) => {
                    push_bounded_error(errors, format!("Residue timeout after {remaining:?}"));
                    None
                }
            }
        }
    }
}

fn stage_future<'a>(
    effects: &'a dyn TeardownEffects,
    ticket: &'a TeardownTicket,
    call: EffectCall,
) -> Result<BoxFuture<'a, StageResult>, String> {
    Ok(match call {
        EffectCall::Drain => effects.drain(ticket),
        EffectCall::CooperativeClose => effects.cooperative_close(ticket),
        EffectCall::InterruptOrSafeClose => effects.interrupt_or_safe_close(ticket),
        EffectCall::TerminateTree => effects.terminate_tree(ticket),
        EffectCall::SettleActiveProcessZero => effects.settle_active_process_zero(ticket),
        EffectCall::DetachAfterZero => effects.detach_after_zero(ticket),
        EffectCall::ReconcilePorts => effects.reconcile_ports(ticket),
        EffectCall::PersistSettlement => effects.persist_settlement(ticket),
        EffectCall::ReleaseStoppedExact => effects.release_stopped_exact(ticket),
    })
}

fn collect_stage_result(result: StageResult, errors: &mut Vec<String>, stage: TeardownStage) {
    if let StageResult::Failed { detail } = result {
        push_bounded_error(errors, format!("{stage:?}: {}", sanitize_text(&detail)));
    }
}

fn push_bounded_error(errors: &mut Vec<String>, detail: String) {
    if errors.len() < MAX_TEARDOWN_ERRORS {
        errors.push(detail);
    }
}

async fn try_settle_after_wait(
    ticket: &TeardownTicket,
    result: WaitResult,
    effects: &Arc<dyn TeardownEffects>,
    deadline: CleanupDeadline,
    attempted_stages: &mut Vec<TeardownStage>,
    errors: &mut Vec<String>,
    cancellation: Arc<CancellationToken>,
) -> bool {
    match result {
        WaitResult::Zero => {
            attempted_stages.push(TeardownStage::SettleActiveProcessZero);
            match bounded_stage(
                TeardownStage::SettleActiveProcessZero,
                deadline,
                Arc::clone(effects),
                ticket,
                EffectCall::SettleActiveProcessZero,
                Arc::clone(&cancellation),
            )
            .await
            {
                StageResult::Completed => true,
                StageResult::Failed { detail } => {
                    push_bounded_error(
                        errors,
                        format!("SettleActiveProcessZero: {}", sanitize_text(&detail)),
                    );
                    false
                }
            }
        }
        WaitResult::TimedOut => false,
        WaitResult::Failed { detail } => {
            push_bounded_error(
                errors,
                format!("zero wait failed: {}", sanitize_text(&detail)),
            );
            false
        }
    }
}

async fn settle_after_zero(
    ticket: TeardownTicket,
    effects: Arc<dyn TeardownEffects>,
    deadline: CleanupDeadline,
    mut attempted_stages: Vec<TeardownStage>,
    errors: Vec<String>,
    cancellation: Arc<CancellationToken>,
) -> TeardownReport {
    let mut errors = errors;
    attempted_stages.push(TeardownStage::DetachAfterZero);
    let detach = bounded_stage(
        TeardownStage::DetachAfterZero,
        deadline,
        Arc::clone(&effects),
        &ticket,
        EffectCall::DetachAfterZero,
        Arc::clone(&cancellation),
    )
    .await;
    if let StageResult::Failed { detail } = detach {
        push_bounded_error(
            &mut errors,
            format!("DetachAfterZero: {}", sanitize_text(&detail)),
        );
        return failed_post_zero_report(
            ticket,
            effects,
            deadline,
            attempted_stages,
            errors,
            cancellation,
        )
        .await;
    }
    attempted_stages.push(TeardownStage::ReconcilePorts);
    let reconcile = bounded_stage(
        TeardownStage::ReconcilePorts,
        deadline,
        Arc::clone(&effects),
        &ticket,
        EffectCall::ReconcilePorts,
        Arc::clone(&cancellation),
    )
    .await;
    if let StageResult::Failed { detail } = reconcile {
        push_bounded_error(
            &mut errors,
            format!("ReconcilePorts: {}", sanitize_text(&detail)),
        );
        return failed_post_zero_report(
            ticket,
            effects,
            deadline,
            attempted_stages,
            errors,
            cancellation,
        )
        .await;
    }
    attempted_stages.push(TeardownStage::PersistSettlement);
    let persist = bounded_stage(
        TeardownStage::PersistSettlement,
        deadline,
        Arc::clone(&effects),
        &ticket,
        EffectCall::PersistSettlement,
        Arc::clone(&cancellation),
    )
    .await;
    if let StageResult::Failed { detail } = persist {
        push_bounded_error(
            &mut errors,
            format!("PersistSettlement: {}", sanitize_text(&detail)),
        );
        return failed_post_zero_report(
            ticket,
            effects,
            deadline,
            attempted_stages,
            errors,
            cancellation,
        )
        .await;
    }
    attempted_stages.push(TeardownStage::ReleaseStoppedExact);
    let release = bounded_stage(
        TeardownStage::ReleaseStoppedExact,
        deadline,
        Arc::clone(&effects),
        &ticket,
        EffectCall::ReleaseStoppedExact,
        Arc::clone(&cancellation),
    )
    .await;
    let outcome = match release {
        StageResult::Completed if errors.is_empty() => TeardownOutcome::Closed,
        StageResult::Completed => TeardownOutcome::CleanupFailed,
        StageResult::Failed { detail } => {
            push_bounded_error(
                &mut errors,
                format!("ReleaseStoppedExact: {}", sanitize_text(&detail)),
            );
            TeardownOutcome::CleanupFailed
        }
    };
    let adapter_residue = if outcome != TeardownOutcome::Closed {
        bounded_residue(
            Arc::clone(&effects),
            &ticket,
            deadline,
            &mut errors,
            cancellation,
        )
        .await
    } else {
        None
    };
    let residue = if outcome != TeardownOutcome::Closed {
        required_residue(
            &ticket,
            &attempted_stages,
            &errors,
            outcome,
            adapter_residue,
        )
    } else {
        None
    };
    TeardownReport {
        ticket,
        outcome,
        attempted_stages,
        errors,
        residue,
    }
}

async fn failed_post_zero_report(
    ticket: TeardownTicket,
    effects: Arc<dyn TeardownEffects>,
    deadline: CleanupDeadline,
    attempted_stages: Vec<TeardownStage>,
    mut errors: Vec<String>,
    cancellation: Arc<CancellationToken>,
) -> TeardownReport {
    // Never release the exact Job authority after a failed detach, port
    // reconciliation, or durable host settlement. The owning terminal keeps
    // the Job and actor handles available for an exact retry; its Drop guard
    // fails closed if the host nevertheless abandons that authority.
    let adapter_residue =
        bounded_residue(effects, &ticket, deadline, &mut errors, cancellation).await;
    let outcome = TeardownOutcome::CleanupFailed;
    let residue = required_residue(
        &ticket,
        &attempted_stages,
        &errors,
        outcome,
        adapter_residue,
    );
    TeardownReport {
        ticket,
        outcome,
        attempted_stages,
        errors,
        residue,
    }
}

fn required_residue(
    ticket: &TeardownTicket,
    attempted_stages: &[TeardownStage],
    errors: &[String],
    outcome: TeardownOutcome,
    residue: Option<ResidueEvidence>,
) -> Option<ResidueEvidence> {
    let fallback = {
        let root = ticket.fence().root();
        let last_stage = attempted_stages
            .last()
            .map(|stage| format!("{stage:?}"))
            .unwrap_or_else(|| "<unavailable: no lifecycle stage recorded>".to_string());
        let detail = errors
            .last()
            .map(|error| format!("; detail={error}"))
            .unwrap_or_default();
        ResidueEvidence::new(
            UNAVAILABLE_JOB_IDENTITY,
            root.id().pid(),
            root.id().creation_time_100ns(),
            root.canonical_executable().to_string_lossy(),
            UNAVAILABLE_ROOT_COMMAND,
            format!(
                "resource={}; state={outcome:?}; last_lifecycle_stage={last_stage}{detail}",
                ticket.resource_id()
            ),
            attempted_stages.to_vec(),
        )
    };
    let residue = residue.unwrap_or(fallback.clone());
    let last_lifecycle_event = if residue.last_lifecycle_event.is_empty() {
        fallback.last_lifecycle_event.clone()
    } else {
        format!(
            "resource={}; state={outcome:?}; last_lifecycle_event={}",
            ticket.resource_id(),
            &residue.last_lifecycle_event
        )
    };
    Some(ResidueEvidence::new(
        if evidence_unavailable(&residue.job_name) {
            &fallback.job_name
        } else {
            &residue.job_name
        },
        if residue.pid == 0 {
            fallback.pid
        } else {
            residue.pid
        },
        if residue.creation_time_100ns == 0 {
            fallback.creation_time_100ns
        } else {
            residue.creation_time_100ns
        },
        if evidence_unavailable(&residue.executable) {
            &fallback.executable
        } else {
            &residue.executable
        },
        if residue.command_label.is_empty() {
            &fallback.command_label
        } else {
            &residue.command_label
        },
        last_lifecycle_event,
        if residue.attempted_stages.is_empty() {
            fallback.attempted_stages
        } else {
            residue.attempted_stages
        },
    ))
}

fn evidence_unavailable(value: &str) -> bool {
    value.is_empty() || value.starts_with("<unavailable:")
}

fn waiter_failure_report(ticket: TeardownTicket, detail: &str) -> TeardownReport {
    let attempted_stages = Vec::new();
    let errors = vec![sanitize_text(detail)];
    let residue = required_residue(
        &ticket,
        &attempted_stages,
        &errors,
        TeardownOutcome::CleanupFailed,
        None,
    );
    TeardownReport {
        ticket,
        outcome: TeardownOutcome::CleanupFailed,
        attempted_stages,
        errors,
        residue,
    }
}

fn panic_report(ticket: TeardownTicket, detail: String) -> TeardownReport {
    waiter_failure_report(ticket, &detail)
}

fn panic_detail(payload: Box<dyn std::any::Any + Send>) -> String {
    if let Some(message) = payload.downcast_ref::<&str>() {
        return sanitize_text(message);
    }
    if let Some(message) = payload.downcast_ref::<String>() {
        return sanitize_text(message);
    }
    "unknown panic payload".to_string()
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::{
        checked_remaining_until, sanitize_text, TeardownExecutor, TeardownReject,
        MAX_EVIDENCE_TEXT_BYTES,
    };

    #[cfg(windows)]
    #[test]
    fn terminal_teardown_inputs_are_rejected_before_job_registration() {
        assert!(super::validate_terminal_teardown_inputs("", 1, &[]).is_err());
        assert!(super::validate_terminal_teardown_inputs("valid-session", 0, &[]).is_err());
        assert!(super::validate_terminal_teardown_inputs(
            "valid-session",
            1,
            &[0; super::MAX_MANAGED_TERMINAL_PORTS + 1]
        )
        .is_err());

        let normalized =
            super::validate_terminal_teardown_inputs("valid-session", 1, &[8080, 443, 8080])
                .expect("valid terminal teardown inputs");
        assert_eq!(normalized, vec![443, 8080]);
    }

    #[test]
    fn absolute_deadline_boundary_rejects_expired_operations() {
        let result = checked_remaining_until(Instant::now(), "test boundary");
        assert!(
            result.is_err(),
            "an expired operation must not cross its boundary"
        );
    }

    #[test]
    fn teardown_executor_normalizes_zero_worker_configuration() {
        let executor = TeardownExecutor::new(0, 1);
        assert_eq!(executor.inner().worker_capacity, 1);
        executor.shutdown();
    }

    #[test]
    fn executor_capacity_reservation_is_deadline_bounded() {
        let executor = TeardownExecutor::new(0, 2);
        let held = executor
            .reserve_many(2, Instant::now() + Duration::from_millis(50))
            .expect("initial executor capacity");
        let result = executor.reserve_many(2, Instant::now() + Duration::from_millis(5));
        assert!(matches!(
            result,
            Err(TeardownReject::CapacityTimeout { retained })
                if retained.tickets().is_empty() && retained.receipts().is_empty()
        ));
        drop(held);
        executor.shutdown();
    }

    #[test]
    fn sanitize_text_bounds_input_and_redacts_adversarial_secret_forms() {
        let secret = "multi-megabyte-secret-sentinel";
        let input = format!(
            "--api-key={secret} Authorization:Bearer {secret} token={secret} password {secret} secret:{secret} {}",
            "x".repeat(8 * 1024 * 1024)
        );
        let sanitized = sanitize_text(&input);

        assert!(sanitized.len() <= MAX_EVIDENCE_TEXT_BYTES);
        assert!(!sanitized.contains(secret));
        assert!(sanitized.contains("--api-key=<redacted>"));
        assert!(sanitized.contains("Authorization: <redacted>"));
        assert!(sanitized.contains("token=<redacted>"));
    }
}
