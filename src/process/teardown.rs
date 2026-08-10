//! Pure, generation-fenced process-tree teardown orchestration.
//!
//! This module deliberately stops at the TeardownEffects seam. The host and
//! the future terminal service provide that small runtime adapter; this core
//! owns admission ordering, exact-fence validation, escalation, bounded
//! concurrency, and waiter lifetime.

use std::collections::VecDeque;
use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use futures_util::FutureExt;
use tokio::sync::watch;

use crate::domain::id::{OperationId, ResourceId, TaskId};
use crate::process::identity::ProcessOwner;
use crate::process::registry::ManagedProcessFence;

pub const DEFAULT_CONFIGURED_CAPACITY: usize = 4;
pub const DEFAULT_COMPLETED_OPERATION_CAPACITY: usize = 256;
pub const DEFAULT_EXECUTOR_QUEUE_CAPACITY: usize = 256;

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
pub struct TeardownDeadline(u64);

impl TeardownDeadline {
    pub fn new(value: u64) -> Self {
        Self(value)
    }

    pub fn value(self) -> u64 {
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
        let now = self.started.elapsed().as_millis() as u64;
        TeardownDeadline::new(now.saturating_add(timeout.as_millis() as u64))
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
/// The Task 3.4 TerminalService/host implementation is intentionally not part
/// of this pure core. Its adapter must source the zero result from the
/// registry completion path and make `settle_active_process_zero` perform the
/// final exact-fence plus authoritative membership check. The coordinator's
/// `WaitResult::Zero` is deliberately not itself a proof-bearing constructor.
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
        self.errors.push(detail);
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
/// dispatch boundary. The in-memory implementation is the current Phase 3.7
/// store; a production durable backend can be added behind this module without
/// widening the public seam to untracked blocking code.
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
pub struct TeardownHostAdapters {
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
    /// Minted by the in-crate host after it has selected the production Job,
    /// admission, and monotonic-clock implementations. The constructor is
    /// crate-private so an external caller cannot provide an unbounded seam.
    #[allow(dead_code)]
    pub(crate) fn from_host(
        admission: Arc<dyn TeardownAdmission>,
        effects: Arc<dyn TeardownEffects>,
        clock: Arc<dyn TeardownClock>,
    ) -> Self {
        Self {
            admission,
            effects,
            clock,
        }
    }
}

#[derive(Debug, Default)]
struct CompletionStoreInner {
    reports: Mutex<VecDeque<(TeardownCompletionKey, TeardownReport)>>,
    persist_error: Mutex<Option<String>>,
    lookup_blocked: AtomicBool,
    lookup_started: AtomicUsize,
    persist_blocked: AtomicBool,
    persist_active: AtomicUsize,
    persist_max_active: AtomicUsize,
}

impl TeardownCompletionStore {
    pub fn new() -> Self {
        Self::default()
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
        Ok(self
            .inner
            .reports
            .lock()
            .expect("completion store reports")
            .iter()
            .find(|(stored_key, _)| stored_key == key)
            .map(|(_, report)| report.clone()))
    }

    fn persist(
        &self,
        key: &TeardownCompletionKey,
        report: &TeardownReport,
        absolute_deadline: Instant,
    ) -> Result<(), String> {
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
        Ok(())
    }
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
    fallback: TeardownReport,
}

impl CleanupCell {
    fn new(ticket: &TeardownTicket) -> Self {
        let (done, _receiver) = watch::channel(false);
        Self {
            result: Mutex::new(None),
            done,
            fallback: waiter_failure_report(ticket.clone(), "teardown waiter channel closed"),
        }
    }

    fn finish(&self, report: TeardownReport) {
        let mut result = self.result.lock().expect("teardown result mutex poisoned");
        if result.is_none() {
            *result = Some(report);
            self.done.send_replace(true);
        }
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
    executor_keepalive: Arc<TeardownExecutorKeepalive>,
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
        for handle in handles {
            if handle.thread().id() == current {
                continue;
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
                state.queue.extend(works);
                self.remaining = self.remaining.saturating_sub(count);
                self.inner.changed.notify_all();
                return Ok(());
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
    executor: Arc<TeardownExecutor>,
}

impl TeardownCoordinator {
    /// Build a coordinator from the host-issued concrete adapter set.
    ///
    /// The adapter set cannot be constructed by external crates; the host
    /// controls which bounded Job/IOCP operations cross this authority
    /// boundary.
    pub fn from_host(
        adapters: TeardownHostAdapters,
        configured_capacity: usize,
        budgets: TeardownBudgets,
        completed_operation_capacity: usize,
        completion_store: TeardownCompletionStore,
    ) -> Self {
        Self::with_configuration_and_completion_store(
            adapters.admission,
            adapters.effects,
            adapters.clock,
            configured_capacity,
            budgets,
            completed_operation_capacity,
            completion_store,
        )
    }

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

    pub(crate) fn with_configuration_and_completion_store(
        admission: Arc<dyn TeardownAdmission>,
        effects: Arc<dyn TeardownEffects>,
        clock: Arc<dyn TeardownClock>,
        configured_capacity: usize,
        budgets: TeardownBudgets,
        completed_operation_capacity: usize,
        completion_store: TeardownCompletionStore,
    ) -> Self {
        let configured_capacity = configured_capacity.max(1);
        let completed_operation_capacity = completed_operation_capacity.max(1);
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
            executor: TeardownExecutor::new(configured_capacity, DEFAULT_EXECUTOR_QUEUE_CAPACITY),
        }
    }

    /// Closes fresh admission and settles every queued or active waiter with
    /// a typed fail-closed report, then settles every fixed executor worker.
    /// Production adapters are sealed and deadline-bounded, so no effect or
    /// persistence mutation can outlive this shutdown boundary.
    pub fn shutdown(&self) {
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
            self.admission
                .rollback_admission_batch(tickets, receipts, deadline)
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
            self.admission
                .close_admission_batch(std::slice::from_ref(&ticket), deadline)
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
            self.admission
                .close_admission_batch(&fresh_tickets, deadline)
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
            executor_keepalive: Arc::clone(&self.executor.keepalive),
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
        let store = self.completion_store.clone();
        let lookup_key = key.clone();
        std::panic::catch_unwind(AssertUnwindSafe(|| {
            store.lookup(&lookup_key, deadline.absolute)
        }))
        .map_err(|payload| TeardownReject::CompletionLookupFailed {
            detail: format!("completion lookup panicked: {}", panic_detail(payload)),
        })?
        .map_err(|detail| TeardownReject::CompletionLookupFailed { detail })
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
        executor_keepalive,
        completed_operation_capacity,
    } = work;
    let _executor_keepalive = executor_keepalive;
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
    let key = key.clone();
    let report = report.clone();
    std::panic::catch_unwind(AssertUnwindSafe(|| {
        completion_store.persist(&key, &report, absolute_deadline)
    }))
    .map_err(|payload| format!("completion persistence panicked: {}", panic_detail(payload)))?
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
    let remaining = match deadline.remaining() {
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
                Ok(Ok(result)) => result,
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
    let remaining = match deadline.remaining() {
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
                Ok(Ok(result)) => result,
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
        errors.push("Residue: teardown cleanup cancellation requested".to_string());
        return None;
    }
    let remaining = match deadline.remaining() {
        Ok(remaining) => remaining,
        Err(detail) => {
            errors.push(format!("Residue: {detail}"));
            return None;
        }
    };
    let future = match std::panic::catch_unwind(AssertUnwindSafe(|| effects.residue(ticket))) {
        Ok(future) => future,
        Err(payload) => {
            errors.push(format!("Residue panicked: {}", panic_detail(payload)));
            return None;
        }
    };
    tokio::select! {
        _ = cancellation.cancelled() => {
            errors.push("Residue: teardown cleanup cancellation requested".to_string());
            None
        }
        result = tokio::time::timeout(remaining, AssertUnwindSafe(future).catch_unwind()) => {
            match result {
                Ok(Ok(residue)) => residue,
                Ok(Err(payload)) => {
                    errors.push(format!("Residue panicked: {}", panic_detail(payload)));
                    None
                }
                Err(_) => {
                    errors.push(format!("Residue timeout after {remaining:?}"));
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
        errors.push(format!("{stage:?}: {}", sanitize_text(&detail)));
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
                    errors.push(format!(
                        "SettleActiveProcessZero: {}",
                        sanitize_text(&detail)
                    ));
                    false
                }
            }
        }
        WaitResult::TimedOut => false,
        WaitResult::Failed { detail } => {
            errors.push(format!("zero wait failed: {}", sanitize_text(&detail)));
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
    collect_stage_result(
        bounded_stage(
            TeardownStage::DetachAfterZero,
            deadline,
            Arc::clone(&effects),
            &ticket,
            EffectCall::DetachAfterZero,
            Arc::clone(&cancellation),
        )
        .await,
        &mut errors,
        TeardownStage::DetachAfterZero,
    );
    attempted_stages.push(TeardownStage::ReconcilePorts);
    collect_stage_result(
        bounded_stage(
            TeardownStage::ReconcilePorts,
            deadline,
            Arc::clone(&effects),
            &ticket,
            EffectCall::ReconcilePorts,
            Arc::clone(&cancellation),
        )
        .await,
        &mut errors,
        TeardownStage::ReconcilePorts,
    );
    attempted_stages.push(TeardownStage::PersistSettlement);
    collect_stage_result(
        bounded_stage(
            TeardownStage::PersistSettlement,
            deadline,
            Arc::clone(&effects),
            &ticket,
            EffectCall::PersistSettlement,
            Arc::clone(&cancellation),
        )
        .await,
        &mut errors,
        TeardownStage::PersistSettlement,
    );
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
            errors.push(format!("ReleaseStoppedExact: {}", sanitize_text(&detail)));
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

    use super::{sanitize_text, TeardownExecutor, TeardownReject, MAX_EVIDENCE_TEXT_BYTES};

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
