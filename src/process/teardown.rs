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
const MAX_RESIDUE_STAGES: usize = 32;
const UNAVAILABLE_JOB_IDENTITY: &str = "<unavailable: managed Job identity>";
const UNAVAILABLE_ROOT_EXECUTABLE: &str = "<unavailable: root executable>";
const UNAVAILABLE_ROOT_COMMAND: &str = "<unavailable: root command>";
const UNAVAILABLE_LIFECYCLE_EVENT: &str = "<unavailable: lifecycle event>";

fn sanitize_text(value: &str) -> String {
    let normalized: String = value
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
        "token" | "password" | "passwd" | "secret" | "api_key" | "apikey" | "authorization"
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

/// Durable idempotency seam for completed teardown operations.
///
/// The coordinator keeps only a bounded in-memory cache. When this seam is
/// supplied, completed entries may be evicted from that cache because an
/// exact retry can be recovered here without rerunning destructive effects.
pub trait TeardownCompletionStore: Send + Sync + 'static {
    fn lookup(&self, key: &TeardownCompletionKey) -> Result<Option<TeardownReport>, String>;

    fn persist<'a>(
        &'a self,
        key: &'a TeardownCompletionKey,
        report: &'a TeardownReport,
    ) -> BoxFuture<'a, Result<(), String>>;
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
struct ActiveCleanup {
    key: TeardownCompletionKey,
    cell: Arc<CleanupCell>,
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
    effects: Arc<dyn TeardownEffects>,
    clock: Arc<dyn TeardownClock>,
    budgets: TeardownBudgets,
    state: Arc<Mutex<CoordinatorState>>,
    completion_store: Arc<dyn TeardownCompletionStore>,
    completed_operation_capacity: usize,
}

struct ExecutorQueueState {
    queue: VecDeque<CleanupWork>,
    occupied: usize,
    closed: bool,
}

struct TeardownExecutorInner {
    state: Mutex<ExecutorQueueState>,
    changed: Condvar,
    worker_capacity: usize,
    queue_capacity: usize,
}

struct TeardownExecutor {
    inner: Arc<TeardownExecutorInner>,
    workers: Mutex<Vec<JoinHandle<()>>>,
}

struct ExecutorReservation {
    inner: Arc<TeardownExecutorInner>,
    remaining: usize,
}

impl TeardownExecutor {
    fn new(worker_capacity: usize, queue_capacity: usize) -> Self {
        let inner = Arc::new(TeardownExecutorInner {
            state: Mutex::new(ExecutorQueueState {
                queue: VecDeque::with_capacity(queue_capacity),
                occupied: 0,
                closed: false,
            }),
            changed: Condvar::new(),
            worker_capacity,
            queue_capacity,
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
        Self {
            inner,
            workers: Mutex::new(workers),
        }
    }

    fn reserve_many(&self, count: usize) -> Result<ExecutorReservation, TeardownReject> {
        if count == 0 {
            return Ok(ExecutorReservation {
                inner: Arc::clone(&self.inner),
                remaining: 0,
            });
        }
        if count > self.inner.queue_capacity {
            return Err(TeardownReject::CompletionJournalFull);
        }
        let capacity = self
            .inner
            .worker_capacity
            .saturating_add(self.inner.queue_capacity);
        if count > capacity {
            return Err(TeardownReject::CompletionJournalFull);
        }
        let mut state = self
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
                    inner: Arc::clone(&self.inner),
                    remaining: count,
                });
            }
            state = self
                .inner
                .changed
                .wait(state)
                .expect("teardown executor state mutex poisoned");
        }
    }
}

impl Drop for TeardownExecutor {
    fn drop(&mut self) {
        let queued = {
            let mut state = self
                .inner
                .state
                .lock()
                .expect("teardown executor state mutex poisoned");
            state.closed = true;
            let queued: Vec<_> = state.queue.drain(..).collect();
            state.occupied = state.occupied.saturating_sub(queued.len());
            self.inner.changed.notify_all();
            queued
        };
        for work in queued {
            cancel_queued_cleanup(work);
        }
        let mut workers = self
            .workers
            .lock()
            .expect("teardown worker handles mutex poisoned");
        for worker in workers.drain(..) {
            worker.join().expect("bounded teardown worker panicked");
        }
    }
}

fn cancel_queued_cleanup(work: CleanupWork) {
    let CleanupWork {
        ticket,
        key,
        cell,
        state,
        ..
    } = work;
    cell.finish(waiter_failure_report(
        ticket,
        "teardown executor shut down before cleanup started",
    ));
    state
        .lock()
        .expect("teardown coordinator state mutex poisoned")
        .active
        .retain(|active| active.key != key);
}

