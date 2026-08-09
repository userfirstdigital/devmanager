//! Pure, generation-fenced process-tree teardown orchestration.
//!
//! This module deliberately stops at the TeardownEffects seam. The host and
//! the future terminal service provide that small runtime adapter; this core
//! owns admission ordering, exact-fence validation, escalation, bounded
//! concurrency, and waiter lifetime.

use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::runtime::Handle;
use tokio::sync::{watch, Semaphore};

use crate::domain::id::{OperationId, ResourceId, TaskId};
use crate::process::identity::ProcessOwner;
use crate::process::registry::ManagedProcessFence;

pub const DEFAULT_CONFIGURED_CAPACITY: usize = 4;
pub const DEFAULT_COMPLETED_OPERATION_CAPACITY: usize = 256;

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
    let redacted = normalized
        .split_whitespace()
        .map(|part| {
            let Some((key, _)) = part.split_once('=') else {
                return part.to_string();
            };
            let lower = key.trim_start_matches('-').to_ascii_lowercase();
            if matches!(
                lower.as_str(),
                "token" | "password" | "passwd" | "secret" | "api_key" | "apikey" | "authorization"
            ) || lower.ends_with("_token")
                || lower.ends_with("_password")
                || lower.ends_with("_secret")
            {
                format!("{key}=<redacted>")
            } else {
                part.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join(" ");
    truncate_utf8(redacted, MAX_EVIDENCE_TEXT_BYTES)
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
            job_name: sanitize_text(job_name.as_ref()),
            pid,
            creation_time_100ns,
            executable: sanitize_text(executable.as_ref()),
            command_label: sanitize_text(command_label.as_ref()),
            last_lifecycle_event: sanitize_text(last_lifecycle_event.as_ref()),
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

    fn persist(&self, key: &TeardownCompletionKey, report: &TeardownReport) -> Result<(), String>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TeardownReject {
    StaleEpoch { expected: u64, actual: u64 },
    NonClosingScope,
    FenceMismatch,
    CompletionJournalFull,
    CompletionLookupFailed { detail: String },
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
}

impl CleanupCell {
    fn new() -> Self {
        let (done, _receiver) = watch::channel(false);
        Self {
            result: Mutex::new(None),
            done,
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
            let _ = done.changed().await;
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
    handoff_failed: bool,
}

pub struct TeardownCoordinator {
    admission: Arc<dyn TeardownAdmission>,
    effects: Arc<dyn TeardownEffects>,
    clock: Arc<dyn TeardownClock>,
    budgets: TeardownBudgets,
    semaphore: Arc<Semaphore>,
    configured_capacity: usize,
    completed_operation_capacity: usize,
    completion_store: Arc<dyn TeardownCompletionStore>,
    state: Arc<Mutex<CoordinatorState>>,
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
            semaphore: Arc::new(Semaphore::new(configured_capacity)),
            configured_capacity,
            completed_operation_capacity,
            completion_store,
            state: Arc::new(Mutex::new(CoordinatorState::default())),
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
        let key = completion_key(&ticket);
        let cell = {
            let mut state = self
                .state
                .lock()
                .expect("teardown coordinator state mutex poisoned");
            if let Some(existing) = state.active.iter().find(|existing| existing.key == key) {
                return Ok(TeardownWaiter {
                    cell: Arc::clone(&existing.cell),
                });
            }
            if let Some(existing) = state.completed.iter().find(|existing| existing.key == key) {
                return Ok(TeardownWaiter {
                    cell: Arc::clone(&existing.cell),
                });
            }
            if state.handoff_failed {
                return Err(TeardownReject::CompletionLookupFailed {
                    detail: "completed teardown handoff is unavailable".to_string(),
                });
            }
            match self.completion_store.lookup(&key) {
                Ok(Some(report)) => {
                    if report.action_epoch() != key.action_epoch || report.fence() != key.fence() {
                        return Err(TeardownReject::CompletionLookupFailed {
                            detail: "completion store returned a mismatched report".to_string(),
                        });
                    }
                    let cell = Arc::new(CleanupCell::new());
                    cell.finish(report);
                    return Ok(TeardownWaiter { cell });
                }
                Ok(None) => {}
                Err(detail) => {
                    return Err(TeardownReject::CompletionLookupFailed { detail });
                }
            }

            let receipt = self
                .admission
                .close_admission(&ticket)
                .map_err(TeardownReject::from)?;
            validate_receipt(&ticket, &receipt)?;
            let cell = Arc::new(CleanupCell::new());
            state.active.push(ActiveCleanup {
                key: key.clone(),
                cell: Arc::clone(&cell),
            });
            cell
        };

        self.spawn_owned_cleanup(ticket, key, Arc::clone(&cell));
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
        let key = TeardownCompletionKey::new(action_epoch, fence.clone());
        let state = self
            .state
            .lock()
            .expect("teardown coordinator state mutex poisoned");
        if let Some(existing) = state.active.iter().find(|existing| existing.key == key) {
            return Ok(TeardownWaiter {
                cell: Arc::clone(&existing.cell),
            });
        }
        if let Some(existing) = state.completed.iter().find(|existing| existing.key == key) {
            return Ok(TeardownWaiter {
                cell: Arc::clone(&existing.cell),
            });
        }
        match self.completion_store.lookup(&key) {
            Ok(Some(report)) => {
                if report.action_epoch() != key.action_epoch || report.fence() != key.fence() {
                    return Err(TeardownReject::CompletionLookupFailed {
                        detail: "completion store returned a mismatched report".to_string(),
                    });
                }
                let cell = Arc::new(CleanupCell::new());
                cell.finish(report);
                return Ok(TeardownWaiter { cell });
            }
            Ok(None) => {}
            Err(detail) => {
                return Err(TeardownReject::CompletionLookupFailed { detail });
            }
        }
        Err(TeardownReject::NoMatchingCleanup)
    }

    pub fn request_batch(
        &self,
        tickets: Vec<TeardownTicket>,
    ) -> Result<TeardownBatchWaiter, TeardownReject> {
        let mut waiters = Vec::with_capacity(tickets.len());
        for ticket in tickets {
            waiters.push(self.request(ticket)?);
        }
        Ok(TeardownBatchWaiter { waiters })
    }

    fn spawn_owned_cleanup(
        &self,
        ticket: TeardownTicket,
        key: TeardownCompletionKey,
        cell: Arc<CleanupCell>,
    ) {
        let effects = Arc::clone(&self.effects);
        let clock = Arc::clone(&self.clock);
        let semaphore = Arc::clone(&self.semaphore);
        let state = Arc::clone(&self.state);
        let completion_store = self.completion_store.clone();
        let completed_operation_capacity = self.completed_operation_capacity;
        let budgets = self.budgets;
        let task = async move {
            let permit = semaphore
                .acquire_owned()
                .await
                .expect("teardown semaphore remains open for coordinator lifetime");
            let report = execute_cleanup(ticket, effects, clock, budgets).await;
            drop(permit);
            let report = handoff_completed_cleanup(
                &state,
                completion_store,
                completed_operation_capacity,
                key,
                Arc::clone(&cell),
                report,
            );
            cell.finish(report);
        };

        if let Ok(handle) = Handle::try_current() {
            std::mem::drop(handle.spawn(task));
        } else {
            std::thread::Builder::new()
                .name("devmanager-teardown".to_string())
                .spawn(move || {
                    let runtime = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .expect("teardown worker runtime");
                    runtime.block_on(task);
                })
                .expect("spawn teardown worker");
        }
    }
}

fn completion_key(ticket: &TeardownTicket) -> TeardownCompletionKey {
    TeardownCompletionKey::new(ticket.action_epoch(), ticket.fence().clone())
}

fn handoff_completed_cleanup(
    state: &Arc<Mutex<CoordinatorState>>,
    completion_store: Arc<dyn TeardownCompletionStore>,
    completed_operation_capacity: usize,
    key: TeardownCompletionKey,
    cell: Arc<CleanupCell>,
    report: TeardownReport,
) -> TeardownReport {
    if let Err(detail) = completion_store.persist(&key, &report) {
        state
            .lock()
            .expect("teardown coordinator state mutex poisoned")
            .handoff_failed = true;
        return report.with_handoff_error(format!(
            "completed teardown handoff failed: {}",
            sanitize_text(&detail)
        ));
    }

    let mut state = state
        .lock()
        .expect("teardown coordinator state mutex poisoned");
    if state.completed.len() >= completed_operation_capacity {
        state.completed.pop_front();
    }
    state.active.retain(|active| active.key != key);
    state.completed.push_back(CompletedCleanup { key, cell });
    report
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
        effects.drain(&ticket).await,
        &mut errors,
        TeardownStage::Drain,
    );

    attempted_stages.push(TeardownStage::CooperativeClose);
    collect_stage_result(
        effects.cooperative_close(&ticket).await,
        &mut errors,
        TeardownStage::CooperativeClose,
    );

    attempted_stages.push(TeardownStage::CooperativeWait);
    let cooperative = effects
        .wait_for_zero(
            &ticket,
            WaitStage::CooperativeGrace,
            clock.deadline(budgets.cooperative_grace),
        )
        .await;
    if try_settle_after_wait(
        &ticket,
        cooperative,
        &effects,
        &mut attempted_stages,
        &mut errors,
    )
    .await
    {
        return settle_after_zero(ticket, effects, attempted_stages, errors).await;
    }

    attempted_stages.push(TeardownStage::InterruptOrSafeClose);
    collect_stage_result(
        effects.interrupt_or_safe_close(&ticket).await,
        &mut errors,
        TeardownStage::InterruptOrSafeClose,
    );

    attempted_stages.push(TeardownStage::InterruptWait);
    let interrupted = effects
        .wait_for_zero(
            &ticket,
            WaitStage::InterruptGrace,
            clock.deadline(budgets.interrupt_grace),
        )
        .await;
    if try_settle_after_wait(
        &ticket,
        interrupted,
        &effects,
        &mut attempted_stages,
        &mut errors,
    )
    .await
    {
        return settle_after_zero(ticket, effects, attempted_stages, errors).await;
    }

    attempted_stages.push(TeardownStage::TerminateTree);
    collect_stage_result(
        effects.terminate_tree(&ticket).await,
        &mut errors,
        TeardownStage::TerminateTree,
    );

    attempted_stages.push(TeardownStage::TerminationWait);
    let terminated = effects
        .wait_for_zero(
            &ticket,
            WaitStage::Termination,
            clock.deadline(budgets.termination),
        )
        .await;
    if try_settle_after_wait(
        &ticket,
        terminated,
        &effects,
        &mut attempted_stages,
        &mut errors,
    )
    .await
    {
        return settle_after_zero(ticket, effects, attempted_stages, errors).await;
    }

    let residue = required_residue(
        &ticket,
        &attempted_stages,
        &errors,
        TeardownOutcome::Leaked,
        effects.residue(&ticket).await,
    );
    TeardownReport {
        ticket,
        outcome: TeardownOutcome::Leaked,
        attempted_stages,
        errors,
        residue,
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
    attempted_stages: &mut Vec<TeardownStage>,
    errors: &mut Vec<String>,
) -> bool {
    match result {
        WaitResult::Zero => {
            attempted_stages.push(TeardownStage::SettleActiveProcessZero);
            match effects.settle_active_process_zero(ticket).await {
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
    mut attempted_stages: Vec<TeardownStage>,
    errors: Vec<String>,
) -> TeardownReport {
    let mut errors = errors;
    attempted_stages.push(TeardownStage::DetachAfterZero);
    collect_stage_result(
        effects.detach_after_zero(&ticket).await,
        &mut errors,
        TeardownStage::DetachAfterZero,
    );
    attempted_stages.push(TeardownStage::ReconcilePorts);
    collect_stage_result(
        effects.reconcile_ports(&ticket).await,
        &mut errors,
        TeardownStage::ReconcilePorts,
    );
    attempted_stages.push(TeardownStage::PersistSettlement);
    collect_stage_result(
        effects.persist_settlement(&ticket).await,
        &mut errors,
        TeardownStage::PersistSettlement,
    );
    attempted_stages.push(TeardownStage::ReleaseStoppedExact);
    let release = effects.release_stopped_exact(&ticket).await;
    let outcome = match release {
        StageResult::Completed if errors.is_empty() => TeardownOutcome::Closed,
        StageResult::Completed => TeardownOutcome::CleanupFailed,
        StageResult::Failed { detail } => {
            errors.push(format!("ReleaseStoppedExact: {}", sanitize_text(&detail)));
            TeardownOutcome::CleanupFailed
        }
    };
    let residue = if outcome != TeardownOutcome::Closed {
        required_residue(
            &ticket,
            &attempted_stages,
            &errors,
            outcome,
            effects.residue(&ticket).await,
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
            UNAVAILABLE_ROOT_EXECUTABLE,
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
        if residue.job_name.is_empty() {
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
        if residue.executable.is_empty() {
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
