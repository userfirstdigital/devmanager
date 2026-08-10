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
use std::sync::{mpsc, Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use futures_util::FutureExt;
use tokio::sync::{oneshot, watch};

use crate::domain::id::{OperationId, ResourceId, TaskId};
use crate::process::identity::ProcessOwner;
use crate::process::registry::ManagedProcessFence;

pub const DEFAULT_CONFIGURED_CAPACITY: usize = 4;
pub const DEFAULT_COMPLETED_OPERATION_CAPACITY: usize = 256;
pub const DEFAULT_EXECUTOR_QUEUE_CAPACITY: usize = 256;
const DEFAULT_DISPATCH_WORKER_CAPACITY: usize = 4;
const DEFAULT_DISPATCH_QUEUE_CAPACITY: usize = 256;

/// A boxed asynchronous operation used by the pure runtime seams.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

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
    Other { detail: String },
}

/// The only admission/state operation the coordinator needs from the host.
///
/// Implementations transition the requested scope to Closing and return the
/// exact state/fence they admitted. The coordinator verifies the receipt
/// before it invokes any process or terminal effect.
pub trait TeardownAdmission: Send + Sync + 'static {
    fn close_admission(
        &self,
        ticket: &TeardownTicket,
    ) -> Result<AdmissionReceipt, TeardownAdmissionError>;

    /// Atomically closes a complete scope before any branch is scheduled.
    ///
    /// Implementations must validate every ticket and receipt before changing
    /// any admission state. A failure therefore leaves the whole batch open.
    fn close_admission_batch(
        &self,
        tickets: &[TeardownTicket],
    ) -> Result<Vec<AdmissionReceipt>, TeardownAdmissionError>;

    /// Reopens exactly the admissions returned by a successful batch when no
    /// cleanup work could be submitted. Implementations must fence every
    /// ticket/receipt pair and leave the whole batch unchanged on failure.
    fn rollback_admission_batch(
        &self,
        tickets: &[TeardownTicket],
        receipts: &[AdmissionReceipt],
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
pub trait TeardownClock: Send + Sync + 'static {
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

    fn total(self) -> Duration {
        self.cooperative_grace
            .saturating_add(self.interrupt_grace)
            .saturating_add(self.termination)
    }
}

#[derive(Debug, Clone, Copy)]
struct CleanupDeadline {
    effect: TeardownDeadline,
    absolute: Instant,
}

impl CleanupDeadline {
    fn new(clock: &dyn TeardownClock, budgets: TeardownBudgets) -> Self {
        let total = budgets.total();
        Self {
            effect: clock.deadline(total),
            absolute: Instant::now() + total,
        }
    }

    fn effect(self) -> TeardownDeadline {
        self.effect
    }

    fn remaining(self) -> Duration {
        self.absolute.saturating_duration_since(Instant::now())
    }
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
pub trait TeardownEffects: Send + Sync + 'static {
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
    pub fn fail_persist_for_test(&self, detail: impl Into<String>) {
        *self
            .inner
            .persist_error
            .lock()
            .expect("completion store persist error") = Some(detail.into());
    }

    #[doc(hidden)]
    pub fn block_lookup_for_test(&self) {
        self.inner.lookup_blocked.store(true, Ordering::SeqCst);
    }

    #[doc(hidden)]
    pub fn release_lookup_for_test(&self) {
        self.inner.lookup_blocked.store(false, Ordering::SeqCst);
    }

    #[doc(hidden)]
    pub fn lookup_started_for_test(&self) -> usize {
        self.inner.lookup_started.load(Ordering::SeqCst)
    }

    #[doc(hidden)]
    pub fn block_persist_for_test(&self) {
        self.inner.persist_blocked.store(true, Ordering::SeqCst);
    }

    #[doc(hidden)]
    pub fn release_persist_for_test(&self) {
        self.inner.persist_blocked.store(false, Ordering::SeqCst);
    }

    #[doc(hidden)]
    pub fn persist_max_active_for_test(&self) -> usize {
        self.inner.persist_max_active.load(Ordering::SeqCst)
    }

    #[doc(hidden)]
    pub fn retained_count_for_test(&self) -> usize {
        self.inner
            .reports
            .lock()
            .expect("completion store reports")
            .len()
    }

