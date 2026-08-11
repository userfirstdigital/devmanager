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
use std::sync::{Arc, Condvar, Mutex, MutexGuard, TryLockError};
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
use crate::process::job::{JobMemberObservation, ManagedProcessJob};
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

mod sealed {
    pub(crate) trait Admission {}
    pub(crate) trait Clock {}
    pub(crate) trait Effects {}

    #[cfg(test)]
    impl<T: Send + Sync + 'static> Admission for T {}
    #[cfg(test)]
    impl<T: Send + Sync + 'static> Clock for T {}
    #[cfg(test)]
    impl<T: Send + Sync + 'static> Effects for T {}
}

pub const DEFAULT_CONFIGURED_CAPACITY: usize = 4;
pub const DEFAULT_COMPLETED_OPERATION_CAPACITY: usize = 256;
pub const DEFAULT_EXECUTOR_QUEUE_CAPACITY: usize = 256;
const MAX_EXECUTOR_WORKER_CAPACITY: usize = 32;
pub(crate) const MAX_MANAGED_TERMINAL_PORTS: usize = 64;
/// The largest request accepted by the atomic batch API.  This is deliberately
/// the same size as the fixed executor mailbox: a caller must never be able to
/// turn a rejected request into an unbounded ticket/receipt allocation.
pub const MAX_TEARDOWN_BATCH_ITEMS: usize = DEFAULT_EXECUTOR_QUEUE_CAPACITY;
const MAX_TEARDOWN_RETENTION_ITEMS: usize = DEFAULT_EXECUTOR_QUEUE_CAPACITY;
const MAX_TEARDOWN_ERRORS: usize = 32;
const MAX_TEARDOWN_STAGE_NOTES: usize = 32;
pub(crate) const MAX_TEARDOWN_HOST_STRING_BYTES: usize = 32 * 1024;
const MAX_DURABLE_REPORT_BYTES: usize = 64 * 1024;
const MAX_COORDINATOR_SHUTDOWN_JOIN: Duration = Duration::from_secs(5);

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
    HostIdentityTooLarge,
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
        if !teardown_host_path_within_bound(fence.root().canonical_executable()) {
            return Err(TeardownTicketError::HostIdentityTooLarge);
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

fn teardown_host_path_within_bound(path: &Path) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        return path
            .as_os_str()
            .encode_wide()
            .count()
            .checked_mul(std::mem::size_of::<u16>())
            .is_some_and(|bytes| bytes <= MAX_TEARDOWN_HOST_STRING_BYTES);
    }
    #[cfg(not(windows))]
    {
        path.as_os_str().as_encoded_bytes().len() <= MAX_TEARDOWN_HOST_STRING_BYTES
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
pub(crate) trait TeardownAdmission: sealed::Admission + Send + Sync + 'static {
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
pub(crate) struct TeardownDeadline(Instant);

impl TeardownDeadline {
    #[cfg(test)]
    pub fn new(value: u64) -> Self {
        Self(
            Instant::now()
                .checked_add(Duration::from_nanos(value))
                .expect("test teardown deadline overflow"),
        )
    }

    fn absolute(self) -> Instant {
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
pub(crate) trait TeardownClock: sealed::Clock + Send + Sync + 'static {
    fn deadline(&self, timeout: Duration) -> TeardownDeadline;
}

#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct MonotonicTeardownClock;

#[cfg(not(test))]
impl sealed::Clock for MonotonicTeardownClock {}

impl TeardownClock for MonotonicTeardownClock {
    fn deadline(&self, timeout: Duration) -> TeardownDeadline {
        TeardownDeadline(
            Instant::now()
                .checked_add(timeout)
                .expect("monotonic teardown deadline overflow"),
        )
    }
}

/// Converts an exact nanosecond count without truncating sub-millisecond
/// precision. `Duration` itself has a `u64` seconds field, so values beyond
/// its representable range are rejected instead of silently saturated.
#[cfg(test)]
fn duration_from_nanos(nanos: u128) -> Result<Duration, String> {
    let seconds = nanos / 1_000_000_000;
    let nanoseconds = nanos % 1_000_000_000;
    if seconds > u64::MAX as u128 {
        Err("monotonic teardown nanoseconds exceed Duration".to_string())
    } else {
        Ok(Duration::new(seconds as u64, nanoseconds as u32))
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

    fn checked_wait_total(self) -> Result<Duration, String> {
        self.cooperative_grace
            .checked_add(self.interrupt_grace)
            .and_then(|total| total.checked_add(self.termination))
            .ok_or_else(|| "teardown duration budget overflow".to_string())
    }

    fn checked_total(self) -> Result<Duration, String> {
        let wait_total = self.checked_wait_total()?;
        // Grace periods are process-side waits. Reserve a fixed multiple for
        // admission, bounded effect construction, Job-zero settlement, actor
        // joins, exact release, and durable writes. These stages all remain
        // under this one absolute deadline; the larger control-plane share
        // prevents scheduler pressure from turning a proven-zero Job into a
        // detached or falsely-stopped session.
        wait_total
            .checked_add(
                wait_total
                    .checked_mul(4)
                    .ok_or_else(|| "teardown control-plane budget overflow".to_string())?,
            )
            .ok_or_else(|| "teardown control-plane budget overflow".to_string())
    }

    fn wait_budget(self, stage: WaitStage) -> Duration {
        match stage {
            WaitStage::CooperativeGrace => self.cooperative_grace,
            WaitStage::InterruptGrace => self.interrupt_grace,
            WaitStage::Termination => self.termination,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct CleanupDeadline {
    absolute: Instant,
    effect_budget: Duration,
}

impl CleanupDeadline {
    fn new(clock: &dyn TeardownClock, budgets: TeardownBudgets) -> Result<Self, String> {
        let total = budgets.checked_total()?;
        let effect_budget = budgets.checked_wait_total()?;
        let clock_deadline =
            std::panic::catch_unwind(AssertUnwindSafe(|| clock.deadline(total)))
                .map_err(|payload| format!("teardown clock panicked: {}", panic_detail(payload)))?;
        Ok(Self {
            absolute: clock_deadline.absolute(),
            effect_budget,
        })
    }

    /// Checks the one absolute monotonic deadline shared by every admission,
    /// executor, effect, and settlement operation.  Callers use this both
    /// immediately before and immediately after crossing an adapter boundary.
    fn check(self, operation: &str) -> Result<Duration, String> {
        checked_remaining_until(self.absolute, operation)
    }

    /// Derives a strictly earlier host-boundary deadline without ever
    /// extending the operation's one authoritative absolute deadline. Store
    /// and synchronous adapter calls use this so one blocked boundary cannot
    /// consume all control-plane reserve needed for rollback and settlement.
    fn boundary_deadline(self, operation: &str) -> Result<Instant, String> {
        let remaining = self.check(operation)?;
        let boundary_budget = remaining.min(self.effect_budget);
        let now = std::panic::catch_unwind(AssertUnwindSafe(Instant::now)).map_err(|payload| {
            format!(
                "teardown monotonic clock panicked: {}",
                panic_detail(payload)
            )
        })?;
        let boundary = now
            .checked_add(boundary_budget)
            .ok_or_else(|| format!("{operation} boundary deadline overflow"))?
            .min(self.absolute);
        checked_remaining_until(boundary, operation)?;
        Ok(boundary)
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

/// Acquires an internal authority lock without allowing mutex contention to
/// escape the one absolute teardown deadline. The check on both sides of the
/// successful acquisition is deliberate: a waiter that acquired the mutex
/// only after its authority expired must drop it without crossing the guarded
/// operation boundary.
fn lock_mutex_until<'a, T>(
    mutex: &'a Mutex<T>,
    absolute_deadline: Instant,
    operation: &str,
) -> Result<MutexGuard<'a, T>, String> {
    loop {
        let remaining = checked_remaining_until(absolute_deadline, operation)?;
        match mutex.try_lock() {
            Ok(guard) => {
                checked_remaining_until(absolute_deadline, operation)?;
                return Ok(guard);
            }
            Err(TryLockError::WouldBlock) => {
                thread::sleep(remaining.min(Duration::from_millis(1)));
            }
            Err(TryLockError::Poisoned(_)) => {
                return Err(format!("{operation} mutex poisoned"));
            }
        }
    }
}

fn fail_closed_shutdown_deadline() -> Instant {
    let Some(deadline) = Instant::now().checked_add(MAX_COORDINATOR_SHUTDOWN_JOIN) else {
        std::process::abort();
    };
    deadline
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
    Unsupported { detail: String },
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
pub(crate) trait TeardownEffects: sealed::Effects + Send + Sync + 'static {
    fn drain<'a>(
        &'a self,
        ticket: &'a TeardownTicket,
        deadline: CleanupDeadline,
    ) -> BoxFuture<'a, StageResult>;

    fn cooperative_close<'a>(
        &'a self,
        ticket: &'a TeardownTicket,
        deadline: CleanupDeadline,
    ) -> BoxFuture<'a, StageResult>;

    fn interrupt_or_safe_close<'a>(
        &'a self,
        ticket: &'a TeardownTicket,
        deadline: CleanupDeadline,
    ) -> BoxFuture<'a, StageResult>;

    fn terminate_tree<'a>(
        &'a self,
        ticket: &'a TeardownTicket,
        deadline: CleanupDeadline,
    ) -> BoxFuture<'a, StageResult>;

    fn wait_for_zero<'a>(
        &'a self,
        ticket: &'a TeardownTicket,
        stage: WaitStage,
        deadline: CleanupDeadline,
    ) -> BoxFuture<'a, WaitResult>;

    fn settle_active_process_zero<'a>(
        &'a self,
        ticket: &'a TeardownTicket,
        deadline: CleanupDeadline,
    ) -> BoxFuture<'a, StageResult>;

    fn detach_after_zero<'a>(
        &'a self,
        ticket: &'a TeardownTicket,
        deadline: CleanupDeadline,
    ) -> BoxFuture<'a, StageResult>;

    fn reconcile_ports<'a>(
        &'a self,
        ticket: &'a TeardownTicket,
        deadline: CleanupDeadline,
    ) -> BoxFuture<'a, StageResult>;

    fn persist_settlement<'a>(
        &'a self,
        ticket: &'a TeardownTicket,
        deadline: CleanupDeadline,
    ) -> BoxFuture<'a, StageResult>;

    fn residue<'a>(
        &'a self,
        ticket: &'a TeardownTicket,
        deadline: CleanupDeadline,
    ) -> BoxFuture<'a, Option<ResidueEvidence>>;

    fn release_stopped_exact<'a>(
        &'a self,
        ticket: &'a TeardownTicket,
        deadline: CleanupDeadline,
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
    stage_notes: Vec<String>,
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

    pub fn stage_notes(&self) -> &[String] {
        &self.stage_notes
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
    input_admission: Arc<AtomicBool>,
    detached: AtomicBool,
}

#[cfg(windows)]
impl ManagedTerminalIo {
    pub(crate) fn new(
        writer: Arc<Mutex<Box<dyn Write + Send>>>,
        master: Arc<Mutex<Option<Box<dyn MasterPty + Send>>>>,
        actors: Arc<Mutex<ManagedTerminalActorHandles>>,
        input_admission: Arc<AtomicBool>,
    ) -> Arc<Self> {
        Arc::new(Self {
            writer,
            master,
            actors,
            input_admission,
            detached: AtomicBool::new(false),
        })
    }

    #[cfg(test)]
    fn empty() -> Arc<Self> {
        Self::new(
            Arc::new(Mutex::new(Box::new(std::io::sink()))),
            Arc::new(Mutex::new(None)),
            Arc::new(Mutex::new(ManagedTerminalActorHandles::default())),
            Arc::new(AtomicBool::new(true)),
        )
    }

