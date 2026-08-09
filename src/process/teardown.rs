//! Pure, generation-fenced process-tree teardown orchestration.
//!
//! This module deliberately stops at the TeardownEffects seam. The host and
//! the future terminal service provide that small runtime adapter; this core
//! owns admission ordering, exact-fence validation, escalation, bounded
//! concurrency, and waiter lifetime.

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::runtime::Handle;
use tokio::sync::{watch, Semaphore};

use crate::domain::id::{OperationId, ResourceId, TaskId};
use crate::process::identity::ProcessOwner;
use crate::process::registry::ManagedProcessFence;

pub const DEFAULT_MAX_CONCURRENT_BRANCHES: usize = 4;

/// A boxed asynchronous operation used by the pure runtime seams.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TeardownScope {
    Task(TaskId),
    Host,
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
    ) -> Self {
        Self {
            operation_id,
            scope,
            action_epoch,
            fence,
        }
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
    ReleaseStoppedExact,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StageResult {
    Completed,
    Failed { detail: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WaitResult {
    Zero {
        fence: ManagedProcessFence,
        active_process_zero: bool,
        active_process_ids: Vec<u32>,
    },
    TimedOut,
    Failed {
        detail: String,
    },
}

/// Small adapter surface for terminal/provider close and exact Job operations.
///
/// terminate_tree receives no PID. A production adapter must retain its
/// owned Job/completion/PTY handles until a matching zero observation has been
/// returned and release_stopped_exact is called.
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

    fn residue<'a>(&'a self, ticket: &'a TeardownTicket) -> BoxFuture<'a, Option<ResidueEvidence>>;

    fn release_stopped_exact<'a>(
        &'a self,
        ticket: &'a TeardownTicket,
    ) -> BoxFuture<'a, StageResult>;
}

const MAX_EVIDENCE_TEXT_BYTES: usize = 256;

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
            attempted_stages,
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
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TeardownReject {
    StaleEpoch { expected: u64, actual: u64 },
    NonClosingScope,
    FenceMismatch,
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
    ticket: TeardownTicket,
    cell: Arc<CleanupCell>,
}

pub struct TeardownCoordinator {
    admission: Arc<dyn TeardownAdmission>,
    effects: Arc<dyn TeardownEffects>,
    clock: Arc<dyn TeardownClock>,
    budgets: TeardownBudgets,
    semaphore: Arc<Semaphore>,
    active: Arc<Mutex<Vec<ActiveCleanup>>>,
}

impl TeardownCoordinator {
    pub fn new(
        admission: Arc<dyn TeardownAdmission>,
        effects: Arc<dyn TeardownEffects>,
        clock: Arc<dyn TeardownClock>,
    ) -> Self {
        Self::with_configuration(
            admission,
            effects,
            clock,
            DEFAULT_MAX_CONCURRENT_BRANCHES,
            TeardownBudgets::default(),
        )
    }

    pub fn with_max_concurrency(
        admission: Arc<dyn TeardownAdmission>,
        effects: Arc<dyn TeardownEffects>,
        clock: Arc<dyn TeardownClock>,
        max_concurrent_branches: usize,
    ) -> Self {
        Self::with_configuration(
            admission,
            effects,
            clock,
            max_concurrent_branches,
            TeardownBudgets::default(),
        )
    }

    pub fn with_configuration(
        admission: Arc<dyn TeardownAdmission>,
        effects: Arc<dyn TeardownEffects>,
        clock: Arc<dyn TeardownClock>,
        max_concurrent_branches: usize,
        budgets: TeardownBudgets,
    ) -> Self {
        Self {
            admission,
            effects,
            clock,
            budgets,
            semaphore: Arc::new(Semaphore::new(max_concurrent_branches.max(1))),
            active: Arc::new(Mutex::new(Vec::new())),
        }
    }

    pub fn max_concurrent_branches(&self) -> usize {
        self.semaphore.available_permits()
    }

    pub fn request(&self, ticket: TeardownTicket) -> Result<TeardownWaiter, TeardownReject> {
        let cell = {
            let mut active = self.active.lock().expect("teardown active mutex poisoned");
            if let Some(existing) = active.iter().find(|existing| {
                existing.ticket.action_epoch() == ticket.action_epoch()
                    && existing.ticket.fence() == ticket.fence()
            }) {
                return Ok(TeardownWaiter {
                    cell: Arc::clone(&existing.cell),
                });
            }

            let receipt = self
                .admission
                .close_admission(&ticket)
                .map_err(TeardownReject::from)?;
            validate_receipt(&ticket, &receipt)?;
            let cell = Arc::new(CleanupCell::new());
            active.push(ActiveCleanup {
                ticket: ticket.clone(),
                cell: Arc::clone(&cell),
            });
            cell
        };

        self.spawn_owned_cleanup(ticket, Arc::clone(&cell));
        Ok(TeardownWaiter { cell })
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

    fn spawn_owned_cleanup(&self, ticket: TeardownTicket, cell: Arc<CleanupCell>) {
        let effects = Arc::clone(&self.effects);
        let clock = Arc::clone(&self.clock);
        let semaphore = Arc::clone(&self.semaphore);
        let budgets = self.budgets;
        let task = async move {
            let permit = semaphore
                .acquire_owned()
                .await
                .expect("teardown semaphore remains open for coordinator lifetime");
            let report = execute_cleanup(ticket, effects, clock, budgets).await;
            drop(permit);
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
    if zero_proof_is_valid(&ticket, &cooperative, &mut errors) {
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
    if zero_proof_is_valid(&ticket, &interrupted, &mut errors) {
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
    if zero_proof_is_valid(&ticket, &terminated, &mut errors) {
        return settle_after_zero(ticket, effects, attempted_stages, errors).await;
    }

    let residue = effects.residue(&ticket).await;
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

fn zero_proof_is_valid(
    ticket: &TeardownTicket,
    result: &WaitResult,
    errors: &mut Vec<String>,
) -> bool {
    match result {
        WaitResult::Zero {
            fence,
            active_process_zero,
            active_process_ids,
        } if fence == ticket.fence() && *active_process_zero && active_process_ids.is_empty() => {
            true
        }
        WaitResult::Zero { .. } => false,
        WaitResult::TimedOut => false,
        WaitResult::Failed { detail } => {
            errors.push(format!("zero wait failed: {}", sanitize_text(detail)));
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
    attempted_stages.push(TeardownStage::ReleaseStoppedExact);
    let release = effects.release_stopped_exact(&ticket).await;
    let mut errors = errors;
    let outcome = match release {
        StageResult::Completed if errors.is_empty() => TeardownOutcome::Closed,
        StageResult::Completed => TeardownOutcome::CleanupFailed,
        StageResult::Failed { detail } => {
            errors.push(format!("ReleaseStoppedExact: {}", sanitize_text(&detail)));
            TeardownOutcome::CleanupFailed
        }
    };
    let residue = if outcome != TeardownOutcome::Closed {
        effects.residue(&ticket).await
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