    fn lookup(&self, key: &TeardownCompletionKey) -> Result<Option<TeardownReport>, String> {
        self.inner.lookup_started.fetch_add(1, Ordering::SeqCst);
        while self.inner.lookup_blocked.load(Ordering::SeqCst) {
            thread::sleep(Duration::from_millis(1));
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

    fn persist(&self, key: &TeardownCompletionKey, report: &TeardownReport) -> Result<(), String> {
        let active = self.inner.persist_active.fetch_add(1, Ordering::SeqCst) + 1;
        self.inner
            .persist_max_active
            .fetch_max(active, Ordering::SeqCst);
        while self.inner.persist_blocked.load(Ordering::SeqCst) {
            thread::sleep(Duration::from_millis(1));
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
}

impl From<TeardownAdmissionError> for TeardownReject {
    fn from(error: TeardownAdmissionError) -> Self {
        match error {
            TeardownAdmissionError::StaleEpoch { expected, actual } => {
                Self::StaleEpoch { expected, actual }
            }
            TeardownAdmissionError::FenceMismatch => Self::FenceMismatch,
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

#[derive(Debug, Clone, PartialEq, Eq)]
enum DispatchReject {
    Closed,
    QueueFull,
}

impl DispatchReject {
    fn detail(&self) -> &'static str {
        match self {
            Self::Closed => "bounded teardown dispatch is closed",
            Self::QueueFull => "bounded teardown dispatch queue is full",
        }
    }
}

type DispatchOperation = Box<dyn FnOnce(Arc<CancellationToken>) -> BoxFuture<'static, ()> + Send>;

struct DispatchTask {
    cancellation: Arc<CancellationToken>,
    operation: Option<DispatchOperation>,
}

struct DispatchQueueState {
    queue: VecDeque<DispatchTask>,
    active: usize,
    closed: bool,
}

struct DispatchInner {
    state: Mutex<DispatchQueueState>,
    changed: Condvar,
    queue_capacity: usize,
}

/// Fixed-capacity dispatch for every potentially blocking teardown boundary.
///
/// The worker owns future construction and polling. A timeout drops only the
/// response wait and requests cancellation; arbitrary Rust code cannot be
/// forcibly killed safely, so a stuck operation keeps its fixed worker slot
/// until it returns. Explicit shutdown joins that slot before returning.
struct BlockingDispatchPool {
    inner: Arc<DispatchInner>,
    workers: Mutex<Vec<JoinHandle<()>>>,
    worker_capacity: usize,
}

struct AsyncDispatch<T> {
    receiver: oneshot::Receiver<Result<T, String>>,
    cancellation: Arc<CancellationToken>,
}

struct SyncDispatch<T> {
    receiver: mpsc::Receiver<Result<T, String>>,
    cancellation: Arc<CancellationToken>,
}

impl BlockingDispatchPool {
    fn new(worker_capacity: usize, queue_capacity: usize) -> Arc<Self> {
        let worker_capacity = worker_capacity.max(1);
        let inner = Arc::new(DispatchInner {
            state: Mutex::new(DispatchQueueState {
                queue: VecDeque::with_capacity(queue_capacity),
                active: 0,
                closed: false,
            }),
            changed: Condvar::new(),
            queue_capacity,
        });
        let pool = Arc::new(Self {
            inner: Arc::clone(&inner),
            workers: Mutex::new(Vec::with_capacity(worker_capacity)),
            worker_capacity,
        });
        let mut workers = pool
            .workers
            .lock()
            .expect("dispatch workers mutex poisoned");
        for index in 0..worker_capacity {
            let worker_inner = Arc::clone(&inner);
            workers.push(
                thread::Builder::new()
                    .name(format!("devmanager-teardown-dispatch-{index}"))
                    .spawn(move || dispatch_worker(worker_inner))
                    .expect("spawn bounded teardown dispatch worker"),
            );
        }
        drop(workers);
        pool
    }

    /// Closes admission, cancels queued work, and joins every worker before
    /// returning. A blocking adapter may not be forcefully killed safely, so
    /// shutdown deliberately waits for that fixed worker slot to finish; it
    /// never detaches a worker that can still mutate effects or persistence.
    fn shutdown(&self) {
        let queued = {
            let mut state = self
                .inner
                .state
                .lock()
                .expect("dispatch state mutex poisoned");
            state.closed = true;
            let queued: Vec<_> = state.queue.drain(..).collect();
            self.inner.changed.notify_all();
            queued
        };
        for task in queued {
            task.cancellation.request();
        }

        let mut workers = self
            .workers
            .lock()
            .expect("dispatch workers mutex poisoned");
        let handles = std::mem::take(&mut *workers);
        drop(workers);

        let current = thread::current().id();
        for handle in handles {
            if handle.thread().id() == current {
                continue;
            }
            let _ = handle.join();
        }
    }

    fn enqueue(&self, task: DispatchTask) -> Result<(), DispatchReject> {
        let mut state = self
            .inner
            .state
            .lock()
            .expect("dispatch state mutex poisoned");
        if state.closed {
            return Err(DispatchReject::Closed);
        }
        if state.queue.len() >= self.inner.queue_capacity {
            return Err(DispatchReject::QueueFull);
        }
        state.queue.push_back(task);
        self.inner.changed.notify_all();
        Ok(())
    }

    fn submit_async<T, F>(
        self: &Arc<Self>,
        operation: F,
    ) -> Result<AsyncDispatch<T>, DispatchReject>
    where
        T: Send + 'static,
        F: FnOnce(Arc<CancellationToken>) -> BoxFuture<'static, Result<T, String>> + Send + 'static,
    {
        let (sender, receiver) = oneshot::channel();
        let cancellation = CancellationToken::new();
        let task_cancellation = Arc::clone(&cancellation);
        let task_operation: DispatchOperation = Box::new(move |dispatch_cancellation| {
            Box::pin(async move {
                if dispatch_cancellation.is_requested() {
                    let _ = sender.send(Err("bounded teardown dispatch cancelled".to_string()));
                    return;
                }
                let result = match std::panic::catch_unwind(AssertUnwindSafe(|| {
                    operation(Arc::clone(&dispatch_cancellation))
                })) {
                    Ok(future) => match AssertUnwindSafe(future).catch_unwind().await {
                        Ok(result) => result,
                        Err(payload) => Err(format!(
                            "bounded teardown dispatch panicked: {}",
                            panic_detail(payload)
                        )),
                    },
                    Err(payload) => Err(format!(
                        "bounded teardown dispatch panicked: {}",
                        panic_detail(payload)
                    )),
                };
                let _ = sender.send(result);
            })
        });
        let task = DispatchTask {
            cancellation: task_cancellation,
            operation: Some(task_operation),
        };
        self.enqueue(task)?;
        Ok(AsyncDispatch {
            receiver,
            cancellation,
        })
    }

    fn submit_sync<T, F>(self: &Arc<Self>, operation: F) -> Result<SyncDispatch<T>, DispatchReject>
    where
        T: Send + 'static,
        F: FnOnce(Arc<CancellationToken>) -> Result<T, String> + Send + 'static,
    {
        let (sender, receiver) = mpsc::sync_channel(1);
        let cancellation = CancellationToken::new();
        let task_cancellation = Arc::clone(&cancellation);
        let task_operation: DispatchOperation = Box::new(move |dispatch_cancellation| {
            Box::pin(async move {
                if dispatch_cancellation.is_requested() {
                    let _ = sender.send(Err("bounded teardown dispatch cancelled".to_string()));
                    return;
                }
                let result = std::panic::catch_unwind(AssertUnwindSafe(|| {
                    operation(Arc::clone(&dispatch_cancellation))
                }))
                .map_err(|payload| {
                    format!(
                        "bounded teardown dispatch panicked: {}",
                        panic_detail(payload)
                    )
                })
                .and_then(|result| result);
                let _ = sender.send(result);
            })
        });
        let task = DispatchTask {
            cancellation: task_cancellation,
            operation: Some(task_operation),
        };
        self.enqueue(task)?;
        Ok(SyncDispatch {
            receiver,
            cancellation,
        })
    }
}

impl<T> AsyncDispatch<T> {
    async fn wait(self, budget: Duration, label: &str) -> Result<T, String> {
        match tokio::time::timeout(budget, self.receiver).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(format!("{label} dispatch response channel closed")),
            Err(_) => {
                self.cancellation.request();
                Err(format!("{label} timeout after {budget:?}"))
            }
        }
    }
}

impl<T> SyncDispatch<T> {
    fn wait(self, budget: Duration, label: &str) -> Result<T, String> {
        match self.receiver.recv_timeout(budget) {
            Ok(result) => result,
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                Err(format!("{label} dispatch response channel closed"))
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                self.cancellation.request();
                Err(format!("{label} timeout after {budget:?}"))
            }
        }
    }
}

fn dispatch_worker(inner: Arc<DispatchInner>) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("teardown dispatch runtime");
    loop {
        let task = {
            let mut state = inner.state.lock().expect("dispatch state mutex poisoned");
            loop {
                if let Some(task) = state.queue.pop_front() {
                    state.active += 1;
                    break Some(task);
                }
                if state.closed {
                    break None;
                }
                state = inner
                    .changed
                    .wait(state)
                    .expect("dispatch state mutex poisoned");
            }
        };
        let Some(mut task) = task else {
            return;
        };
        if let Some(operation) = task.operation.take() {
            runtime.block_on(operation(task.cancellation));
        }
        let mut state = inner.state.lock().expect("dispatch state mutex poisoned");
        state.active = state.active.saturating_sub(1);
        inner.changed.notify_all();
    }
}

impl Drop for BlockingDispatchPool {
    fn drop(&mut self) {
        self.shutdown();
        debug_assert!(self.worker_capacity > 0);
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
    clock: Arc<dyn TeardownClock>,
    budgets: TeardownBudgets,
    state: Arc<Mutex<CoordinatorState>>,
    completion_store: TeardownCompletionStore,
    dispatcher: Arc<BlockingDispatchPool>,
    completion_dispatcher: Arc<BlockingDispatchPool>,
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

    fn reserve_many(&self, count: usize) -> Result<ExecutorReservation, TeardownReject> {
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
            .saturating_add(self.inner().queue_capacity);
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
            state = self
                .keepalive
                .inner
                .changed
                .wait(state)
                .expect("teardown executor state mutex poisoned");
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

impl ExecutorReservation {
    fn submit_all(&mut self, works: Vec<CleanupWork>) -> Result<(), Vec<CleanupWork>> {
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
                return Err(works);
            }
            if count <= self.inner.queue_capacity.saturating_sub(state.queue.len()) {
                state.queue.extend(works);
                self.remaining = self.remaining.saturating_sub(count);
                self.inner.changed.notify_all();
                return Ok(());
            }
            state = self
                .inner
                .changed
                .wait(state)
                .expect("teardown executor state mutex poisoned");
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
    dispatcher: Arc<BlockingDispatchPool>,
    completion_dispatcher: Arc<BlockingDispatchPool>,
    lookup_dispatcher: Arc<BlockingDispatchPool>,
    state: Arc<Mutex<CoordinatorState>>,
    admission_serial: Mutex<()>,
    executor: Arc<TeardownExecutor>,
}

impl TeardownCoordinator {
    pub fn new(
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

    pub fn with_capacity(
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

    pub fn with_configuration(
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

    pub fn with_configuration_and_completion_store(
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
            dispatcher: BlockingDispatchPool::new(
                DEFAULT_DISPATCH_WORKER_CAPACITY,
                DEFAULT_DISPATCH_QUEUE_CAPACITY,
            ),
            completion_dispatcher: BlockingDispatchPool::new(
                DEFAULT_DISPATCH_WORKER_CAPACITY,
                DEFAULT_DISPATCH_QUEUE_CAPACITY,
            ),
            lookup_dispatcher: BlockingDispatchPool::new(1, DEFAULT_DISPATCH_QUEUE_CAPACITY),
            state: Arc::new(Mutex::new(CoordinatorState::default())),
            admission_serial: Mutex::new(()),
            executor: TeardownExecutor::new(configured_capacity, DEFAULT_EXECUTOR_QUEUE_CAPACITY),
        }
    }

    /// Closes fresh admission and settles every queued or active waiter with
    /// a typed fail-closed report, then joins every executor and dispatch
    /// worker. In-flight effect/store code is allowed to finish its fixed
    /// worker slot before this method returns, so no mutation can outlive the
    /// coordinator shutdown boundary.
    pub fn shutdown(&self) {
        self.executor.shutdown();
        self.dispatcher.shutdown();
        self.completion_dispatcher.shutdown();
        self.lookup_dispatcher.shutdown();
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
        rejection: TeardownReject,
    ) -> TeardownReject {
        if !receipts
            .iter()
            .any(|receipt| receipt.state() == AdmissionState::Closing)
        {
            return rejection;
        }
        match self.admission.rollback_admission_batch(tickets, receipts) {
            Ok(()) => rejection,
            Err(error) => TeardownReject::Admission(error),
        }
    }

    pub fn request(&self, ticket: TeardownTicket) -> Result<TeardownWaiter, TeardownReject> {
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
        if let Some(waiter) = self.lookup_completed(&ticket, &key)? {
            return Ok(waiter);
        }

        let mut reservation = self.executor.reserve_many(1)?;
        let rollback_tickets = [ticket.clone()];
        let receipts = self
            .admission
            .close_admission_batch(std::slice::from_ref(&ticket))
            .map_err(TeardownReject::from)?;
        if receipts.len() != 1 {
            return Err(self.rollback_rejection(
                &rollback_tickets,
                &receipts,
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
                    TeardownReject::Admission(TeardownAdmissionError::Other {
                        detail: "admission returned no receipt".to_string(),
                    }),
                ));
            }
        };
        if let Err(rejection) = validate_receipt(&ticket, receipt) {
            return Err(self.rollback_rejection(&rollback_tickets, &receipts, rejection));
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
        let work = self.cleanup_work(ticket, key.clone(), Arc::clone(&cell));
        if let Err(works) = reservation.submit_all(vec![work]) {
            for work in works {
                cancel_queued_cleanup(work);
            }
            self.remove_active(&key);
            return Err(self.rollback_rejection(
                &rollback_tickets,
                &rollback_receipts,
                TeardownReject::ExecutorClosed,
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
        if let Some(waiter) = self.lookup_completed_by_key(&key)? {
            return Ok(waiter);
        }
        Err(TeardownReject::NoMatchingCleanup)
    }

    pub fn request_batch(
        &self,
        tickets: Vec<TeardownTicket>,
    ) -> Result<TeardownBatchWaiter, TeardownReject> {
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
            } else if let Some(waiter) = self.lookup_completed(&ticket, &key)? {
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

        let mut reservation = self.executor.reserve_many(fresh.len())?;
        let fresh_tickets: Vec<TeardownTicket> =
            fresh.iter().map(|(_, ticket)| ticket.clone()).collect();
        let receipts = self
            .admission
            .close_admission_batch(&fresh_tickets)
            .map_err(TeardownReject::from)?;
        if receipts.len() != fresh.len() {
            return Err(self.rollback_rejection(
                &fresh_tickets,
                &receipts,
                TeardownReject::Admission(TeardownAdmissionError::Other {
                    detail: "admission returned an incomplete receipt batch".to_string(),
                }),
            ));
        }
        for ((_, ticket), receipt) in fresh.iter().zip(receipts.iter()) {
            if let Err(rejection) = validate_receipt(ticket, receipt) {
                return Err(self.rollback_rejection(&fresh_tickets, &receipts, rejection));
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
            works.push(self.cleanup_work(ticket, key, cell));
        }
        for key in fresh_duplicates {
            if let Some((_, cell)) = created.iter().find(|(created_key, _)| *created_key == key) {
                waiters.push(TeardownWaiter {
                    cell: Arc::clone(cell),
                });
            }
        }
        if let Err(works) = reservation.submit_all(works) {
            for work in works {
                cancel_queued_cleanup(work);
            }
            for (key, _) in &created {
                self.remove_active(key);
            }
            return Err(self.rollback_rejection(
                &fresh_tickets,
                &rollback_receipts,
                TeardownReject::ExecutorClosed,
            ));
        }
        Ok(TeardownBatchWaiter { waiters })
    }

    fn cleanup_work(
        &self,
        ticket: TeardownTicket,
        key: TeardownCompletionKey,
        cell: Arc<CleanupCell>,
    ) -> CleanupWork {
        CleanupWork {
            ticket,
            key,
            cell,
            effects: Arc::clone(&self.effects),
            clock: Arc::clone(&self.clock),
            budgets: self.budgets,
            state: Arc::clone(&self.state),
            completion_store: self.completion_store.clone(),
            dispatcher: Arc::clone(&self.dispatcher),
            completion_dispatcher: Arc::clone(&self.completion_dispatcher),
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
    ) -> Result<Option<TeardownWaiter>, TeardownReject> {
        self.lookup_completed_by_key_with_ticket(key, ticket)
    }

    fn lookup_completed_by_key(
        &self,
        key: &TeardownCompletionKey,
    ) -> Result<Option<TeardownWaiter>, TeardownReject> {
        match self.lookup_report(key)? {
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
    ) -> Result<Option<TeardownWaiter>, TeardownReject> {
        match self.lookup_report(key)? {
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
    ) -> Result<Option<TeardownReport>, TeardownReject> {
        let store = self.completion_store.clone();
        let lookup_key = key.clone();
        let dispatch = self
            .lookup_dispatcher
            .submit_sync(move |_dispatch_cancellation| store.lookup(&lookup_key))
            .map_err(|error| TeardownReject::CompletionLookupFailed {
                detail: error.detail().to_string(),
            })?;
        dispatch
            .wait(self.budgets.termination, "completion lookup")
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
        clock,
        budgets,
        state,
        completion_store,
        dispatcher,
        completion_dispatcher,
        executor_keepalive,
        completed_operation_capacity,
    } = work;
    let _executor_keepalive = executor_keepalive;
    let deadline = CleanupDeadline::new(clock.as_ref(), budgets);
    let fallback_ticket = ticket.clone();
    let report = match AssertUnwindSafe(execute_cleanup(
        ticket,
        effects,
        deadline,
        Arc::clone(&dispatcher),
        Arc::clone(&cancellation),
    ))
    .catch_unwind()
    .await
    {
        Ok(report) => report,
        Err(payload) => panic_report(
            fallback_ticket,
            format!("teardown worker panicked: {}", panic_detail(payload),),
        ),
    };
    let original_report = report.clone();
    let handoff = AssertUnwindSafe(handoff_completed_cleanup(
        &state,
        completion_store,
        completion_dispatcher,
        completed_operation_capacity,
        key.clone(),
        Arc::clone(&cell),
        report,
        deadline.remaining(),
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
    dispatcher: Arc<BlockingDispatchPool>,
    completion_store: TeardownCompletionStore,
    key: &TeardownCompletionKey,
    report: &TeardownReport,
    budget: Duration,
) -> Result<(), String> {
    let key = key.clone();
    let report = report.clone();
    let dispatch = dispatcher
        .submit_async(move |_dispatch_cancellation| {
            Box::pin(async move { completion_store.persist(&key, &report) })
        })
        .map_err(|error| error.detail().to_string())?;
    dispatch.wait(budget, "completion persistence").await
}

async fn handoff_completed_cleanup(
    state: &Arc<Mutex<CoordinatorState>>,
    completion_store: TeardownCompletionStore,
    dispatcher: Arc<BlockingDispatchPool>,
    completed_operation_capacity: usize,
    key: TeardownCompletionKey,
    cell: Arc<CleanupCell>,
    report: TeardownReport,
    persistence_budget: Duration,
) -> TeardownReport {
    if let Err(detail) = persist_completion(
        dispatcher,
        completion_store,
        &key,
        &report,
        persistence_budget,
    )
    .await
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

fn dispatch_stage(
    dispatcher: &Arc<BlockingDispatchPool>,
    effects: Arc<dyn TeardownEffects>,
    ticket: TeardownTicket,
    call: EffectCall,
    cancellation: Arc<CancellationToken>,
) -> Result<AsyncDispatch<StageResult>, String> {
    dispatcher
        .submit_async(move |dispatch_cancellation| {
            Box::pin(async move {
                if dispatch_cancellation.is_requested() || cancellation.is_requested() {
                    return Err("teardown stage dispatch cancellation requested".to_string());
                }
                let future = match call {
                    EffectCall::Drain => effects.drain(&ticket),
                    EffectCall::CooperativeClose => effects.cooperative_close(&ticket),
                    EffectCall::InterruptOrSafeClose => effects.interrupt_or_safe_close(&ticket),
                    EffectCall::TerminateTree => effects.terminate_tree(&ticket),
                    EffectCall::SettleActiveProcessZero => {
                        effects.settle_active_process_zero(&ticket)
                    }
                    EffectCall::DetachAfterZero => effects.detach_after_zero(&ticket),
                    EffectCall::ReconcilePorts => effects.reconcile_ports(&ticket),
                    EffectCall::PersistSettlement => effects.persist_settlement(&ticket),
                    EffectCall::ReleaseStoppedExact => effects.release_stopped_exact(&ticket),
                };
                tokio::select! {
                    _ = dispatch_cancellation.cancelled() => {
                        Err("teardown stage dispatch cancellation requested".to_string())
                    }
                    _ = cancellation.cancelled() => {
                        Err("teardown cleanup cancellation requested".to_string())
                    }
                    result = AssertUnwindSafe(future).catch_unwind() => match result {
                        Ok(result) => Ok(result),
                        Err(payload) => Err(format!("teardown stage panicked: {}", panic_detail(payload))),
                    }
                }
            })
        })
        .map_err(|error| error.detail().to_string())
}

fn dispatch_wait(
    dispatcher: &Arc<BlockingDispatchPool>,
    effects: Arc<dyn TeardownEffects>,
    ticket: TeardownTicket,
    stage: WaitStage,
    deadline: TeardownDeadline,
    cancellation: Arc<CancellationToken>,
) -> Result<AsyncDispatch<WaitResult>, String> {
    dispatcher
        .submit_async(move |dispatch_cancellation| {
            Box::pin(async move {
                if dispatch_cancellation.is_requested() || cancellation.is_requested() {
                    return Err("zero wait dispatch cancellation requested".to_string());
                }
                let future = effects.wait_for_zero(&ticket, stage, deadline);
                tokio::select! {
                    _ = dispatch_cancellation.cancelled() => {
                        Err("zero wait dispatch cancellation requested".to_string())
                    }
                    _ = cancellation.cancelled() => {
                        Err("teardown cleanup cancellation requested".to_string())
                    }
                    result = AssertUnwindSafe(future).catch_unwind() => match result {
                        Ok(result) => Ok(result),
                        Err(payload) => Err(format!("zero wait panicked: {}", panic_detail(payload))),
                    }
                }
            })
        })
        .map_err(|error| error.detail().to_string())
}

fn dispatch_residue(
    dispatcher: &Arc<BlockingDispatchPool>,
    effects: Arc<dyn TeardownEffects>,
    ticket: TeardownTicket,
    cancellation: Arc<CancellationToken>,
) -> Result<AsyncDispatch<Option<ResidueEvidence>>, String> {
    dispatcher
        .submit_async(move |dispatch_cancellation| {
            Box::pin(async move {
                if dispatch_cancellation.is_requested() || cancellation.is_requested() {
                    return Err("teardown residue dispatch cancellation requested".to_string());
                }
                let future = effects.residue(&ticket);
                tokio::select! {
                    _ = dispatch_cancellation.cancelled() => {
                        Err("teardown residue dispatch cancellation requested".to_string())
                    }
                    _ = cancellation.cancelled() => {
                        Err("teardown cleanup cancellation requested".to_string())
                    }
                    result = AssertUnwindSafe(future).catch_unwind() => match result {
                        Ok(result) => Ok(result),
                        Err(payload) => Err(format!("teardown residue panicked: {}", panic_detail(payload))),
                    }
                }
            })
        })
        .map_err(|error| error.detail().to_string())
}

async fn execute_cleanup(
    ticket: TeardownTicket,
    effects: Arc<dyn TeardownEffects>,
    deadline: CleanupDeadline,
    dispatcher: Arc<BlockingDispatchPool>,
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
            &dispatcher,
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
            &dispatcher,
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
        &dispatcher,
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
        &dispatcher,
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
            dispatcher,
            cancellation,
        )
        .await;
    }

    attempted_stages.push(TeardownStage::InterruptOrSafeClose);
    collect_stage_result(
        bounded_stage(
            TeardownStage::InterruptOrSafeClose,
            deadline,
            &dispatcher,
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
        &dispatcher,
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
        &dispatcher,
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
            dispatcher,
            cancellation,
        )
        .await;
    }

    attempted_stages.push(TeardownStage::TerminateTree);
    collect_stage_result(
        bounded_stage(
            TeardownStage::TerminateTree,
            deadline,
            &dispatcher,
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
        &dispatcher,
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
        &dispatcher,
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
            dispatcher,
            cancellation,
        )
        .await;
    }

    let adapter_residue = bounded_residue(
        &dispatcher,
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
    dispatcher: &Arc<BlockingDispatchPool>,
    effects: Arc<dyn TeardownEffects>,
    ticket: &TeardownTicket,
    call: EffectCall,
    cancellation: Arc<CancellationToken>,
) -> StageResult {
    let dispatch = match dispatch_stage(dispatcher, effects, ticket.clone(), call, cancellation) {
        Ok(dispatch) => dispatch,
        Err(detail) => {
            return StageResult::Failed {
                detail: format!("{stage:?}: {detail}"),
            };
        }
    };
    match dispatch
        .wait(deadline.remaining(), &format!("{stage:?}"))
        .await
    {
        Ok(result) => result,
        Err(detail) => StageResult::Failed { detail },
    }
}

async fn bounded_wait(
    stage: TeardownStage,
    deadline: CleanupDeadline,
    dispatcher: &Arc<BlockingDispatchPool>,
    effects: Arc<dyn TeardownEffects>,
    ticket: &TeardownTicket,
    wait_stage: WaitStage,
    cancellation: Arc<CancellationToken>,
) -> WaitResult {
    let dispatch = match dispatch_wait(
        dispatcher,
        effects,
        ticket.clone(),
        wait_stage,
        deadline.effect(),
        cancellation,
    ) {
        Ok(dispatch) => dispatch,
        Err(detail) => {
            return WaitResult::Failed {
                detail: format!("{stage:?}: {detail}"),
            };
        }
    };
    match dispatch
        .wait(deadline.remaining(), &format!("{stage:?}"))
        .await
    {
        Ok(result) => result,
        Err(detail) => WaitResult::Failed { detail },
    }
}

async fn bounded_residue(
    dispatcher: &Arc<BlockingDispatchPool>,
    effects: Arc<dyn TeardownEffects>,
    ticket: &TeardownTicket,
    deadline: CleanupDeadline,
    errors: &mut Vec<String>,
    cancellation: Arc<CancellationToken>,
) -> Option<ResidueEvidence> {
    let dispatch = match dispatch_residue(dispatcher, effects, ticket.clone(), cancellation) {
        Ok(dispatch) => dispatch,
        Err(detail) => {
            errors.push(format!("Residue: {detail}"));
            return None;
        }
    };
    match dispatch.wait(deadline.remaining(), "Residue").await {
        Ok(residue) => residue,
        Err(detail) => {
            errors.push(format!("Residue: {detail}"));
            None
        }
    }
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
    dispatcher: &Arc<BlockingDispatchPool>,
    cancellation: Arc<CancellationToken>,
) -> bool {
    match result {
        WaitResult::Zero => {
            attempted_stages.push(TeardownStage::SettleActiveProcessZero);
            match bounded_stage(
                TeardownStage::SettleActiveProcessZero,
                deadline,
                dispatcher,
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
    dispatcher: Arc<BlockingDispatchPool>,
    cancellation: Arc<CancellationToken>,
) -> TeardownReport {
    let mut errors = errors;
    attempted_stages.push(TeardownStage::DetachAfterZero);
    collect_stage_result(
        bounded_stage(
            TeardownStage::DetachAfterZero,
            deadline,
            &dispatcher,
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
            &dispatcher,
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
            &dispatcher,
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
        &dispatcher,
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
            &dispatcher,
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
    use super::{sanitize_text, BlockingDispatchPool, MAX_EVIDENCE_TEXT_BYTES};

    #[test]
    fn blocking_dispatch_pool_normalizes_zero_worker_configuration() {
        let pool = BlockingDispatchPool::new(0, 1);
        assert_eq!(pool.worker_capacity, 1);
        pool.shutdown();
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