    pub(crate) fn open_input_after_start(&self) -> Result<(), String> {
        let _writer = lock_mutex_until(
            &self.writer,
            fail_closed_shutdown_deadline(),
            "terminal PTY input-open setup",
        )?;
        self.input_admission.store(true, Ordering::Release);
        Ok(())
    }

    async fn begin_drain(&self, deadline: CleanupDeadline) -> Result<(), String> {
        while !self.try_begin_drain()? {
            deadline.check("terminal input drain")?;
            tokio::task::yield_now().await;
        }
        deadline.check("terminal input drain")?;
        Ok(())
    }

    fn try_begin_drain(&self) -> Result<bool, String> {
        match self.writer.try_lock() {
            Ok(_writer) => {
                self.input_admission.store(false, Ordering::Release);
                Ok(true)
            }
            Err(std::sync::TryLockError::WouldBlock) => Ok(false),
            Err(std::sync::TryLockError::Poisoned(_)) => {
                Err("terminal PTY writer poisoned".to_string())
            }
        }
    }

    async fn close_input(&self, deadline: CleanupDeadline) -> Result<(), String> {
        while !self.try_close_input()? {
            deadline.check("terminal input close")?;
            tokio::task::yield_now().await;
        }
        deadline.check("terminal input close")?;
        Ok(())
    }

    fn try_close_input(&self) -> Result<bool, String> {
        match self.writer.try_lock() {
            Ok(mut writer) => {
                self.input_admission.store(false, Ordering::Release);
                let old = std::mem::replace(&mut *writer, Box::new(std::io::sink()));
                drop(old);
                Ok(true)
            }
            Err(std::sync::TryLockError::WouldBlock) => Ok(false),
            Err(std::sync::TryLockError::Poisoned(_)) => {
                Err("terminal PTY writer poisoned".to_string())
            }
        }
    }

    async fn detach_after_zero(&self, deadline: CleanupDeadline) -> Result<(), String> {
        if self.detached.load(Ordering::Acquire) {
            return Ok(());
        }
        deadline.check("terminal post-zero detach")?;
        self.close_input(deadline).await?;
        while !self.try_close_master()? {
            deadline.check("terminal PTY master detach")?;
            tokio::task::yield_now().await;
        }

        let current = thread::current().id();
        let mut actors = loop {
            if let Some(actors) = self.try_take_finished_actors(current)? {
                break actors;
            }
            deadline.check("terminal actor join")?;
            tokio::time::sleep(Duration::from_millis(2)).await;
        };
        if let Some(reader) = actors.reader.take() {
            deadline.check("terminal reader actor join")?;
            reader
                .join()
                .map_err(|_| "terminal reader actor panicked".to_string())?;
            deadline.check("terminal reader actor join")?;
        }
        if let Some(waiter) = actors.waiter.take() {
            deadline.check("terminal wait actor join")?;
            waiter
                .join()
                .map_err(|_| "terminal wait actor panicked".to_string())?;
            deadline.check("terminal wait actor join")?;
        }
        self.detached.store(true, Ordering::Release);
        Ok(())
    }

    fn try_close_master(&self) -> Result<bool, String> {
        match self.master.try_lock() {
            Ok(mut master) => {
                master.take();
                Ok(true)
            }
            Err(std::sync::TryLockError::WouldBlock) => Ok(false),
            Err(std::sync::TryLockError::Poisoned(_)) => {
                Err("terminal PTY master poisoned".to_string())
            }
        }
    }

    fn try_take_finished_actors(
        &self,
        current: thread::ThreadId,
    ) -> Result<Option<ManagedTerminalActorHandles>, String> {
        let mut actors = match self.actors.try_lock() {
            Ok(actors) => actors,
            Err(std::sync::TryLockError::WouldBlock) => return Ok(None),
            Err(std::sync::TryLockError::Poisoned(_)) => {
                return Err("terminal actor handles poisoned".to_string())
            }
        };
        for handle in [&actors.reader, &actors.waiter].into_iter().flatten() {
            if handle.thread().id() == current {
                return Err(
                    "terminal teardown actor attempted to synchronously join itself".to_string(),
                );
            }
            if !handle.is_finished() {
                // SAFETY: JoinHandle owns the exact actor thread handle. This
                // only cancels that actor's synchronous PTY/wait operation;
                // ownership remains in the slot until it acknowledges and is
                // joined below.
                unsafe {
                    let _ = CancelSynchronousIo(handle.as_raw_handle());
                }
            }
        }
        if [&actors.reader, &actors.waiter]
            .into_iter()
            .flatten()
            .any(|handle| !handle.is_finished())
        {
            return Ok(None);
        }
        Ok(Some(std::mem::take(&mut *actors)))
    }

    fn detached_and_joined(&self) -> bool {
        self.detached.load(Ordering::Acquire)
            && self
                .actors
                .lock()
                .map(|actors| actors.reader.is_none() && actors.waiter.is_none())
                .unwrap_or(false)
    }
}