impl ExecutorReservation {
    fn submit(&mut self, work: CleanupWork) -> Result<(), CleanupWork> {
        let mut state = self
            .inner
            .state
            .lock()
            .expect("teardown executor state mutex poisoned");
        loop {
            if state.closed {
                return Err(work);
            }
            if state.queue.len() < self.inner.queue_capacity {
                state.queue.push_back(work);
                self.remaining = self.remaining.saturating_sub(1);
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
        runtime.block_on(run_cleanup(work));
        let mut state = inner
            .state
            .lock()
            .expect("teardown executor state mutex poisoned");
        state.occupied = state.occupied.saturating_sub(1);
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
    completion_store: Arc<dyn TeardownCompletionStore>,
    state: Arc<Mutex<CoordinatorState>>,
    admission_serial: Mutex<()>,
    executor: TeardownExecutor,
}

impl TeardownCoordinator {
    pub fn new(
        admission: Arc<dyn TeardownAdmission>,
        effects: Arc<dyn TeardownEffects>,
        clock: Arc<dyn TeardownClock>,
        completion_store: Arc<dyn TeardownCompletionStore>,
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
        completion_store: Arc<dyn TeardownCompletionStore>,
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
        completion_store: Arc<dyn TeardownCompletionStore>,
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
        completion_store: Arc<dyn TeardownCompletionStore>,
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

    pub fn request(&self, ticket: TeardownTicket) -> Result<TeardownWaiter, TeardownReject> {
        let _admission_serial = self
            .admission_serial
            .lock()
            .expect("teardown admission serial mutex poisoned");
        let key = completion_key(&ticket);
        if let Some(waiter) = self.find_existing_waiter(&key) {
            return Ok(waiter);
        }
        if let Some(waiter) = self.lookup_completed(&ticket, &key)? {
            return Ok(waiter);
        }

        let mut reservation = self.executor.reserve_many(1)?;
        let receipts = self
            .admission
            .close_admission_batch(std::slice::from_ref(&ticket))
            .map_err(TeardownReject::from)?;
        if receipts.len() != 1 {
            return Err(TeardownReject::Admission(TeardownAdmissionError::Other {
                detail: "admission returned an invalid single-ticket receipt batch".to_string(),
            }));
        }
        let receipt =
            receipts
                .first()
                .ok_or(TeardownReject::Admission(TeardownAdmissionError::Other {
                    detail: "admission returned no receipt".to_string(),
                }))?;
        validate_receipt(&ticket, receipt)?;

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
        if reservation.submit(work).is_err() {
            self.remove_active(&key);
            return Err(TeardownReject::ExecutorClosed);
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
            return Err(TeardownReject::Admission(TeardownAdmissionError::Other {
                detail: "admission returned an incomplete receipt batch".to_string(),
            }));
        }
        for ((_, ticket), receipt) in fresh.iter().zip(receipts.iter()) {
            validate_receipt(ticket, receipt)?;
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
            works.push(self.cleanup_work(ticket, key, cell));
        }
        for key in fresh_duplicates {
            if let Some((_, cell)) = created.iter().find(|(created_key, _)| *created_key == key) {
                waiters.push(TeardownWaiter {
                    cell: Arc::clone(cell),
                });
            }
        }
        for work in works {
            if reservation.submit(work).is_err() {
                return Err(TeardownReject::ExecutorClosed);
            }
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
            completion_store: Arc::clone(&self.completion_store),
            completed_operation_capacity: self.completed_operation_capacity,
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
        match self.completion_store.lookup(key) {
            Ok(Some(report)) => {
                if report.action_epoch() != key.action_epoch || report.fence() != key.fence() {
                    return Err(TeardownReject::CompletionLookupFailed {
                        detail: "completion store returned a mismatched report".to_string(),
                    });
                }
                let cell = Arc::new(CleanupCell::new(&report.ticket));
                cell.finish(report);
                Ok(Some(TeardownWaiter { cell }))
            }
            Ok(None) => Ok(None),
            Err(detail) => Err(TeardownReject::CompletionLookupFailed { detail }),
        }
    }

    fn lookup_completed_by_key_with_ticket(
        &self,
        key: &TeardownCompletionKey,
        ticket: &TeardownTicket,
    ) -> Result<Option<TeardownWaiter>, TeardownReject> {
        match self.completion_store.lookup(key) {
            Ok(Some(report)) => {
                if report.action_epoch() != key.action_epoch || report.fence() != key.fence() {
                    return Err(TeardownReject::CompletionLookupFailed {
                        detail: "completion store returned a mismatched report".to_string(),
                    });
                }
                let cell = Arc::new(CleanupCell::new(ticket));
                cell.finish(report);
                Ok(Some(TeardownWaiter { cell }))
            }
            Ok(None) => Ok(None),
            Err(detail) => Err(TeardownReject::CompletionLookupFailed { detail }),
        }
    }

    fn remove_active(&self, key: &TeardownCompletionKey) {
        self.state
            .lock()
            .expect("teardown coordinator state mutex poisoned")
            .active
            .retain(|active| active.key != *key);
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
        effects,
        clock,
        budgets,
        state,
        completion_store,
        completed_operation_capacity,
    } = work;
    let fallback_ticket = ticket.clone();
    let report = match AssertUnwindSafe(execute_cleanup(ticket, effects, clock, budgets))
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
        completed_operation_capacity,
        key.clone(),
        Arc::clone(&cell),
        report,
        budgets.termination,
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
    completion_store: &Arc<dyn TeardownCompletionStore>,
    key: &TeardownCompletionKey,
    report: &TeardownReport,
    budget: Duration,
) -> Result<(), String> {
    let future =
        std::panic::catch_unwind(AssertUnwindSafe(|| completion_store.persist(key, report)))
            .map_err(|payload| {
                format!("completion persistence panicked: {}", panic_detail(payload))
            })?;
    match tokio::time::timeout(budget, AssertUnwindSafe(future).catch_unwind()).await {
        Ok(Ok(result)) => result,
        Ok(Err(payload)) => Err(format!(
            "completion persistence panicked: {}",
            panic_detail(payload)
        )),
        Err(_) => Err(format!(
            "completion persistence timed out after {:?}",
            budget
        )),
    }
}

async fn handoff_completed_cleanup(
    state: &Arc<Mutex<CoordinatorState>>,
    completion_store: Arc<dyn TeardownCompletionStore>,
    completed_operation_capacity: usize,
    key: TeardownCompletionKey,
    cell: Arc<CleanupCell>,
    report: TeardownReport,
    persistence_budget: Duration,
) -> TeardownReport {
    if let Err(detail) =
        persist_completion(&completion_store, &key, &report, persistence_budget).await
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

async fn execute_cleanup(
    ticket: TeardownTicket,
    effects: Arc<dyn TeardownEffects>,
    clock: Arc<dyn TeardownClock>,
    budgets: TeardownBudgets,
) -> TeardownReport {
    let mut attempted_stages = Vec::new();
    let mut errors = Vec::new();

    attempted_stages.push(TeardownStage::Drain);
    collect_stage_result(
        bounded_stage(
            TeardownStage::Drain,
            budgets.cooperative_grace,
            effects.drain(&ticket),
        )
        .await,
        &mut errors,
        TeardownStage::Drain,
    );

    attempted_stages.push(TeardownStage::CooperativeClose);
    collect_stage_result(
        bounded_stage(
            TeardownStage::CooperativeClose,
            budgets.cooperative_grace,
            effects.cooperative_close(&ticket),
        )
        .await,
        &mut errors,
        TeardownStage::CooperativeClose,
    );

    attempted_stages.push(TeardownStage::CooperativeWait);
    let cooperative = bounded_wait(
        TeardownStage::CooperativeWait,
        budgets.cooperative_grace,
        effects.wait_for_zero(
            &ticket,
            WaitStage::CooperativeGrace,
            clock.deadline(budgets.cooperative_grace),
        ),
    )
    .await;
    if try_settle_after_wait(
        &ticket,
        cooperative,
        &effects,
        budgets.termination,
        &mut attempted_stages,
        &mut errors,
    )
    .await
    {
        return settle_after_zero(ticket, effects, budgets, attempted_stages, errors).await;
    }

    attempted_stages.push(TeardownStage::InterruptOrSafeClose);
    collect_stage_result(
        bounded_stage(
            TeardownStage::InterruptOrSafeClose,
            budgets.interrupt_grace,
            effects.interrupt_or_safe_close(&ticket),
        )
        .await,
        &mut errors,
        TeardownStage::InterruptOrSafeClose,
    );

    attempted_stages.push(TeardownStage::InterruptWait);
    let interrupted = bounded_wait(
        TeardownStage::InterruptWait,
        budgets.interrupt_grace,
        effects.wait_for_zero(
            &ticket,
            WaitStage::InterruptGrace,
            clock.deadline(budgets.interrupt_grace),
        ),
    )
    .await;
    if try_settle_after_wait(
        &ticket,
        interrupted,
        &effects,
        budgets.termination,
        &mut attempted_stages,
        &mut errors,
    )
    .await
    {
        return settle_after_zero(ticket, effects, budgets, attempted_stages, errors).await;
    }

    attempted_stages.push(TeardownStage::TerminateTree);
    collect_stage_result(
        bounded_stage(
            TeardownStage::TerminateTree,
            budgets.termination,
            effects.terminate_tree(&ticket),
        )
        .await,
        &mut errors,
        TeardownStage::TerminateTree,
    );

    attempted_stages.push(TeardownStage::TerminationWait);
    let terminated = bounded_wait(
        TeardownStage::TerminationWait,
        budgets.termination,
        effects.wait_for_zero(
            &ticket,
            WaitStage::Termination,
            clock.deadline(budgets.termination),
        ),
    )
    .await;
    if try_settle_after_wait(
        &ticket,
        terminated,
        &effects,
        budgets.termination,
        &mut attempted_stages,
        &mut errors,
    )
    .await
    {
        return settle_after_zero(ticket, effects, budgets, attempted_stages, errors).await;
    }

    let adapter_residue =
        bounded_residue(&effects, &ticket, budgets.termination, &mut errors).await;
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
    budget: Duration,
    future: BoxFuture<'_, StageResult>,
) -> StageResult {
    match tokio::time::timeout(budget, AssertUnwindSafe(future).catch_unwind()).await {
        Ok(Ok(result)) => result,
        Ok(Err(payload)) => StageResult::Failed {
            detail: format!("{stage:?} panic: {}", panic_detail(payload)),
        },
        Err(_) => StageResult::Failed {
            detail: format!("{stage:?} timeout after {budget:?}"),
        },
    }
}

async fn bounded_wait(
    stage: TeardownStage,
    budget: Duration,
    future: BoxFuture<'_, WaitResult>,
) -> WaitResult {
    match tokio::time::timeout(budget, AssertUnwindSafe(future).catch_unwind()).await {
        Ok(Ok(result)) => result,
        Ok(Err(payload)) => WaitResult::Failed {
            detail: format!("{stage:?} panic: {}", panic_detail(payload)),
        },
        Err(_) => WaitResult::Failed {
            detail: format!("{stage:?} timeout after {budget:?}"),
        },
    }
}

async fn bounded_residue(
    effects: &Arc<dyn TeardownEffects>,
    ticket: &TeardownTicket,
    budget: Duration,
    errors: &mut Vec<String>,
) -> Option<ResidueEvidence> {
    match tokio::time::timeout(
        budget,
        AssertUnwindSafe(effects.residue(ticket)).catch_unwind(),
    )
    .await
    {
        Ok(Ok(residue)) => residue,
        Ok(Err(payload)) => {
            errors.push(format!("Residue: panic: {}", panic_detail(payload)));
            None
        }
        Err(_) => {
            errors.push(format!("Residue: timeout after {budget:?}"));
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
    budget: Duration,
    attempted_stages: &mut Vec<TeardownStage>,
    errors: &mut Vec<String>,
) -> bool {
    match result {
        WaitResult::Zero => {
            attempted_stages.push(TeardownStage::SettleActiveProcessZero);
            match bounded_stage(
                TeardownStage::SettleActiveProcessZero,
                budget,
                effects.settle_active_process_zero(ticket),
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
    budgets: TeardownBudgets,
    mut attempted_stages: Vec<TeardownStage>,
    errors: Vec<String>,
) -> TeardownReport {
    let mut errors = errors;
    attempted_stages.push(TeardownStage::DetachAfterZero);
    collect_stage_result(
        bounded_stage(
            TeardownStage::DetachAfterZero,
            budgets.termination,
            effects.detach_after_zero(&ticket),
        )
        .await,
        &mut errors,
        TeardownStage::DetachAfterZero,
    );
    attempted_stages.push(TeardownStage::ReconcilePorts);
    collect_stage_result(
        bounded_stage(
            TeardownStage::ReconcilePorts,
            budgets.termination,
            effects.reconcile_ports(&ticket),
        )
        .await,
        &mut errors,
        TeardownStage::ReconcilePorts,
    );
    attempted_stages.push(TeardownStage::PersistSettlement);
    collect_stage_result(
        bounded_stage(
            TeardownStage::PersistSettlement,
            budgets.termination,
            effects.persist_settlement(&ticket),
        )
        .await,
        &mut errors,
        TeardownStage::PersistSettlement,
    );
    attempted_stages.push(TeardownStage::ReleaseStoppedExact);
    let release = bounded_stage(
        TeardownStage::ReleaseStoppedExact,
        budgets.termination,
        effects.release_stopped_exact(&ticket),
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
        bounded_residue(&effects, &ticket, budgets.termination, &mut errors).await
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