#[cfg(windows)]
pub(crate) fn validate_terminal_teardown_inputs(
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
    job_internal_name: String,
    display_label: String,
    released_exact: bool,
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
        let mut state = lock_mutex_until(
            &teardown.state,
            fail_closed_shutdown_deadline(),
            "terminal process registry handoff before resume",
        )?;
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
        let (job_internal_name, display_label) = registry
            .current(fence.resource().resource_id)
            .map(|process| {
                (
                    process.job().internal_name().to_string(),
                    process.display_label().to_string(),
                )
            })
            .expect("registered terminal fence has an authoritative Job entry");
        let state = Arc::new(Mutex::new(ManagedTerminalTeardownState {
            registry,
            fence,
            release_authority: None,
            session_id,
            ports,
            io,
            job_internal_name,
            display_label,
            released_exact: false,
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
        // The coordinator is itself the idempotent exact-key waiter registry.
        // Keeping a second terminal-side waiter cache made a failed release
        // non-retryable when that cache could not be cleared and introduced a
        // second timeout before the operation's one absolute deadline.
        self.coordinator
            .request(self.ticket.clone())
            .map_err(|error| format!("terminal teardown admission failed: {error:?}"))
    }

    /// Starts exact teardown without blocking the terminal reader/wait actor.
    /// The actor can then exit and be joined by the host's synchronous close.
    pub(crate) fn request_close(&self) -> Result<(), String> {
        self.waiter().map(|_| ())
    }

    pub(crate) fn matches_fence(&self, expected: &ManagedProcessFence) -> bool {
        self.ticket.fence() == expected
    }

    pub(crate) fn actors_joined(&self) -> bool {
        self.state
            .lock()
            .map(|state| state.io.detached_and_joined())
            .unwrap_or(false)
    }

    pub(crate) fn close(&self) -> Result<TeardownReport, String> {
        let waiter = self.waiter()?;
        if let Some(report) = self
            .report
            .try_lock()
            .ok()
            .and_then(|report| report.clone())
        {
            return Ok(report);
        }

        // `TerminalSession::close` is synchronous and is also called by its
        // Drop implementation. The waiter carries the coordinator's one
        // absolute admission/effect/settlement deadline; this bridge must not
        // mint a later timeout or let a result mutex extend that authority.
        let report = match waiter.wait_blocking() {
            Ok(report) => report,
            Err(wait_error) => {
                // A host adapter that violates its bounded contract must not
                // strand the process or a worker. Shutdown requests
                // cancellation and joins the fixed executor. Do not mint a
                // second waiter deadline after the operation already expired.
                self.coordinator.shutdown();
                return Err(format!("terminal teardown wait failed: {wait_error}"));
            }
        };
        if report.outcome() == TeardownOutcome::Closed {
            if let Ok(mut retained) = self.report.try_lock() {
                *retained = Some(report.clone());
            }
        }
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
        let state = &self.state;
        let state = lock_mutex_until(
            state,
            fail_closed_shutdown_deadline(),
            "terminal managed-process snapshot",
        )?;
        let active_process_ids = state
            .registry
            .current(state.fence.resource().resource_id)
            .map(|process| process.job().active_process_ids())
            .unwrap_or_else(|| Ok(Vec::new()))?;
        Ok((state.fence.clone(), active_process_ids))
    }

    /// Returns the exact registry fence and exact Job-member observations
    /// while retaining the same teardown registry lock for the whole query.
    /// The returned values are read-only; exact close still revalidates the
    /// fence against the retained registry/Job authority.
    pub(crate) fn managed_process_observations_until(
        &self,
        absolute_deadline: Instant,
        max_members: usize,
    ) -> Result<(ManagedProcessFence, Vec<JobMemberObservation>), String> {
        if Instant::now() >= absolute_deadline {
            return Err("terminal managed-process observation exceeded deadline".to_string());
        }
        let state = lock_mutex_until(
            &self.state,
            absolute_deadline,
            "terminal managed-process observation",
        )?;
        if Instant::now() >= absolute_deadline {
            return Err("terminal managed-process observation exceeded deadline".to_string());
        }

        let resource_id = state.fence.resource().resource_id;
        if Instant::now() >= absolute_deadline {
            return Err("terminal managed-process observation exceeded deadline".to_string());
        }
        let process = state.registry.current(resource_id);
        if Instant::now() >= absolute_deadline {
            return Err("terminal managed-process observation exceeded deadline".to_string());
        }
        let observations = match process {
            Some(process) => process
                .job()
                .active_process_observations_until(absolute_deadline, max_members)?,
            None => Vec::new(),
        };
        if Instant::now() >= absolute_deadline {
            return Err("terminal managed-process observation exceeded deadline".to_string());
        }
        Ok((state.fence.clone(), observations))
    }
}

#[cfg(windows)]
impl Drop for ManagedTerminalTeardown {
    fn drop(&mut self) {
        if self.armed.load(Ordering::Acquire) {
            let already_closed = self
                .report
                .try_lock()
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

#[cfg(all(windows, not(test)))]
impl sealed::Admission for TerminalTeardownAdmission {}

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
        deadline: CleanupDeadline,
    ) -> Result<Vec<AdmissionReceipt>, TeardownAdmissionError> {
        for ticket in tickets {
            self.validate_ticket(ticket)?;
        }
        let mut state =
            lock_mutex_until(&self.state, deadline.absolute, "terminal admission close")
                .map_err(|detail| TeardownAdmissionError::Timeout { detail })?;
        if *state == AdmissionState::Closed {
            return Err(TeardownAdmissionError::Other {
                detail: "terminal admission is already closed".to_string(),
            });
        }
        *state = AdmissionState::Closing;
        let receipts: Vec<_> = tickets
            .iter()
            .map(|ticket| {
                AdmissionReceipt::new(
                    ticket.scope(),
                    AdmissionState::Closing,
                    ticket.action_epoch(),
                    self.fence.clone(),
                )
            })
            .collect();
        if let Err(detail) = deadline.check("terminal admission close") {
            *state = AdmissionState::Open;
            return Err(TeardownAdmissionError::Timeout { detail });
        }
        Ok(receipts)
    }

    fn rollback_admission_batch(
        &self,
        tickets: &[TeardownTicket],
        receipts: &[AdmissionReceipt],
        deadline: CleanupDeadline,
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
        let mut state = lock_mutex_until(
            &self.state,
            deadline.absolute,
            "terminal admission rollback",
        )
        .map_err(|detail| TeardownAdmissionError::Timeout { detail })?;
        if *state != AdmissionState::Closing {
            return Err(TeardownAdmissionError::Other {
                detail: "terminal admission was not closing during rollback".to_string(),
            });
        }
        *state = AdmissionState::Open;
        deadline
            .check("terminal admission rollback")
            .map_err(|detail| TeardownAdmissionError::Timeout { detail })?;
        Ok(())
    }
}

#[cfg(windows)]
struct TerminalTeardownEffects {
    state: Arc<Mutex<ManagedTerminalTeardownState>>,
}

#[cfg(all(windows, not(test)))]
impl sealed::Effects for TerminalTeardownEffects {}

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
        deadline: CleanupDeadline,
    ) -> Result<bool, String> {
        deadline.check("terminal ACTIVE_PROCESS_ZERO reconciliation")?;
        Self::validate_ticket(state, ticket)?;
        if state.released_exact {
            return Ok(true);
        }
        state
            .registry
            .drain_job_completions_until(ticket.resource_id(), deadline.absolute)
            .map_err(|error| error.to_string())?;
        deadline.check("terminal ACTIVE_PROCESS_ZERO reconciliation")?;
        state
            .registry
            .reconcile_membership_until(ticket.resource_id(), deadline.absolute)
            .map_err(|error| error.to_string())?;
        deadline.check("terminal ACTIVE_PROCESS_ZERO reconciliation")?;
        Ok(state
            .registry
            .current(ticket.resource_id())
            .is_some_and(|process| {
                process.state() == ManagedProcessState::ZeroSettled && process.member_count() == 0
            }))
    }
}

#[cfg(windows)]
impl TeardownEffects for TerminalTeardownEffects {
    fn drain<'a>(
        &'a self,
        ticket: &'a TeardownTicket,
        deadline: CleanupDeadline,
    ) -> BoxFuture<'a, StageResult> {
        let state = Arc::clone(&self.state);
        let ticket = ticket.clone();
        Box::pin(async move {
            let io = {
                let mut state = match lock_mutex_until(
                    &state,
                    deadline.absolute,
                    "terminal drain registry lookup",
                ) {
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
                if let Err(error) = state
                    .registry
                    .drain_job_completions_until(ticket.resource_id(), deadline.absolute)
                {
                    return StageResult::Failed {
                        detail: error.to_string(),
                    };
                }
                if let Err(detail) = deadline.check("terminal drain registry lookup") {
                    return StageResult::Failed { detail };
                }
                Arc::clone(&state.io)
            };
            match io.begin_drain(deadline).await {
                Ok(()) => StageResult::Completed,
                Err(detail) => StageResult::Failed { detail },
            }
        })
    }

    fn cooperative_close<'a>(
        &'a self,
        ticket: &'a TeardownTicket,
        deadline: CleanupDeadline,
    ) -> BoxFuture<'a, StageResult> {
        let state = Arc::clone(&self.state);
        let ticket = ticket.clone();
        Box::pin(async move {
            let state = match lock_mutex_until(
                &state,
                deadline.absolute,
                "terminal cooperative-close capability lookup",
            ) {
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
            StageResult::Unsupported {
                detail: "native ConPTY has no provider-level graceful close capability".to_string(),
            }
        })
    }

    fn interrupt_or_safe_close<'a>(
        &'a self,
        ticket: &'a TeardownTicket,
        deadline: CleanupDeadline,
    ) -> BoxFuture<'a, StageResult> {
        let state = Arc::clone(&self.state);
        let ticket = ticket.clone();
        Box::pin(async move {
            let io = {
                let state = match lock_mutex_until(
                    &state,
                    deadline.absolute,
                    "terminal input-close registry lookup",
                ) {
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
            match io.close_input(deadline).await {
                Ok(()) => StageResult::Completed,
                Err(detail) => StageResult::Failed { detail },
            }
        })
    }

    fn terminate_tree<'a>(
        &'a self,
        ticket: &'a TeardownTicket,
        deadline: CleanupDeadline,
    ) -> BoxFuture<'a, StageResult> {
        let state = Arc::clone(&self.state);
        let ticket = ticket.clone();
        Box::pin(async move {
            let state = match lock_mutex_until(
                &state,
                deadline.absolute,
                "terminal Job termination lookup",
            ) {
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
            if state.released_exact {
                return StageResult::Completed;
            }
            let result = state
                .registry
                .current(ticket.resource_id())
                .map(|process| process.job().terminate_tree_until(deadline.absolute))
                .unwrap_or_else(|| Err("terminal process registry entry is missing".to_string()));
            if let Err(detail) = deadline.check("terminal Job termination") {
                return StageResult::Failed { detail };
            }
            match result {
                Ok(()) => StageResult::Completed,
                Err(detail) => StageResult::Failed { detail },
            }
        })
    }

    fn wait_for_zero<'a>(
        &'a self,
        ticket: &'a TeardownTicket,
        stage: WaitStage,
        deadline: CleanupDeadline,
    ) -> BoxFuture<'a, WaitResult> {
        let state = Arc::clone(&self.state);
        let ticket = ticket.clone();
        Box::pin(async move {
            let _ = stage;
            loop {
                if deadline.check("terminal ACTIVE_PROCESS_ZERO wait").is_err() {
                    return WaitResult::TimedOut;
                }
                let zero = match lock_mutex_until(
                    &state,
                    deadline.absolute,
                    "terminal ACTIVE_PROCESS_ZERO state lookup",
                ) {
                    Ok(mut state) => match Self::zero_state(&mut state, &ticket, deadline) {
                        Ok(zero) => zero,
                        Err(detail) => {
                            return if deadline.check("terminal ACTIVE_PROCESS_ZERO wait").is_err() {
                                WaitResult::TimedOut
                            } else {
                                WaitResult::Failed { detail }
                            }
                        }
                    },
                    Err(detail) => {
                        return if deadline.check("terminal ACTIVE_PROCESS_ZERO wait").is_err() {
                            WaitResult::TimedOut
                        } else {
                            WaitResult::Failed { detail }
                        }
                    }
                };
                if zero {
                    return WaitResult::Zero;
                }
                let remaining = match deadline.check("terminal ACTIVE_PROCESS_ZERO wait") {
                    Ok(remaining) => remaining,
                    Err(_) => return WaitResult::TimedOut,
                };
                tokio::time::sleep(remaining.min(Duration::from_millis(5))).await;
            }
        })
    }

    fn settle_active_process_zero<'a>(
        &'a self,
        ticket: &'a TeardownTicket,
        deadline: CleanupDeadline,
    ) -> BoxFuture<'a, StageResult> {
        let state = Arc::clone(&self.state);
        let ticket = ticket.clone();
        Box::pin(async move {
            let mut state =
                match lock_mutex_until(&state, deadline.absolute, "terminal zero-proof settlement")
                {
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
            if state.released_exact {
                return StageResult::Completed;
            }
            if state.release_authority.is_some() {
                // The registry settlement is irreversible: its zero nonce was
                // consumed only to mint this exact authority. If the caller's
                // deadline expired immediately afterward, retain and reuse
                // the authority instead of asking for a now-consumed proof.
                return StageResult::Completed;
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
            if let Err(detail) = deadline.check("terminal zero-proof settlement") {
                return StageResult::Failed { detail };
            }
            match state.registry.mint_teardown_release_authority_exact_until(
                &ticket,
                proof,
                deadline.absolute,
            ) {
                Ok(authority) => {
                    state.release_authority = Some(authority);
                    match deadline.check("terminal zero-proof settlement") {
                        Ok(_) => StageResult::Completed,
                        Err(detail) => StageResult::Failed { detail },
                    }
                }
                Err(error) => StageResult::Failed {
                    detail: error.to_string(),
                },
            }
        })
    }

    fn detach_after_zero<'a>(
        &'a self,
        ticket: &'a TeardownTicket,
        deadline: CleanupDeadline,
    ) -> BoxFuture<'a, StageResult> {
        let state = Arc::clone(&self.state);
        let ticket = ticket.clone();
        Box::pin(async move {
            let io = {
                let state = match lock_mutex_until(
                    &state,
                    deadline.absolute,
                    "terminal post-zero detach lookup",
                ) {
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
            match io.detach_after_zero(deadline).await {
                Ok(()) => StageResult::Completed,
                Err(detail) => StageResult::Failed { detail },
            }
        })
    }

    fn reconcile_ports<'a>(
        &'a self,
        ticket: &'a TeardownTicket,
        deadline: CleanupDeadline,
    ) -> BoxFuture<'a, StageResult> {
        let state = Arc::clone(&self.state);
        let ticket = ticket.clone();
        Box::pin(async move {
            let state = match lock_mutex_until(
                &state,
                deadline.absolute,
                "terminal port reconciliation lookup",
            ) {
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
            if !state.released_exact {
                return StageResult::Failed {
                    detail: "terminal ports cannot reconcile before exact Job release".to_string(),
                };
            }
            if state.ports.is_empty() {
                return StageResult::Completed;
            }
            if let Err(detail) = deadline.check("terminal port reconciliation") {
                return StageResult::Failed { detail };
            }
            let listeners = match crate::services::platform_service::snapshot_listener_pids_until(
                &state.ports,
                deadline.absolute,
            ) {
                Ok(listeners) => listeners,
                Err(detail) => {
                    let detail = deadline
                        .check("terminal port reconciliation")
                        .err()
                        .unwrap_or(detail);
                    return StageResult::Failed { detail };
                }
            };
            if let Err(detail) = deadline.check("terminal port reconciliation") {
                return StageResult::Failed { detail };
            }
            let _ = listeners;
            // A listener that is not an exact member is external. It remains
            // untouched and the normal background port inventory will expose
            // it as externally occupied (blue).
            StageResult::Completed
        })
    }

    fn persist_settlement<'a>(
        &'a self,
        ticket: &'a TeardownTicket,
        deadline: CleanupDeadline,
    ) -> BoxFuture<'a, StageResult> {
        let state = Arc::clone(&self.state);
        let ticket = ticket.clone();
        Box::pin(async move {
            let (session_id, root_pid, already_persisted) = {
                let state = match lock_mutex_until(
                    &state,
                    deadline.absolute,
                    "terminal settlement ledger lookup",
                ) {
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
                if !state.released_exact {
                    return StageResult::Failed {
                        detail: "terminal settlement cannot publish before exact Job release"
                            .to_string(),
                    };
                }
                (
                    state.session_id.clone(),
                    state.fence.root().id().pid(),
                    state.settlement_persisted,
                )
            };
            if !already_persisted {
                if let Err(detail) = crate::services::pid_file::release_session_root_after_job_zero(
                    &session_id,
                    root_pid,
                    deadline.absolute,
                ) {
                    return StageResult::Failed { detail };
                }
                if let Err(detail) = deadline.check("terminal settlement ledger persistence") {
                    return StageResult::Failed { detail };
                }
                let mut state = match lock_mutex_until(
                    &state,
                    deadline.absolute,
                    "terminal settlement publication",
                ) {
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

    fn residue<'a>(
        &'a self,
        ticket: &'a TeardownTicket,
        deadline: CleanupDeadline,
    ) -> BoxFuture<'a, Option<ResidueEvidence>> {
        let state = Arc::clone(&self.state);
        let ticket = ticket.clone();
        Box::pin(async move {
            let state =
                lock_mutex_until(&state, deadline.absolute, "terminal residue lookup").ok()?;
            Self::validate_ticket(&state, &ticket).ok()?;
            let root = state.fence.root();
            Some(ResidueEvidence::new(
                &state.job_internal_name,
                root.id().pid(),
                root.id().creation_time_100ns(),
                root.canonical_executable().display().to_string(),
                &state.display_label,
                "terminal teardown retained managed Job",
                Vec::new(),
            ))
        })
    }

    fn release_stopped_exact<'a>(
        &'a self,
        ticket: &'a TeardownTicket,
        deadline: CleanupDeadline,
    ) -> BoxFuture<'a, StageResult> {
        let state = Arc::clone(&self.state);
        let ticket = ticket.clone();
        Box::pin(async move {
            let mut state =
                match lock_mutex_until(&state, deadline.absolute, "terminal exact Job release") {
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
                released_exact,
                ..
            } = &mut *state;
            if *released_exact {
                return StageResult::Completed;
            }
            let Some(authority) = release_authority.as_ref() else {
                return StageResult::Failed {
                    detail: "terminal teardown release authority was not minted".to_string(),
                };
            };
            match registry.release_stopped_with_authority_until(
                &ticket,
                authority,
                deadline.absolute,
            ) {
                Ok(crate::process::registry::UnregisterOutcome::Removed(_)) => {
                    *release_authority = None;
                    *released_exact = true;
                    match deadline.check("terminal exact Job release") {
                        Ok(_) => StageResult::Completed,
                        Err(detail) => StageResult::Failed { detail },
                    }
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
    attempts: Mutex<VecDeque<(TeardownCompletionKey, TeardownAttemptState)>>,
    durable_path: Option<PathBuf>,
    persist_error: Mutex<Option<String>>,
    lookup_blocked: AtomicBool,
    lookup_started: AtomicUsize,
    persist_blocked: AtomicBool,
    persist_active: AtomicUsize,
    persist_max_active: AtomicUsize,
    #[cfg(test)]
    begin_attempt_fail_after_writes: AtomicUsize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum TeardownAttemptState {
    InProgress,
    RetryableFailure(TeardownReport),
    EffectsClosed(TeardownReport),
}

impl TeardownCompletionStore {
    /// Opens the host-owned durable idempotency journal.  Production terminal
    /// launch authority must carry a store created through this constructor;
    /// pure coordinator tests use the in-memory `Default` implementation.
    pub(crate) fn durable(path: impl AsRef<Path>) -> Result<Self, String> {
        let path = path.as_ref();
        if !teardown_host_path_within_bound(path) {
            return Err("teardown completion journal path exceeds host string bound".to_string());
        }
        let path = path.to_path_buf();
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
    pub(crate) fn clear_persist_failure_for_test(&self) {
        *self
            .inner
            .persist_error
            .lock()
            .expect("completion store persist error") = None;
    }

    #[doc(hidden)]
    #[cfg(test)]
    pub(crate) fn fail_begin_attempt_after_writes_for_test(&self, writes: usize) {
        self.inner
            .begin_attempt_fail_after_writes
            .store(writes, Ordering::SeqCst);
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
        let cached = lock_mutex_until(
            &self.inner.reports,
            absolute_deadline,
            "completion report cache lookup",
        )?
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
                    "SELECT length(report_json),
                            CASE WHEN length(report_json) <= ?2 THEN report_json ELSE NULL END
                     FROM teardown_completions WHERE completion_key = ?1",
                    params![key_json, MAX_DURABLE_REPORT_BYTES as i64],
                    |row| Ok((row.get::<_, i64>(0)?, row.get::<_, Option<String>>(1)?)),
                )
                .optional()
                .map_err(|error| format!("read teardown completion journal: {error}"))?;
            match payload {
                Some((length, None)) if length > MAX_DURABLE_REPORT_BYTES as i64 => {
                    return Err(format!(
                        "durable teardown completion report exceeds {} bytes",
                        MAX_DURABLE_REPORT_BYTES
                    ));
                }
                Some((_, Some(payload))) => Some(decode_durable_report(key, &payload)?),
                Some((_, None)) => {
                    return Err("durable teardown completion report is unavailable".to_string());
                }
                None => None,
            }
        } else {
            None
        };
        checked_remaining_until(absolute_deadline, "completion lookup")?;
        Ok(report)
    }

    fn lookup_attempt(
        &self,
        key: &TeardownCompletionKey,
        absolute_deadline: Instant,
    ) -> Result<Option<TeardownAttemptState>, String> {
        checked_remaining_until(absolute_deadline, "teardown attempt lookup")?;
        let cached = lock_mutex_until(
            &self.inner.attempts,
            absolute_deadline,
            "teardown attempt cache lookup",
        )?
        .iter()
        .find(|(stored_key, _)| stored_key == key)
        .map(|(_, state)| state.clone());
        let attempt = if cached.is_some() {
            cached
        } else if let Some(path) = self.inner.durable_path.as_ref() {
            let remaining = checked_remaining_until(absolute_deadline, "teardown attempt lookup")?;
            let connection = open_completion_journal(path, remaining)?;
            let key_json = durable_completion_key(key)?;
            let row = connection
                .query_row(
                    "SELECT status, length(report_json),
                            CASE WHEN report_json IS NULL OR length(report_json) <= ?2
                                 THEN report_json ELSE NULL END
                     FROM teardown_attempts WHERE completion_key = ?1",
                    params![key_json, MAX_DURABLE_REPORT_BYTES as i64],
                    |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, Option<i64>>(1)?,
                            row.get::<_, Option<String>>(2)?,
                        ))
                    },
                )
                .optional()
                .map_err(|error| format!("read teardown attempt journal: {error}"))?;
            match row {
                None => None,
                Some((_, Some(length), None)) if length > MAX_DURABLE_REPORT_BYTES as i64 => {
                    return Err(format!(
                        "durable teardown attempt report exceeds {} bytes",
                        MAX_DURABLE_REPORT_BYTES
                    ));
                }
                Some((status, _, payload)) => {
                    Some(decode_attempt_state(key, &status, payload.as_deref())?)
                }
            }
        } else {
            None
        };
        checked_remaining_until(absolute_deadline, "teardown attempt lookup")?;
        Ok(attempt)
    }

    fn begin_attempt(
        &self,
        key: &TeardownCompletionKey,
        absolute_deadline: Instant,
    ) -> Result<(), String> {
        self.begin_attempt_batch(std::slice::from_ref(key), absolute_deadline)
    }

    fn begin_attempt_batch(
        &self,
        keys: &[TeardownCompletionKey],
        absolute_deadline: Instant,
    ) -> Result<(), String> {
        if keys.len() > MAX_TEARDOWN_BATCH_ITEMS {
            return Err("teardown attempt batch exceeds bounded capacity".to_string());
        }
        checked_remaining_until(absolute_deadline, "teardown attempt batch persistence")?;

        // Validate every caller-derived host field before opening a journal or
        // allocating any durable key. A later invalid member therefore cannot
        // leave a valid prefix admitted as InProgress.
        for key in keys {
            if !teardown_host_path_within_bound(key.fence.root().canonical_executable()) {
                return Err("teardown attempt host identity exceeds bounded capacity".to_string());
            }
        }

        if let Some(path) = self.inner.durable_path.as_ref() {
            let remaining =
                checked_remaining_until(absolute_deadline, "teardown attempt batch persistence")?;
            let mut connection = open_completion_journal(path, remaining)?;
            checked_remaining_until(absolute_deadline, "teardown attempt batch persistence")?;
            let transaction = connection
                .transaction()
                .map_err(|error| format!("begin teardown attempt batch transaction: {error}"))?;
            for (_index, key) in keys.iter().enumerate() {
                checked_remaining_until(absolute_deadline, "teardown attempt batch persistence")?;
                let key_json = durable_completion_key(key)?;
                transaction
                    .execute(
                        "INSERT INTO teardown_attempts(completion_key, status, report_json) VALUES (?1, 'in_progress', NULL)
                         ON CONFLICT(completion_key) DO UPDATE SET status = excluded.status, report_json = excluded.report_json",
                        params![key_json],
                    )
                    .map_err(|error| format!("persist teardown attempt batch: {error}"))?;
                #[cfg(test)]
                {
                    let fail_after = self
                        .inner
                        .begin_attempt_fail_after_writes
                        .load(Ordering::SeqCst);
                    if fail_after != 0 && _index + 1 >= fail_after {
                        return Err("injected teardown attempt batch failure".to_string());
                    }
                }
                checked_remaining_until(absolute_deadline, "teardown attempt batch persistence")?;
            }
            transaction
                .execute(
                    "DELETE FROM teardown_attempts
                     WHERE rowid NOT IN (
                       SELECT rowid FROM teardown_attempts
                       ORDER BY rowid DESC LIMIT ?1
                     )",
                    params![DEFAULT_COMPLETED_OPERATION_CAPACITY as i64],
                )
                .map_err(|error| format!("bound teardown attempt batch journal: {error}"))?;
            checked_remaining_until(absolute_deadline, "teardown attempt batch persistence")?;
            transaction
                .commit()
                .map_err(|error| format!("commit teardown attempt batch: {error}"))?;
            checked_remaining_until(absolute_deadline, "teardown attempt batch persistence")?;
        }

        let mut attempts = lock_mutex_until(
            &self.inner.attempts,
            absolute_deadline,
            "teardown attempt batch cache persistence",
        )?;
        for key in keys {
            if let Some((_, stored)) = attempts
                .iter_mut()
                .find(|(stored_key, _)| stored_key == key)
            {
                *stored = TeardownAttemptState::InProgress;
            } else {
                if attempts.len() >= DEFAULT_COMPLETED_OPERATION_CAPACITY {
                    attempts.pop_front();
                }
                attempts.push_back((key.clone(), TeardownAttemptState::InProgress));
            }
        }
        drop(attempts);
        checked_remaining_until(absolute_deadline, "teardown attempt batch persistence")?;
        Ok(())
    }

    fn record_attempt(
        &self,
        key: &TeardownCompletionKey,
        attempt: TeardownAttemptState,
        absolute_deadline: Instant,
    ) -> Result<(), String> {
        checked_remaining_until(absolute_deadline, "teardown attempt persistence")?;
        if let Some(path) = self.inner.durable_path.as_ref() {
            let remaining =
                checked_remaining_until(absolute_deadline, "teardown attempt persistence")?;
            let mut connection = open_completion_journal(path, remaining)?;
            let key_json = durable_completion_key(key)?;
            let (status, report_json) = encode_attempt_state(&attempt)?;
            checked_remaining_until(absolute_deadline, "teardown attempt persistence")?;
            let transaction = connection
                .transaction()
                .map_err(|error| format!("begin teardown attempt transaction: {error}"))?;
            checked_remaining_until(absolute_deadline, "teardown attempt persistence")?;
            transaction
                .execute(
                    "INSERT INTO teardown_attempts(completion_key, status, report_json) VALUES (?1, ?2, ?3)
                     ON CONFLICT(completion_key) DO UPDATE SET status = excluded.status, report_json = excluded.report_json",
                    params![key_json, status, report_json],
                )
                .map_err(|error| format!("persist teardown attempt: {error}"))?;
            checked_remaining_until(absolute_deadline, "teardown attempt persistence")?;
            transaction
                .execute(
                    "DELETE FROM teardown_attempts
                     WHERE rowid NOT IN (
                       SELECT rowid FROM teardown_attempts
                       ORDER BY rowid DESC LIMIT ?1
                     )",
                    params![DEFAULT_COMPLETED_OPERATION_CAPACITY as i64],
                )
                .map_err(|error| format!("bound teardown attempt journal: {error}"))?;
            checked_remaining_until(absolute_deadline, "teardown attempt persistence")?;
            transaction
                .commit()
                .map_err(|error| format!("commit teardown attempt: {error}"))?;
            checked_remaining_until(absolute_deadline, "teardown attempt persistence")?;
        }
        let mut attempts = lock_mutex_until(
            &self.inner.attempts,
            absolute_deadline,
            "teardown attempt cache persistence",
        )?;
        if let Some((_, stored)) = attempts
            .iter_mut()
            .find(|(stored_key, _)| stored_key == key)
        {
            *stored = attempt;
        } else {
            if attempts.len() >= DEFAULT_COMPLETED_OPERATION_CAPACITY {
                attempts.pop_front();
            }
            attempts.push_back((key.clone(), attempt));
        }
        drop(attempts);
        checked_remaining_until(absolute_deadline, "teardown attempt persistence")?;
        Ok(())
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

        if let Some(detail) = lock_mutex_until(
            &self.inner.persist_error,
            absolute_deadline,
            "completion persistence fault lookup",
        )?
        .clone()
        {
            return Err(detail);
        }
        if let Some(path) = self.inner.durable_path.as_ref() {
            let remaining = checked_remaining_until(absolute_deadline, "completion persistence")?;
            let mut connection = open_completion_journal(path, remaining)?;
            let key_json = durable_completion_key(key)?;
            let report_json = encode_durable_report(report)?;
            checked_remaining_until(absolute_deadline, "completion persistence")?;
            let transaction = connection
                .transaction()
                .map_err(|error| format!("begin teardown completion transaction: {error}"))?;
            checked_remaining_until(absolute_deadline, "completion persistence")?;
            transaction
                .execute(
                    "INSERT INTO teardown_completions(completion_key, report_json) VALUES (?1, ?2)
                     ON CONFLICT(completion_key) DO UPDATE SET report_json = excluded.report_json",
                    params![key_json, report_json],
                )
                .map_err(|error| format!("persist teardown completion: {error}"))?;
            checked_remaining_until(absolute_deadline, "completion persistence")?;
            transaction
                .execute(
                    "DELETE FROM teardown_attempts WHERE completion_key = ?1",
                    params![key_json],
                )
                .map_err(|error| format!("clear settled teardown attempt: {error}"))?;
            checked_remaining_until(absolute_deadline, "completion persistence")?;
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
            checked_remaining_until(absolute_deadline, "completion persistence")?;
            transaction
                .commit()
                .map_err(|error| format!("commit teardown completion: {error}"))?;
            checked_remaining_until(absolute_deadline, "completion persistence")?;
        }

        let mut reports = lock_mutex_until(
            &self.inner.reports,
            absolute_deadline,
            "completion report cache persistence",
        )?;
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
        lock_mutex_until(
            &self.inner.attempts,
            absolute_deadline,
            "teardown attempt cache release",
        )?
        .retain(|(stored_key, _)| stored_key != key);
        checked_remaining_until(absolute_deadline, "completion persistence")?;
        Ok(())
    }
}

const COMPLETION_JOURNAL_SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS teardown_completions (
    completion_key TEXT PRIMARY KEY NOT NULL,
    report_json TEXT NOT NULL
) STRICT;
CREATE TABLE IF NOT EXISTS teardown_attempts (
    completion_key TEXT PRIMARY KEY NOT NULL,
    status TEXT NOT NULL,
    report_json TEXT
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
    if !teardown_host_path_within_bound(root.canonical_executable()) {
        return Err("teardown completion executable exceeds host string bound".to_string());
    }
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
    #[serde(default)]
    stage_notes: Vec<String>,
    errors: Vec<String>,
    residue: Option<DurableResidue>,
}

fn encode_attempt_state(
    attempt: &TeardownAttemptState,
) -> Result<(&'static str, Option<String>), String> {
    match attempt {
        TeardownAttemptState::InProgress => Ok(("in_progress", None)),
        TeardownAttemptState::RetryableFailure(report) => {
            Ok(("retryable_failure", Some(encode_durable_report(report)?)))
        }
        TeardownAttemptState::EffectsClosed(report) => {
            if report.outcome() != TeardownOutcome::Closed {
                return Err("effects-closed attempt must carry a Closed report".to_string());
            }
            Ok(("effects_closed", Some(encode_durable_report(report)?)))
        }
    }
}

fn decode_attempt_state(
    key: &TeardownCompletionKey,
    status: &str,
    payload: Option<&str>,
) -> Result<TeardownAttemptState, String> {
    match status {
        "in_progress" => {
            if payload.is_some() {
                return Err(
                    "in-progress teardown attempt unexpectedly carries a report".to_string()
                );
            }
            Ok(TeardownAttemptState::InProgress)
        }
        "retryable_failure" => Ok(TeardownAttemptState::RetryableFailure(
            decode_durable_report(
                key,
                payload.ok_or_else(|| {
                    "retryable teardown attempt is missing its report".to_string()
                })?,
            )?,
        )),
        "effects_closed" => {
            let report = decode_durable_report(
                key,
                payload.ok_or_else(|| {
                    "effects-closed teardown attempt is missing its report".to_string()
                })?,
            )?;
            if report.outcome() != TeardownOutcome::Closed {
                return Err("effects-closed teardown attempt report is not Closed".to_string());
            }
            Ok(TeardownAttemptState::EffectsClosed(report))
        }
        other => Err(format!("unknown durable teardown attempt status `{other}`")),
    }
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
        stage_notes: report
            .stage_notes
            .iter()
            .take(MAX_TEARDOWN_STAGE_NOTES)
            .cloned()
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
    if payload.len() > MAX_DURABLE_REPORT_BYTES {
        return Err(format!(
            "durable teardown completion report exceeds {} bytes",
            MAX_DURABLE_REPORT_BYTES
        ));
    }
    let durable: DurableReport = serde_json::from_str(payload)
        .map_err(|error| format!("decode teardown completion report: {error}"))?;
    if durable.attempted_stages.len() > MAX_RESIDUE_STAGES
        || durable.stage_notes.len() > MAX_TEARDOWN_STAGE_NOTES
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
        stage_notes: durable
            .stage_notes
            .into_iter()
            .map(|note| sanitize_text(&note))
            .collect(),
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
        detail: impl AsRef<str>,
    ) -> Self {
        let tickets = bounded_ticket_vec(tickets);
        let receipts = bounded_receipt_vec(receipts);
        Self {
            tickets,
            receipts,
            detail: sanitize_text(detail.as_ref()),
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
    absolute_deadline: Instant,
}

impl CleanupCell {
    fn new(ticket: &TeardownTicket, absolute_deadline: Instant) -> Self {
        let (done, _receiver) = watch::channel(false);
        Self {
            result: Mutex::new(None),
            done,
            blocking_done: Condvar::new(),
            fallback: waiter_failure_report(ticket.clone(), "teardown waiter channel closed"),
            absolute_deadline,
        }
    }

    fn finish(&self, report: TeardownReport) {
        let Ok(mut result) = self.lock_result_for_finish() else {
            std::process::abort();
        };
        if result.is_none() {
            *result = Some(report);
            self.done.send_replace(true);
        }
        self.blocking_done.notify_all();
    }

    fn lock_result_for_finish(&self) -> Result<MutexGuard<'_, Option<TeardownReport>>, String> {
        match self.result.try_lock() {
            // An uncontended immediate settlement is safe even at the exact
            // deadline boundary: it creates no new wait or authority window.
            Ok(result) => Ok(result),
            Err(TryLockError::WouldBlock) => lock_mutex_until(
                &self.result,
                self.absolute_deadline,
                "teardown waiter settlement",
            ),
            Err(TryLockError::Poisoned(_)) => {
                Err("teardown waiter settlement mutex poisoned".to_string())
            }
        }
    }

    fn has_retryable_result(&self) -> bool {
        self.result
            .try_lock()
            .ok()
            .and_then(|result| result.as_ref().map(|report| report.outcome()))
            .is_some_and(|outcome| outcome != TeardownOutcome::Closed)
    }

    async fn wait(&self) -> TeardownReport {
        let mut done = self.done.subscribe();
        loop {
            let report = match lock_mutex_until(
                &self.result,
                self.absolute_deadline,
                "teardown waiter async result lookup",
            ) {
                Ok(result) => result.clone(),
                // A poisoned or non-releasing result slot must not turn a
                // bounded waiter into a permanently pending future. Durable
                // attempt state remains available to a later exact retry.
                Err(_) => return self.fallback.clone(),
            };
            if let Some(report) = report {
                return report;
            }
            if *done.borrow_and_update() {
                // Settlement was signalled but its report was unavailable.
                // Waiting for another watch transition could hang forever.
                return self.fallback.clone();
            }
            let remaining = match checked_remaining_until(
                self.absolute_deadline,
                "teardown waiter notification",
            ) {
                Ok(remaining) => remaining,
                Err(_) => return self.fallback.clone(),
            };
            match tokio::time::timeout(remaining, done.changed()).await {
                Ok(Ok(())) => {}
                Ok(Err(_)) => {
                    let fallback = self.fallback.clone();
                    self.finish(fallback.clone());
                    return fallback;
                }
                Err(_) => return self.fallback.clone(),
            }
        }
    }

    fn wait_blocking(&self) -> Result<TeardownReport, String> {
        let deadline = self.absolute_deadline;
        let mut result = lock_mutex_until(&self.result, deadline, "teardown waiter result")?;
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

    pub(crate) fn wait_blocking(&self) -> Result<TeardownReport, String> {
        self.cell.wait_blocking()
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
    budgets: TeardownBudgets,
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
    absolute_deadline: Instant,
}

impl TeardownExecutor {
    fn new(worker_capacity: usize, queue_capacity: usize) -> Arc<Self> {
        let worker_capacity = worker_capacity.clamp(1, MAX_EXECUTOR_WORKER_CAPACITY);
        let queue_capacity = queue_capacity.clamp(1, DEFAULT_EXECUTOR_QUEUE_CAPACITY);
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
        self.shutdown_until(fail_closed_shutdown_deadline());
    }

    fn shutdown_until(&self, absolute_deadline: Instant) {
        let work = {
            let Ok(mut state) = lock_mutex_until(
                &self.keepalive.inner.state,
                absolute_deadline,
                "teardown executor shutdown",
            ) else {
                std::process::abort();
            };
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
            self.keepalive.join_workers_until(absolute_deadline);
            return;
        };
        for work in queued {
            cancel_queued_cleanup(work, absolute_deadline);
        }
        for execution in active {
            execution.cancellation.request();
            execution.cell.finish(waiter_failure_report(
                execution.ticket.clone(),
                "teardown coordinator dropped while cleanup was active; cancellation requested",
            ));
            let Ok(mut state) = lock_mutex_until(
                &execution.coordinator_state,
                absolute_deadline,
                "active teardown cancellation settlement",
            ) else {
                std::process::abort();
            };
            state.active.retain(|entry| entry.key != execution.key);
        }

        // Waiters are settled above, but the worker may still be inside an
        // effect or persistence adapter. Join the fixed executor workers so
        // no cleanup can mutate state after shutdown returns.
        self.keepalive.join_workers_until(absolute_deadline);
    }

    fn is_closed_until(&self, absolute_deadline: Instant) -> Result<bool, String> {
        Ok(lock_mutex_until(
            &self.keepalive.inner.state,
            absolute_deadline,
            "executor closed-state lookup",
        )?
        .closed)
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
                absolute_deadline,
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
        let mut state = lock_mutex_until(
            &self.keepalive.inner.state,
            absolute_deadline,
            "executor capacity reservation",
        )
        .map_err(|detail| TeardownReject::CleanupFailed {
            retained: TeardownRetention::new(Vec::new(), Vec::new(), detail),
        })?;
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
                    absolute_deadline,
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
                .map_err(|_| TeardownReject::CleanupFailed {
                    retained: TeardownRetention::new(
                        Vec::new(),
                        Vec::new(),
                        "executor capacity reservation mutex poisoned",
                    ),
                })?;
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
        self.join_workers_until(fail_closed_shutdown_deadline());
    }
}

impl TeardownExecutorKeepalive {
    fn join_workers_until(&self, absolute_deadline: Instant) {
        let current = thread::current().id();
        let Ok(mut workers) = lock_mutex_until(
            &self.workers,
            absolute_deadline,
            "teardown worker join ownership",
        ) else {
            std::process::abort();
        };
        let handles = std::mem::take(&mut *workers);
        drop(workers);
        while handles.iter().any(|handle| !handle.is_finished())
            && checked_remaining_until(absolute_deadline, "teardown worker join").is_ok()
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

fn cancel_queued_cleanup(work: CleanupWork, absolute_deadline: Instant) {
    let CleanupWork {
        ticket,
        key,
        cell,
        cancellation,
        state,
        completion_store,
        ..
    } = work;
    cancellation.request();
    let mut report = waiter_failure_report(
        ticket,
        "teardown executor shut down before cleanup started; cancellation requested",
    );
    if let Err(detail) = completion_store.record_attempt(
        &key,
        TeardownAttemptState::RetryableFailure(report.clone()),
        absolute_deadline,
    ) {
        // CleanupWork is constructible only after `begin_attempt[_batch]`
        // durably commits InProgress. If the tighter cancellation deadline no
        // longer permits upgrading that row, retain the replayable InProgress
        // admission and make the failed upgrade visible to this waiter.
        push_bounded_error(
            &mut report.errors,
            format!(
                "queued teardown cancellation journal update failed; durable InProgress admission retained for retry: {detail}"
            ),
        );
    }
    cell.finish(report);
    let state_guard = if Instant::now() < absolute_deadline {
        lock_mutex_until(
            &state,
            absolute_deadline,
            "queued teardown cancellation settlement",
        )
    } else {
        // Do not mint a later timeout after the request's checked absolute
        // deadline. Ownership rollback may proceed only if it is immediately
        // available; otherwise fail closed rather than detach a stale waiter.
        match state.try_lock() {
            Ok(state) => Ok(state),
            Err(TryLockError::WouldBlock | TryLockError::Poisoned(_)) => {
                Err("queued teardown cancellation settlement unavailable".to_string())
            }
        }
    };
    if let Ok(mut state) = state_guard {
        state.active.retain(|active| active.key != key);
    }
    // If the original authority deadline expired while another short
    // coordinator-state transition held the lock, the settled retryable cell
    // remains safe and inert. The next exact request lazily removes it before
    // consulting durable attempt state; never extend effect authority merely
    // to win an in-memory bookkeeping race.
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
        let mut state =
            match lock_mutex_until(&self.inner.state, absolute_deadline, "executor submission") {
                Ok(state) => state,
                Err(detail) if detail.contains("exceeded teardown absolute deadline") => {
                    return Err(ExecutorSubmitError::Timeout(works));
                }
                Err(detail) => return Err(ExecutorSubmitError::ClockFailure(works, detail)),
            };
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
            let (next_state, _) = match self.inner.changed.wait_timeout(state, remaining) {
                Ok(wait) => wait,
                Err(_) => {
                    return Err(ExecutorSubmitError::ClockFailure(
                        works,
                        "executor submission mutex poisoned".to_string(),
                    ));
                }
            };
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
        let state_guard = if Instant::now() < self.absolute_deadline {
            lock_mutex_until(
                &self.inner.state,
                self.absolute_deadline,
                "executor reservation rollback",
            )
        } else {
            match self.inner.state.try_lock() {
                Ok(state) => Ok(state),
                Err(TryLockError::WouldBlock | TryLockError::Poisoned(_)) => {
                    Err("executor reservation rollback unavailable".to_string())
                }
            }
        };
        let Ok(mut state) = state_guard else {
            // Leaking occupied capacity would make later teardown requests
            // silently unavailable. A bounded fail-closed stop is safer than
            // returning with unowned capacity or waiting forever.
            std::process::abort();
        };
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
        let configured_capacity = configured_capacity.clamp(1, MAX_EXECUTOR_WORKER_CAPACITY);
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
        let absolute_deadline = fail_closed_shutdown_deadline();
        let Ok(_admission_serial) = lock_mutex_until(
            &self.admission_serial,
            absolute_deadline,
            "teardown shutdown admission serialization",
        ) else {
            // Returning while an admission transition can still publish work
            // would violate shutdown linearizability. Never detach it.
            std::process::abort();
        };
        self.executor.shutdown_until(absolute_deadline);
    }

    pub fn configured_capacity(&self) -> usize {
        self.configured_capacity
    }

    pub fn active_operation_count(&self) -> usize {
        let mut state = self
            .state
            .lock()
            .expect("teardown coordinator state mutex poisoned");
        state
            .active
            .retain(|entry| !entry.cell.has_retryable_result());
        state.active.len()
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
        let _admission_serial = lock_mutex_until(
            &self.admission_serial,
            absolute_deadline,
            "teardown admission serialization",
        )
        .map_err(|detail| TeardownReject::CleanupFailed {
            retained: TeardownRetention::new(vec![ticket.clone()], Vec::new(), detail),
        })?;
        if self.shutdown_started.load(Ordering::Acquire) {
            return Err(TeardownReject::ExecutorClosed);
        }
        let key = completion_key(&ticket);
        if let Some(waiter) = self.find_existing_waiter(&key, deadline)? {
            return Ok(waiter);
        }
        match self.executor.is_closed_until(absolute_deadline) {
            Ok(true) => return Err(TeardownReject::ExecutorClosed),
            Ok(false) => {}
            Err(detail) => {
                return Err(TeardownReject::CleanupFailed {
                    retained: TeardownRetention::new(vec![ticket], Vec::new(), detail),
                });
            }
        }
        if let Some(waiter) = self.lookup_completed(&ticket, &key, deadline)? {
            return Ok(waiter);
        }
        if let Some(TeardownAttemptState::EffectsClosed(report)) =
            self.lookup_attempt(&key, deadline)?
        {
            return self.finalize_effects_closed_attempt(&key, report, deadline);
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

        if let Err(detail) = self.completion_store.begin_attempt(&key, absolute_deadline) {
            return Err(self.rollback_rejection(
                &rollback_tickets,
                &rollback_receipts,
                deadline,
                TeardownReject::CleanupFailed {
                    retained: TeardownRetention::new(
                        vec![ticket],
                        receipts,
                        format!("teardown attempt persistence failed: {detail}"),
                    ),
                },
            ));
        }

        let cell = Arc::new(CleanupCell::new(&ticket, absolute_deadline));
        if let Err(detail) = self.insert_active(
            vec![ActiveCleanup {
                key: key.clone(),
                cell: Arc::clone(&cell),
            }],
            deadline,
        ) {
            return Err(self.rollback_rejection(
                &rollback_tickets,
                &rollback_receipts,
                deadline,
                TeardownReject::CleanupFailed {
                    retained: TeardownRetention::new(
                        vec![ticket],
                        rollback_receipts.clone(),
                        detail,
                    ),
                },
            ));
        }
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
                cancel_queued_cleanup(work, absolute_deadline);
            }
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
        let deadline = self.cleanup_deadline()?;
        let _admission_serial = lock_mutex_until(
            &self.admission_serial,
            deadline.absolute,
            "teardown join admission serialization",
        )
        .map_err(|detail| TeardownReject::CleanupFailed {
            retained: TeardownRetention::new(Vec::new(), Vec::new(), detail),
        })?;
        if self.shutdown_started.load(Ordering::Acquire) {
            return Err(TeardownReject::ExecutorClosed);
        }
        let key = TeardownCompletionKey::new(action_epoch, fence.clone());
        if let Some(waiter) = self.find_existing_waiter(&key, deadline)? {
            return Ok(waiter);
        }
        if let Some(waiter) = self.lookup_completed_by_key(&key, deadline)? {
            return Ok(waiter);
        }
        if let Some(TeardownAttemptState::EffectsClosed(report)) =
            self.lookup_attempt(&key, deadline)?
        {
            return self.finalize_effects_closed_attempt(&key, report, deadline);
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
        let _admission_serial = lock_mutex_until(
            &self.admission_serial,
            absolute_deadline,
            "teardown batch admission serialization",
        )
        .map_err(|detail| TeardownReject::CleanupFailed {
            retained: TeardownRetention::new(tickets.clone(), Vec::new(), detail),
        })?;
        if self.shutdown_started.load(Ordering::Acquire) {
            return Err(TeardownReject::ExecutorClosed);
        }
        let mut waiters = Vec::with_capacity(tickets.len());
        let mut fresh = Vec::new();
        let mut fresh_duplicates = Vec::new();
        match self.executor.is_closed_until(absolute_deadline) {
            Ok(true) => return Err(TeardownReject::ExecutorClosed),
            Ok(false) => {}
            Err(detail) => {
                return Err(TeardownReject::CleanupFailed {
                    retained: TeardownRetention::new(tickets, Vec::new(), detail),
                });
            }
        }
        for ticket in tickets {
            let key = completion_key(&ticket);
            if let Some(waiter) = self.find_existing_waiter(&key, deadline)? {
                waiters.push(waiter);
            } else if let Some(waiter) = self.lookup_completed(&ticket, &key, deadline)? {
                waiters.push(waiter);
            } else if let Some(TeardownAttemptState::EffectsClosed(report)) =
                self.lookup_attempt(&key, deadline)?
            {
                waiters.push(self.finalize_effects_closed_attempt(&key, report, deadline)?);
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

        let fresh_keys: Vec<TeardownCompletionKey> =
            fresh.iter().map(|(key, _)| key.clone()).collect();
        if let Err(detail) = self
            .completion_store
            .begin_attempt_batch(&fresh_keys, absolute_deadline)
        {
            return Err(self.rollback_rejection(
                &fresh_tickets,
                &rollback_receipts,
                deadline,
                TeardownReject::CleanupFailed {
                    retained: TeardownRetention::new(
                        fresh_tickets.clone(),
                        rollback_receipts.clone(),
                        format!("teardown attempt persistence failed: {detail}"),
                    ),
                },
            ));
        }

        let mut works = Vec::with_capacity(fresh.len());
        let mut created = Vec::with_capacity(fresh.len());
        let mut active_entries = Vec::with_capacity(fresh.len());
        for (key, ticket) in fresh {
            let cell = Arc::new(CleanupCell::new(&ticket, absolute_deadline));
            active_entries.push(ActiveCleanup {
                key: key.clone(),
                cell: Arc::clone(&cell),
            });
            waiters.push(TeardownWaiter {
                cell: Arc::clone(&cell),
            });
            created.push((key.clone(), Arc::clone(&cell)));
            works.push(self.cleanup_work(ticket, key, cell, deadline));
        }
        if let Err(detail) = self.insert_active(active_entries, deadline) {
            return Err(self.rollback_rejection(
                &fresh_tickets,
                &rollback_receipts,
                deadline,
                TeardownReject::CleanupFailed {
                    retained: TeardownRetention::new(
                        fresh_tickets.clone(),
                        rollback_receipts.clone(),
                        detail,
                    ),
                },
            ));
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
                cancel_queued_cleanup(work, absolute_deadline);
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
            budgets: self.budgets,
            deadline,
            state: Arc::clone(&self.state),
            completion_store: self.completion_store.clone(),
            completed_operation_capacity: self.completed_operation_capacity,
            cancellation: CancellationToken::new(),
        }
    }

    fn find_existing_waiter(
        &self,
        key: &TeardownCompletionKey,
        deadline: CleanupDeadline,
    ) -> Result<Option<TeardownWaiter>, TeardownReject> {
        let mut state = lock_mutex_until(&self.state, deadline.absolute, "teardown waiter lookup")
            .map_err(|detail| TeardownReject::CompletionLookupFailed { detail })?;
        if let Some(index) = state
            .active
            .iter()
            .position(|existing| existing.key == *key)
        {
            if state.active[index].cell.has_retryable_result() {
                state.active.remove(index);
            } else {
                return Ok(Some(TeardownWaiter {
                    cell: Arc::clone(&state.active[index].cell),
                }));
            }
        }
        Ok(state
            .completed
            .iter()
            .find(|existing| existing.key == *key)
            .map(|existing| TeardownWaiter {
                cell: Arc::clone(&existing.cell),
            }))
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
                let cell = Arc::new(CleanupCell::new(&report.ticket, deadline.absolute));
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
                let cell = Arc::new(CleanupCell::new(ticket, deadline.absolute));
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
        let boundary_deadline = deadline
            .boundary_deadline("completion lookup")
            .map_err(|detail| TeardownReject::CompletionLookupFailed { detail })?;
        let store = self.completion_store.clone();
        let lookup_key = key.clone();
        let report = std::panic::catch_unwind(AssertUnwindSafe(|| {
            store.lookup(&lookup_key, boundary_deadline)
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

    fn lookup_attempt(
        &self,
        key: &TeardownCompletionKey,
        deadline: CleanupDeadline,
    ) -> Result<Option<TeardownAttemptState>, TeardownReject> {
        deadline
            .check("teardown attempt lookup")
            .map_err(|detail| TeardownReject::CompletionLookupFailed { detail })?;
        let boundary_deadline = deadline
            .boundary_deadline("teardown attempt lookup")
            .map_err(|detail| TeardownReject::CompletionLookupFailed { detail })?;
        let attempt = self
            .completion_store
            .lookup_attempt(key, boundary_deadline)
            .map_err(|detail| TeardownReject::CompletionLookupFailed { detail })?;
        deadline
            .check("teardown attempt lookup")
            .map_err(|detail| TeardownReject::CompletionLookupFailed { detail })?;
        Ok(attempt)
    }

    fn finalize_effects_closed_attempt(
        &self,
        key: &TeardownCompletionKey,
        report: TeardownReport,
        deadline: CleanupDeadline,
    ) -> Result<TeardownWaiter, TeardownReject> {
        self.completion_store
            .persist(key, &report, deadline.absolute)
            .map_err(|detail| TeardownReject::CompletionLookupFailed { detail })?;
        deadline
            .check("effects-closed teardown handoff")
            .map_err(|detail| TeardownReject::CompletionLookupFailed { detail })?;
        let cell = Arc::new(CleanupCell::new(&report.ticket, deadline.absolute));
        cell.finish(report);
        Ok(TeardownWaiter { cell })
    }

    fn insert_active(
        &self,
        entries: Vec<ActiveCleanup>,
        deadline: CleanupDeadline,
    ) -> Result<(), String> {
        let keys: Vec<TeardownCompletionKey> =
            entries.iter().map(|entry| entry.key.clone()).collect();
        let mut state = lock_mutex_until(
            &self.state,
            deadline.absolute,
            "teardown active-state publication",
        )?;
        state.active.extend(entries);
        if let Err(detail) = deadline.check("teardown active-state publication") {
            state
                .active
                .retain(|active| !keys.iter().any(|key| *key == active.key));
            return Err(detail);
        }
        Ok(())
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
        budgets,
        deadline,
        state,
        completion_store,
        completed_operation_capacity,
    } = work;
    let fallback_ticket = ticket.clone();
    let report = match AssertUnwindSafe(execute_cleanup(
        ticket,
        effects,
        budgets,
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
            remove_active_cleanup(&state, &key, deadline.absolute);
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
    let attempt = if report.outcome() == TeardownOutcome::Closed {
        TeardownAttemptState::EffectsClosed(report.clone())
    } else {
        TeardownAttemptState::RetryableFailure(report.clone())
    };
    if let Err(detail) = completion_store.record_attempt(&key, attempt, absolute_deadline) {
        let report = report.with_handoff_error(format!(
            "teardown attempt handoff failed: {}",
            sanitize_text(&detail)
        ));
        remove_active_cleanup(state, &key, absolute_deadline);
        return report;
    }
    if report.outcome() != TeardownOutcome::Closed {
        remove_active_cleanup(state, &key, absolute_deadline);
        return report;
    }
    if let Err(detail) =
        persist_completion(completion_store, &key, &report, absolute_deadline).await
    {
        let report = report.with_handoff_error(format!(
            "completed teardown handoff failed: {}",
            sanitize_text(&detail)
        ));
        remove_active_cleanup(state, &key, absolute_deadline);
        return report;
    }

    match retain_completed_cleanup(
        state,
        completed_operation_capacity,
        key,
        cell,
        absolute_deadline,
    ) {
        Ok(()) => report,
        Err(detail) => report.with_handoff_error(format!(
            "completed teardown cache handoff failed: {}",
            sanitize_text(&detail)
        )),
    }
}

fn remove_active_cleanup(
    state: &Arc<Mutex<CoordinatorState>>,
    key: &TeardownCompletionKey,
    absolute_deadline: Instant,
) {
    let state_guard = if Instant::now() < absolute_deadline {
        lock_mutex_until(state, absolute_deadline, "teardown active-state release")
    } else {
        match state.try_lock() {
            Ok(state) => Ok(state),
            Err(TryLockError::WouldBlock | TryLockError::Poisoned(_)) => {
                Err("teardown active-state release unavailable".to_string())
            }
        }
    };
    if let Ok(mut state) = state_guard {
        state.active.retain(|active| active.key != *key);
    }
    // A missed removal after the exact deadline cannot authorize any further
    // effect. `CleanupCell::finish` makes the entry inert, and admission
    // prunes retryable settled cells before it can return a cached waiter.
}

fn retain_completed_cleanup(
    state: &Arc<Mutex<CoordinatorState>>,
    completed_operation_capacity: usize,
    key: TeardownCompletionKey,
    cell: Arc<CleanupCell>,
    absolute_deadline: Instant,
) -> Result<(), String> {
    let mut state = lock_mutex_until(
        state,
        absolute_deadline,
        "teardown completed-state retention",
    )?;
    state.active.retain(|active| active.key != key);
    if state.completed.iter().any(|completed| completed.key == key) {
        return Ok(());
    }
    if state.completed.len() >= completed_operation_capacity {
        state.completed.pop_front();
    }
    state.completed.push_back(CompletedCleanup {
        key: key.clone(),
        cell,
    });
    if let Err(detail) =
        checked_remaining_until(absolute_deadline, "teardown completed-state retention")
    {
        state.completed.retain(|completed| completed.key != key);
        return Err(detail);
    }
    Ok(())
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

impl EffectCall {
    fn poll_budget(self, deadline: CleanupDeadline, remaining: Duration) -> Duration {
        match self {
            Self::Drain
            | Self::CooperativeClose
            | Self::InterruptOrSafeClose
            | Self::TerminateTree => remaining.min(deadline.effect_budget),
            Self::SettleActiveProcessZero
            | Self::DetachAfterZero
            | Self::ReconcilePorts
            | Self::PersistSettlement
            | Self::ReleaseStoppedExact => remaining,
        }
    }
}

async fn execute_cleanup(
    ticket: TeardownTicket,
    effects: Arc<dyn TeardownEffects>,
    budgets: TeardownBudgets,
    deadline: CleanupDeadline,
    cancellation: Arc<CancellationToken>,
) -> TeardownReport {
    let mut attempted_stages = Vec::new();
    let mut stage_notes = Vec::new();
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
        &mut stage_notes,
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
        &mut stage_notes,
        &mut errors,
        TeardownStage::CooperativeClose,
    );

    attempted_stages.push(TeardownStage::CooperativeWait);
    let cooperative = bounded_wait(
        TeardownStage::CooperativeWait,
        deadline,
        Arc::clone(&effects),
        budgets.wait_budget(WaitStage::CooperativeGrace),
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
            stage_notes,
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
        &mut stage_notes,
        &mut errors,
        TeardownStage::InterruptOrSafeClose,
    );

    attempted_stages.push(TeardownStage::InterruptWait);
    let interrupted = bounded_wait(
        TeardownStage::InterruptWait,
        deadline,
        Arc::clone(&effects),
        budgets.wait_budget(WaitStage::InterruptGrace),
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
            stage_notes,
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
        &mut stage_notes,
        &mut errors,
        TeardownStage::TerminateTree,
    );

    attempted_stages.push(TeardownStage::TerminationWait);
    let terminated = bounded_wait(
        TeardownStage::TerminationWait,
        deadline,
        Arc::clone(&effects),
        budgets.wait_budget(WaitStage::Termination),
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
            stage_notes,
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
        stage_notes,
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
    if let Err(detail) = deadline.check(&format!("{stage:?}")) {
        return StageResult::Failed { detail };
    }
    let future = match std::panic::catch_unwind(AssertUnwindSafe(|| {
        stage_future(effects.as_ref(), ticket, call, deadline)
    })) {
        Ok(Ok(future)) => future,
        Ok(Err(detail)) => return StageResult::Failed { detail },
        Err(payload) => {
            return StageResult::Failed {
                detail: format!("{stage:?} panicked: {}", panic_detail(payload)),
            };
        }
    };
    let remaining = match deadline.check(&format!("{stage:?}")) {
        Ok(remaining) => call.poll_budget(deadline, remaining),
        Err(detail) => {
            return StageResult::Failed {
                detail: format!("{stage:?} timeout during effect construction: {detail}"),
            };
        }
    };
    if remaining.is_zero() {
        return StageResult::Failed {
            detail: format!("{stage:?} timeout before effect settlement"),
        };
    }
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
    wait_budget: Duration,
    ticket: &TeardownTicket,
    wait_stage: WaitStage,
    cancellation: Arc<CancellationToken>,
) -> WaitResult {
    if cancellation.is_requested() {
        return WaitResult::Failed {
            detail: format!("{stage:?}: teardown cleanup cancellation requested"),
        };
    }
    let absolute_remaining = match deadline.check(&format!("{stage:?}")) {
        Ok(remaining) => remaining,
        Err(detail) => return WaitResult::Failed { detail },
    };
    let stage_budget = absolute_remaining.min(wait_budget);
    if stage_budget.is_zero() {
        return WaitResult::TimedOut;
    }
    if let Err(detail) = deadline.check(&format!("{stage:?}")) {
        return WaitResult::Failed { detail };
    }
    // Derive a strictly narrower stage boundary from the coordinator's one
    // authoritative absolute deadline.  A compliant host adapter returns
    // `TimedOut` at this boundary so ordinary escalation is not reported as a
    // cleanup failure.  The outer timer remains an independent cancellation
    // boundary: a non-returning adapter is still reported as failed.
    let stage_absolute = match Instant::now().checked_add(stage_budget) {
        Some(candidate) => candidate.min(deadline.absolute),
        None => {
            return WaitResult::Failed {
                detail: format!("{stage:?} deadline overflow"),
            }
        }
    };
    let stage_deadline = CleanupDeadline {
        absolute: stage_absolute,
        effect_budget: deadline.effect_budget,
    };
    let future = match std::panic::catch_unwind(AssertUnwindSafe(|| {
        effects.wait_for_zero(ticket, wait_stage, stage_deadline)
    })) {
        Ok(future) => future,
        Err(payload) => {
            return WaitResult::Failed {
                detail: format!("{stage:?} panicked: {}", panic_detail(payload)),
            };
        }
    };
    if let Err(detail) = stage_deadline.check(&format!("{stage:?}")) {
        return WaitResult::Failed {
            detail: format!("{stage:?} adapter timeout during effect construction: {detail}"),
        };
    }
    let stage_timer = tokio::time::sleep_until(tokio::time::Instant::from_std(stage_absolute));
    tokio::pin!(stage_timer);
    tokio::select! {
        biased;
        _ = cancellation.cancelled() => WaitResult::Failed {
            detail: format!("{stage:?}: teardown cleanup cancellation requested"),
        },
        result = AssertUnwindSafe(future).catch_unwind() => {
            match result {
                Ok(result) => match deadline.check(&format!("{stage:?}")) {
                    Ok(_) => result,
                    Err(detail) => WaitResult::Failed { detail },
                },
                Err(payload) => WaitResult::Failed {
                    detail: format!("{stage:?} panicked: {}", panic_detail(payload)),
                },
            }
        },
        _ = &mut stage_timer => WaitResult::Failed {
            detail: format!("{stage:?} adapter timeout before its bounded stage deadline"),
        },
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
    if let Err(detail) = deadline.check("Residue") {
        push_bounded_error(errors, format!("Residue: {detail}"));
        return None;
    }
    let future =
        match std::panic::catch_unwind(AssertUnwindSafe(|| effects.residue(ticket, deadline))) {
            Ok(future) => future,
            Err(payload) => {
                push_bounded_error(
                    errors,
                    format!("Residue panicked: {}", panic_detail(payload)),
                );
                return None;
            }
        };
    let remaining = match deadline.check("Residue") {
        Ok(remaining) => remaining.min(deadline.effect_budget),
        Err(detail) => {
            push_bounded_error(
                errors,
                format!("Residue timeout during effect construction: {detail}"),
            );
            return None;
        }
    };
    if remaining.is_zero() {
        push_bounded_error(
            errors,
            "Residue timeout before effect settlement".to_string(),
        );
        return None;
    }
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
    deadline: CleanupDeadline,
) -> Result<BoxFuture<'a, StageResult>, String> {
    Ok(match call {
        EffectCall::Drain => effects.drain(ticket, deadline),
        EffectCall::CooperativeClose => effects.cooperative_close(ticket, deadline),
        EffectCall::InterruptOrSafeClose => effects.interrupt_or_safe_close(ticket, deadline),
        EffectCall::TerminateTree => effects.terminate_tree(ticket, deadline),
        EffectCall::SettleActiveProcessZero => effects.settle_active_process_zero(ticket, deadline),
        EffectCall::DetachAfterZero => effects.detach_after_zero(ticket, deadline),
        EffectCall::ReconcilePorts => effects.reconcile_ports(ticket, deadline),
        EffectCall::PersistSettlement => effects.persist_settlement(ticket, deadline),
        EffectCall::ReleaseStoppedExact => effects.release_stopped_exact(ticket, deadline),
    })
}

fn collect_stage_result(
    result: StageResult,
    stage_notes: &mut Vec<String>,
    errors: &mut Vec<String>,
    stage: TeardownStage,
) {
    match result {
        StageResult::Completed => {}
        StageResult::Unsupported { detail } => {
            if stage_notes.len() < MAX_TEARDOWN_STAGE_NOTES {
                stage_notes.push(format!("{stage:?} unsupported: {}", sanitize_text(&detail)));
            }
        }
        StageResult::Failed { detail } => {
            push_bounded_error(errors, format!("{stage:?}: {}", sanitize_text(&detail)));
        }
    }
}

fn push_bounded_error(errors: &mut Vec<String>, detail: String) {
    if errors.len() < MAX_TEARDOWN_ERRORS {
        errors.push(detail);
    }
}

fn required_stage_failure(result: StageResult, stage: TeardownStage) -> Option<String> {
    match result {
        StageResult::Completed => None,
        StageResult::Unsupported { detail } => {
            Some(format!("{stage:?} unsupported: {}", sanitize_text(&detail)))
        }
        StageResult::Failed { detail } => Some(format!("{stage:?}: {}", sanitize_text(&detail))),
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
                StageResult::Unsupported { detail } => {
                    push_bounded_error(
                        errors,
                        format!(
                            "SettleActiveProcessZero unsupported: {}",
                            sanitize_text(&detail)
                        ),
                    );
                    false
                }
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
    stage_notes: Vec<String>,
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
    if let Some(detail) = required_stage_failure(detach, TeardownStage::DetachAfterZero) {
        push_bounded_error(&mut errors, detail);
        return failed_post_zero_report(
            ticket,
            effects,
            deadline,
            attempted_stages,
            stage_notes,
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
    if let Some(detail) = required_stage_failure(release, TeardownStage::ReleaseStoppedExact) {
        push_bounded_error(&mut errors, detail);
        return failed_post_zero_report(
            ticket,
            effects,
            deadline,
            attempted_stages,
            stage_notes,
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
    if let Some(detail) = required_stage_failure(reconcile, TeardownStage::ReconcilePorts) {
        push_bounded_error(&mut errors, detail);
        return failed_post_zero_report(
            ticket,
            effects,
            deadline,
            attempted_stages,
            stage_notes,
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
    let outcome = match persist {
        StageResult::Completed if errors.is_empty() => TeardownOutcome::Closed,
        StageResult::Completed => TeardownOutcome::CleanupFailed,
        StageResult::Unsupported { detail } => {
            push_bounded_error(
                &mut errors,
                format!("PersistSettlement unsupported: {}", sanitize_text(&detail)),
            );
            TeardownOutcome::CleanupFailed
        }
        StageResult::Failed { detail } => {
            push_bounded_error(
                &mut errors,
                format!("PersistSettlement: {}", sanitize_text(&detail)),
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
        stage_notes,
        errors,
        residue,
    }
}

async fn failed_post_zero_report(
    ticket: TeardownTicket,
    effects: Arc<dyn TeardownEffects>,
    deadline: CleanupDeadline,
    attempted_stages: Vec<TeardownStage>,
    stage_notes: Vec<String>,
    mut errors: Vec<String>,
    cancellation: Arc<CancellationToken>,
) -> TeardownReport {
    // Before ReleaseStoppedExact, the owning terminal retains its exact Job
    // and receiver authority. After release, the adapter retains the exact
    // fence plus idempotent post-release reconciliation/persistence state so
    // a retry never remints or redirects termination authority.
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
        stage_notes,
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
        stage_notes: Vec::new(),
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
    use std::path::PathBuf;
    use std::time::{Duration, Instant};

    use super::{
        checked_remaining_until, duration_from_nanos, sanitize_text,
        teardown_host_path_within_bound, TeardownCompletionKey, TeardownCompletionStore,
        TeardownExecutor, TeardownReject, MAX_EVIDENCE_TEXT_BYTES,
    };

    fn completion_key_for_test(tail: u8, executable: PathBuf) -> TeardownCompletionKey {
        let mut bytes = [0_u8; 16];
        bytes[0] = 0x01;
        bytes[6] = 0x70;
        bytes[8] = 0x80;
        bytes[15] = tail;
        let resource_id = crate::domain::id::ResourceId::from_bytes(bytes).expect("resource id");
        let identity = crate::process::identity::ManagedProcessIdentity::new(
            crate::process::identity::ManagedProcessId::new(
                40_000 + u32::from(tail),
                400_000 + u64::from(tail),
            )
            .expect("managed process id"),
            executable,
        )
        .expect("managed process identity");
        let fence = crate::process::registry::ManagedProcessFence::new(
            super::ResourceFence::new(resource_id, 1),
            crate::process::identity::ProcessOwner::Host,
            identity,
        );
        TeardownCompletionKey::new(1, fence)
    }

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
    fn monotonic_deadline_conversion_rejects_unrepresentable_nanoseconds() {
        let error = duration_from_nanos(u128::MAX)
            .expect_err("deadline conversion must never saturate or truncate");
        assert!(error.contains("Duration"));
    }

    #[test]
    fn teardown_host_path_is_rejected_before_durable_string_allocation() {
        let oversized =
            std::path::PathBuf::from("x".repeat(super::MAX_TEARDOWN_HOST_STRING_BYTES + 1));
        assert!(!teardown_host_path_within_bound(&oversized));
        assert!(matches!(
            TeardownCompletionStore::durable(&oversized),
            Err(detail) if detail.contains("host string bound")
        ));
    }

    #[test]
    fn completion_store_mutex_contention_obeys_the_absolute_deadline() {
        let store = std::sync::Mutex::new(());
        let _held = store.lock().expect("hold report store");
        let error = super::lock_mutex_until(
            &store,
            Instant::now() + Duration::from_millis(5),
            "completion store test lock",
        )
        .expect_err("contended report store must not block indefinitely");
        assert!(error.contains("deadline"));
    }

    #[test]
    fn signalled_waiter_without_observable_report_fails_closed_without_hanging() {
        let key =
            completion_key_for_test(9, std::env::current_exe().expect("current test executable"));
        let ticket = super::TeardownTicket::new(
            crate::domain::id::OperationId::new(),
            super::TeardownScope::Host,
            key.action_epoch,
            key.fence.clone(),
        )
        .expect("exact waiter ticket");
        let cell = super::CleanupCell::new(&ticket, Instant::now() + Duration::from_secs(1));

        // Model a terminal watch transition whose report slot was lost or
        // poisoned. The waiter must not await a second transition forever.
        cell.done.send_replace(true);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("waiter test runtime");
        let report = runtime.block_on(async {
            tokio::time::timeout(Duration::from_millis(50), cell.wait())
                .await
                .expect("signalled waiter must resolve within its bound")
        });
        assert_eq!(report.outcome(), super::TeardownOutcome::CleanupFailed);
    }

    #[test]
    fn unsignalled_async_waiter_obeys_its_original_absolute_deadline() {
        let key = completion_key_for_test(
            10,
            std::env::current_exe().expect("current test executable"),
        );
        let ticket = super::TeardownTicket::new(
            crate::domain::id::OperationId::new(),
            super::TeardownScope::Host,
            key.action_epoch,
            key.fence.clone(),
        )
        .expect("exact waiter ticket");
        let cell = super::CleanupCell::new(&ticket, Instant::now() + Duration::from_millis(10));
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("waiter test runtime");
        let started = Instant::now();
        let report = runtime.block_on(cell.wait());

        assert_eq!(report.outcome(), super::TeardownOutcome::CleanupFailed);
        assert!(
            started.elapsed() < Duration::from_millis(100),
            "the waiter must not mint a later timeout"
        );
    }

    #[test]
    fn cleanup_settlement_lock_uses_the_original_operation_deadline() {
        let key = completion_key_for_test(
            11,
            std::env::current_exe().expect("current test executable"),
        );
        let ticket = super::TeardownTicket::new(
            crate::domain::id::OperationId::new(),
            super::TeardownScope::Host,
            key.action_epoch,
            key.fence.clone(),
        )
        .expect("exact waiter ticket");
        let cell = std::sync::Arc::new(super::CleanupCell::new(
            &ticket,
            Instant::now() + Duration::from_millis(15),
        ));
        let held = cell.result.lock().expect("hold settlement slot");
        let worker_cell = cell.clone();
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        let worker = std::thread::spawn(move || {
            let result = worker_cell.lock_result_for_finish().map(|_| ());
            let _ = tx.send(result);
        });

        let error = rx
            .recv_timeout(Duration::from_millis(100))
            .expect("settlement lock attempt must remain bounded")
            .expect_err("the original deadline must expire while the slot is held");
        assert!(error.contains("deadline"));
        drop(held);
        worker.join().expect("settlement lock worker");
    }

    #[test]
    fn durable_batch_attempt_admission_is_atomic_before_any_row_is_written() {
        let temp = tempfile::tempdir().expect("teardown attempt journal directory");
        let store = TeardownCompletionStore::durable(temp.path().join("attempts.sqlite3"))
            .expect("open teardown attempt journal");
        let executable = std::env::current_exe().expect("current test executable");
        let valid = completion_key_for_test(1, executable.clone());
        let second = completion_key_for_test(2, executable);
        let deadline = Instant::now() + Duration::from_secs(1);
        store.fail_begin_attempt_after_writes_for_test(1);

        let error = store
            .begin_attempt_batch(&[valid.clone(), second], deadline)
            .expect_err("the injected mid-batch failure must reject the whole admission");
        assert!(error.contains("injected"));
        assert!(
            store
                .lookup_attempt(&valid, deadline)
                .expect("lookup valid batch member")
                .is_none(),
            "the valid prefix must not be left InProgress"
        );
    }

    #[test]
    fn teardown_executor_normalizes_zero_worker_configuration() {
        let executor = TeardownExecutor::new(0, 1);
        assert_eq!(executor.inner().worker_capacity, 1);
        executor.shutdown();
    }

    #[test]
    fn teardown_executor_clamps_caller_controlled_worker_and_queue_capacity() {
        let executor = TeardownExecutor::new(
            super::MAX_EXECUTOR_WORKER_CAPACITY + 1,
            super::DEFAULT_EXECUTOR_QUEUE_CAPACITY + 1,
        );
        assert_eq!(
            executor.inner().worker_capacity,
            super::MAX_EXECUTOR_WORKER_CAPACITY
        );
        assert_eq!(
            executor.inner().queue_capacity,
            super::DEFAULT_EXECUTOR_QUEUE_CAPACITY
        );
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
