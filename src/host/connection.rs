//! Single host-owned CommandBus executor boundary.
//!
//! Transport connection tasks never mutate the bus or projections directly.
//! They submit decoded requests through [`HostRequestHandle`]; one
//! [`HostRequestExecutor`] task exclusively owns [`CommandBus`] and services
//! them in arrival order. The executor also owns the bounded SnapshotSession,
//! EventReplaySession, and ArtifactContentSession registries for paged snapshot,
//! event-replay, and artifact-content queries.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::pin::pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures_util::stream::{FuturesUnordered, StreamExt};
use tokio::sync::{mpsc, oneshot, watch, Notify, OwnedSemaphorePermit, Semaphore};
use tokio::task::JoinHandle;
use tokio::time::{interval_at, MissedTickBehavior};
use uuid::Uuid;

use crate::domain::command::{Command, CommandReceipt};
use crate::domain::event::DomainEvent;
use crate::domain::id::{ArtifactId, OperationId, SnapshotId, SubscriptionId, TaskId};
use crate::domain::query::{
    Query, QueryEnvelope, QueryError, QueryOutcome, QueryReply, QueryResult,
};
use crate::domain::snapshot::{PageLimits, SnapshotSection};
use crate::domain::ClientId;
use crate::kernel::{
    ArtifactContentError, ArtifactContentRegistry, CommandBus, EventReplaySession, ReplayError,
    SnapshotError, SnapshotSession, StoreError,
};
use crate::protocol::{
    Capability, CapabilitySet, ClientRequest, DetachAck, DetachRequest, NegotiatedParameters,
    ServerMessage, StreamFrame, StreamKey,
};

use super::ipc::IpcError;
use super::shutdown::{
    HostCleanupProgress, HostCleanupWorker, ProcessEmptyTeardown, ProcessEmptyTeardownWorker,
};

/// Fixed capacity for the host request queue.
///
/// When the queue is full, [`HostRequestHandle::execute`] awaits send capacity
/// (bounded backpressure). Requests are never silently dropped.
pub const HOST_REQUEST_QUEUE_CAPACITY: usize = 32;

/// Default durable event output lane capacity for one duplex connection.
pub(crate) const HOST_DURABLE_OUTPUT_QUEUE_CAPACITY: usize = 32;

/// Default ephemeral stream output lane capacity for one duplex connection.
pub(crate) const HOST_EPHEMERAL_OUTPUT_QUEUE_CAPACITY: usize = 32;

const MAX_SNAPSHOT_SESSIONS: usize = 32;
const SNAPSHOT_IDLE_TTL: Duration = Duration::from_secs(30);
const SNAPSHOT_REAPER_PERIOD: Duration = Duration::from_secs(1);

const MAX_EVENT_REPLAY_SESSIONS: usize = 32;
const EVENT_REPLAY_IDLE_TTL: Duration = Duration::from_secs(30);
const EVENT_REPLAY_REAPER_PERIOD: Duration = Duration::from_secs(1);

/// One absolute deadline for all quit-terminal high-water ack waits.
const QUIT_TERMINAL_ACK_TIMEOUT: Duration = Duration::from_secs(5);

/// Capacity-one supervisor arm request: drop the pending listener before ack.
#[derive(Debug)]
pub struct PhysicalExitArmRequest {
    pub operation_id: OperationId,
    pub action_epoch: u64,
    pub ack: oneshot::Sender<()>,
}

/// Typed intentional exit from a supervised [`HostRequestExecutor`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostExecutorOutcome {
    Intentional {
        operation_id: OperationId,
        action_epoch: u64,
    },
}

/// Supervised foreground executor: arm channel + join handle with typed outcome.
pub struct SupervisedHostExecutor {
    pub arm_rx: mpsc::Receiver<PhysicalExitArmRequest>,
    pub join: JoinHandle<Result<HostExecutorOutcome, StoreError>>,
}

/// Internal completion routing for one host request job.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HostRequestCompletionRouting {
    /// Caller (reader / one-shot serve) owns writing the response frame.
    CallerOwned,
    /// Executor may directly admit an accepted ConfirmHostQuit receipt.
    ExecutorOwnsAcceptedHostQuitReceipt,
}

/// Crate-private duplex execute completion: either the reader must write, or the
/// executor already admitted the quit receipt onto the critical lane.
#[derive(Debug)]
pub(crate) enum DuplexExecuteCompletion {
    CallerMustWrite(ServerMessage),
    ExecutorAdmittedQuitReceipt { operation_id: OperationId },
}

struct HostRequestJob {
    negotiated: NegotiatedParameters,
    request: ClientRequest,
    output_id: Option<ConnectionOutputId>,
    routing: HostRequestCompletionRouting,
    reply: oneshot::Sender<Result<DuplexExecuteCompletion, IpcError>>,
}

struct PendingQuitReceiptAck {
    operation_id: OperationId,
    ack: PhysicalWriteAck,
}

enum ExecutorControl {
    RegisterOutput {
        id: ConnectionOutputId,
        output: ConnectionOutputHandle,
        ack: oneshot::Sender<()>,
    },
    UnregisterOutput {
        id: ConnectionOutputId,
    },
    #[cfg(test)]
    InspectOutput {
        id: ConnectionOutputId,
        ack: oneshot::Sender<OutputInspection>,
    },
    #[cfg(test)]
    RunMaintenanceOnce {
        ack: oneshot::Sender<Result<(), StoreError>>,
    },
    #[cfg(test)]
    TakePendingQuitReceiptAck {
        id: ConnectionOutputId,
        ack: oneshot::Sender<Option<(OperationId, PhysicalWriteAck)>>,
    },
}

#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct OutputInspection {
    pub(crate) registered: bool,
    pub(crate) live_bound: bool,
}

struct SnapshotRegistryEntry {
    owner: ClientId,
    session: SnapshotSession,
    limits: PageLimits,
    last_touch: Instant,
}

struct SnapshotRegistry {
    entries: HashMap<SnapshotId, SnapshotRegistryEntry>,
}

impl SnapshotRegistry {
    fn new() -> Self {
        Self {
            entries: HashMap::new(),
        }
    }

    fn reap_idle(&mut self, now: Instant) {
        self.entries
            .retain(|_, entry| now.duration_since(entry.last_touch) < SNAPSHOT_IDLE_TTL);
    }

    fn evict_lru_if_at_capacity(&mut self) {
        while self.entries.len() >= MAX_SNAPSHOT_SESSIONS {
            let Some((&victim_id, _)) = self
                .entries
                .iter()
                .min_by_key(|(_, entry)| entry.last_touch)
            else {
                break;
            };
            self.entries.remove(&victim_id);
        }
    }

    fn insert(
        &mut self,
        owner: ClientId,
        session: SnapshotSession,
        limits: PageLimits,
        now: Instant,
    ) {
        self.evict_lru_if_at_capacity();
        let snapshot_id = session.snapshot_id();
        self.entries.insert(
            snapshot_id,
            SnapshotRegistryEntry {
                owner,
                session,
                limits,
                last_touch: now,
            },
        );
    }

    fn touch(&mut self, snapshot_id: SnapshotId, now: Instant) {
        if let Some(entry) = self.entries.get_mut(&snapshot_id) {
            entry.last_touch = now;
        }
    }

    fn remove(&mut self, snapshot_id: SnapshotId) -> Option<SnapshotRegistryEntry> {
        self.entries.remove(&snapshot_id)
    }

    fn get(
        &self,
        snapshot_id: SnapshotId,
        requester: ClientId,
        limits: PageLimits,
        now: Instant,
    ) -> Result<&SnapshotSession, QueryError> {
        let Some(entry) = self.entries.get(&snapshot_id) else {
            return Err(QueryError::NotFound);
        };
        if now.duration_since(entry.last_touch) >= SNAPSHOT_IDLE_TTL {
            return Err(QueryError::NotFound);
        }
        if entry.owner != requester {
            return Err(QueryError::Unauthorized);
        }
        if entry.limits != limits {
            return Err(QueryError::InvalidRequest);
        }
        Ok(&entry.session)
    }
}

pub(crate) struct LiveStreamState {
    /// Bumped on cancel/resync so already-queued durables that have not started
    /// writing are skipped. In-flight frames that complete a physical write still
    /// record their sequence.
    generation: AtomicU64,
    /// Conservative last sequence successfully written on the durable pipe.
    last_physically_written: AtomicU64,
    /// Persistent progress wakeups for quit durable high-water waits.
    progress: Notify,
}

impl LiveStreamState {
    pub(crate) fn new(baseline: u64) -> Arc<Self> {
        Arc::new(Self {
            generation: AtomicU64::new(1),
            last_physically_written: AtomicU64::new(baseline),
            progress: Notify::new(),
        })
    }

    pub(crate) fn current_generation(&self) -> u64 {
        self.generation.load(Ordering::SeqCst)
    }

    pub(crate) fn cancel(&self) {
        self.generation.fetch_add(1, Ordering::SeqCst);
    }

    pub(crate) fn last_physically_written(&self) -> u64 {
        self.last_physically_written.load(Ordering::SeqCst)
    }

    pub(crate) fn record_physical_write(&self, sequence: u64) {
        let mut current = self.last_physically_written.load(Ordering::SeqCst);
        while sequence > current {
            match self.last_physically_written.compare_exchange_weak(
                current,
                sequence,
                Ordering::SeqCst,
                Ordering::SeqCst,
            ) {
                Ok(_) => {
                    // Notify only when the atomic high-water actually advances.
                    self.progress.notify_waiters();
                    return;
                }
                Err(observed) => current = observed,
            }
        }
    }

    /// Wait until [`Self::last_physically_written`] is at least `target`.
    ///
    /// Uses `Notified::enable` plus a recheck so a notify between the atomic
    /// load and the await cannot be lost.
    pub(crate) async fn wait_until_physically_written(&self, target: u64) {
        loop {
            if self.last_physically_written() >= target {
                return;
            }
            let mut notified = pin!(self.progress.notified());
            notified.as_mut().enable();
            if self.last_physically_written() >= target {
                return;
            }
            notified.await;
        }
    }
}

struct LiveTail {
    output_id: ConnectionOutputId,
    last_admitted_sequence: u64,
    stream: Arc<LiveStreamState>,
}

impl LiveTail {
    fn new(output_id: ConnectionOutputId, baseline: u64) -> Self {
        Self {
            output_id,
            last_admitted_sequence: baseline,
            stream: LiveStreamState::new(baseline),
        }
    }
}

struct EventReplayRegistryEntry {
    owner: ClientId,
    /// Present only while frozen pages remain. Dropped when frozen replay completes.
    frozen: Option<EventReplaySession>,
    limits: PageLimits,
    last_touch: Instant,
    /// Lightweight live delivery metadata; retained after frozen completion.
    live: Option<LiveTail>,
}

struct EventReplayRegistry {
    entries: HashMap<SubscriptionId, EventReplayRegistryEntry>,
}

impl EventReplayRegistry {
    fn new() -> Self {
        Self {
            entries: HashMap::new(),
        }
    }

    fn reap_idle(&mut self, now: Instant) {
        // Incomplete frozen replay keeps the bounded TTL. Completed live
        // subscriptions do not expire merely because no events arrive.
        self.entries.retain(|_, entry| match &entry.frozen {
            Some(_) if now.duration_since(entry.last_touch) >= EVENT_REPLAY_IDLE_TTL => {
                if let Some(live) = &entry.live {
                    live.stream.cancel();
                }
                false
            }
            _ => true,
        });
    }

    /// Evict only incomplete frozen entries that have no live binding.
    /// Never silently evict an active live tail; caller must fail closed.
    fn try_evict_inactive_frozen_for_capacity(&mut self) -> bool {
        if self.entries.len() < MAX_EVENT_REPLAY_SESSIONS {
            return true;
        }
        let victim = self
            .entries
            .iter()
            .filter(|(_, entry)| entry.frozen.is_some() && entry.live.is_none())
            .min_by_key(|(_, entry)| entry.last_touch)
            .map(|(id, _)| *id);
        if let Some(victim_id) = victim {
            let _ = self.remove(victim_id);
            true
        } else {
            false
        }
    }

    fn insert_open(
        &mut self,
        owner: ClientId,
        session: EventReplaySession,
        limits: PageLimits,
        live: Option<LiveTail>,
        retain_frozen: bool,
        now: Instant,
    ) -> Result<SubscriptionId, IpcError> {
        while self.entries.len() >= MAX_EVENT_REPLAY_SESSIONS {
            if !self.try_evict_inactive_frozen_for_capacity() {
                if let Some(live) = &live {
                    live.stream.cancel();
                }
                return Err(IpcError::Busy);
            }
        }
        let subscription_id = session.subscription_id();
        self.entries.insert(
            subscription_id,
            EventReplayRegistryEntry {
                owner,
                frozen: retain_frozen.then_some(session),
                limits,
                last_touch: now,
                live,
            },
        );
        Ok(subscription_id)
    }

    fn touch(&mut self, subscription_id: SubscriptionId, now: Instant) {
        if let Some(entry) = self.entries.get_mut(&subscription_id) {
            entry.last_touch = now;
        }
    }

    fn remove(&mut self, subscription_id: SubscriptionId) -> Option<EventReplayRegistryEntry> {
        let entry = self.entries.remove(&subscription_id)?;
        if let Some(live) = &entry.live {
            live.stream.cancel();
        }
        Some(entry)
    }

    fn remove_for_output(&mut self, output_id: ConnectionOutputId) {
        let mut remove_ids = Vec::new();
        for (subscription_id, entry) in self.entries.iter_mut() {
            let Some(live) = entry.live.as_ref() else {
                continue;
            };
            if live.output_id != output_id {
                continue;
            }
            live.stream.cancel();
            if entry.frozen.is_some() {
                // Incomplete frozen replay keeps its TTL for reconnect; drop only
                // the live binding tied to the closed connection output.
                entry.live = None;
            } else {
                remove_ids.push(*subscription_id);
            }
        }
        for subscription_id in remove_ids {
            self.entries.remove(&subscription_id);
        }
    }

    fn get_frozen(
        &self,
        subscription_id: SubscriptionId,
        requester: ClientId,
        limits: PageLimits,
        now: Instant,
    ) -> Result<&EventReplaySession, QueryError> {
        let Some(entry) = self.entries.get(&subscription_id) else {
            return Err(QueryError::NotFound);
        };
        if entry.frozen.is_some() && now.duration_since(entry.last_touch) >= EVENT_REPLAY_IDLE_TTL {
            return Err(QueryError::NotFound);
        }
        if entry.owner != requester {
            return Err(QueryError::Unauthorized);
        }
        if entry.limits != limits {
            return Err(QueryError::InvalidRequest);
        }
        entry.frozen.as_ref().ok_or(QueryError::NotFound)
    }
}

/// Drops unregister the connection output so executor-held handles cannot keep
/// a pipe/writer/reader/task alive after connection shutdown.
pub(crate) struct ConnectionOutputRegistration {
    id: ConnectionOutputId,
    output: ConnectionOutputHandle,
    control_tx: mpsc::Sender<ExecutorControl>,
}

impl ConnectionOutputRegistration {
    pub(crate) fn id(&self) -> ConnectionOutputId {
        self.id
    }
}

impl Drop for ConnectionOutputRegistration {
    fn drop(&mut self) {
        self.output.request_shutdown();
        let _ = self
            .control_tx
            .try_send(ExecutorControl::UnregisterOutput { id: self.id });
    }
}

/// Cloneable submit handle for the single host CommandBus executor.
#[derive(Clone, Debug)]
pub struct HostRequestHandle {
    tx: mpsc::Sender<HostRequestJob>,
    control_tx: mpsc::Sender<ExecutorControl>,
    output_id: Option<ConnectionOutputId>,
}

impl HostRequestHandle {
    /// Bind this handle clone to one duplex connection output.
    pub(crate) fn with_output(&self, output_id: ConnectionOutputId) -> Self {
        Self {
            tx: self.tx.clone(),
            control_tx: self.control_tx.clone(),
            output_id: Some(output_id),
        }
    }

    /// Register dual-lane output for live durable delivery on this connection.
    ///
    /// The returned registration guard is armed before the send/await window so
    /// task cancellation always requests shutdown even if the executor already
    /// inserted the output and the ack is never observed.
    pub(crate) async fn register_output(
        &self,
        output: ConnectionOutputHandle,
    ) -> Result<ConnectionOutputRegistration, IpcError> {
        let id = output.id();
        // Arm before any await: cancel must not leave an inserted output without
        // a shutdown owner. Shutdown goes through the handle's synchronized path.
        let registration = ConnectionOutputRegistration {
            id,
            output: output.clone(),
            control_tx: self.control_tx.clone(),
        };
        let (ack_tx, ack_rx) = oneshot::channel();
        self.control_tx
            .send(ExecutorControl::RegisterOutput {
                id,
                output,
                ack: ack_tx,
            })
            .await
            .map_err(|_| IpcError::Unavailable)?;
        ack_rx.await.map_err(|_| IpcError::Unavailable)?;
        Ok(registration)
    }

    #[cfg(test)]
    pub(crate) async fn inspect_output(
        &self,
        id: ConnectionOutputId,
    ) -> Result<OutputInspection, IpcError> {
        let (ack_tx, ack_rx) = oneshot::channel();
        self.control_tx
            .send(ExecutorControl::InspectOutput { id, ack: ack_tx })
            .await
            .map_err(|_| IpcError::Unavailable)?;
        ack_rx.await.map_err(|_| IpcError::Unavailable)
    }

    /// Test seam: run exactly one maintenance cleanup/teardown unit on the executor.
    #[cfg(test)]
    pub(crate) async fn run_maintenance_once(&self) -> Result<(), StoreError> {
        let (ack_tx, ack_rx) = oneshot::channel();
        self.control_tx
            .send(ExecutorControl::RunMaintenanceOnce { ack: ack_tx })
            .await
            .map_err(|_| StoreError::Io("executor control channel closed".into()))?;
        ack_rx
            .await
            .map_err(|_| StoreError::Io("maintenance ack dropped".into()))?
    }

    /// Enqueue one authenticated request and await its correlated reply.
    ///
    /// Blocks (with bounded queue backpressure) when the executor queue is full.
    /// Returns [`IpcError::Unavailable`] if the executor has stopped.
    pub async fn execute(
        &self,
        negotiated: NegotiatedParameters,
        request: ClientRequest,
    ) -> Result<ServerMessage, IpcError> {
        match self
            .submit(
                negotiated,
                request,
                HostRequestCompletionRouting::CallerOwned,
            )
            .await?
        {
            DuplexExecuteCompletion::CallerMustWrite(message) => Ok(message),
            DuplexExecuteCompletion::ExecutorAdmittedQuitReceipt { .. } => {
                Err(IpcError::Unavailable)
            }
        }
    }

    /// Duplex path: may return executor-admitted quit receipt (reader must not enqueue).
    pub(crate) async fn execute_for_duplex(
        &self,
        negotiated: NegotiatedParameters,
        request: ClientRequest,
    ) -> Result<DuplexExecuteCompletion, IpcError> {
        self.submit(
            negotiated,
            request,
            HostRequestCompletionRouting::ExecutorOwnsAcceptedHostQuitReceipt,
        )
        .await
    }

    async fn submit(
        &self,
        negotiated: NegotiatedParameters,
        request: ClientRequest,
        routing: HostRequestCompletionRouting,
    ) -> Result<DuplexExecuteCompletion, IpcError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx
            .send(HostRequestJob {
                negotiated,
                request,
                output_id: self.output_id,
                routing,
                reply: reply_tx,
            })
            .await
            .map_err(|_| IpcError::Unavailable)?;
        reply_rx.await.map_err(|_| IpcError::Unavailable)?
    }

    /// Test seam: take the pending accepted-quit receipt ack for one output, if any.
    #[cfg(test)]
    pub(crate) async fn take_pending_quit_receipt_ack(
        &self,
        id: ConnectionOutputId,
    ) -> Result<Option<(OperationId, PhysicalWriteAck)>, IpcError> {
        let (ack_tx, ack_rx) = oneshot::channel();
        self.control_tx
            .send(ExecutorControl::TakePendingQuitReceiptAck { id, ack: ack_tx })
            .await
            .map_err(|_| IpcError::Unavailable)?;
        ack_rx.await.map_err(|_| IpcError::Unavailable)
    }
}

/// Exclusive owner of [`CommandBus`]. Runs on one task and drains a bounded queue.
pub struct HostRequestExecutor {
    bus: CommandBus,
    rx: mpsc::Receiver<HostRequestJob>,
    control_rx: mpsc::Receiver<ExecutorControl>,
    control_closed: bool,
    registry: SnapshotRegistry,
    replay_registry: EventReplayRegistry,
    artifact_content_registry: ArtifactContentRegistry,
    outputs: HashMap<ConnectionOutputId, ConnectionOutputHandle>,
    /// Latest accepted ConfirmHostQuit receipt ack per output (for terminal drain).
    pending_quit_receipt_acks: HashMap<ConnectionOutputId, PendingQuitReceiptAck>,
    /// Supervised foreground only: capacity-one arm sender to the host supervisor.
    arm_tx: Option<mpsc::Sender<PhysicalExitArmRequest>>,
}

impl HostRequestExecutor {
    /// Spawn the single CommandBus executor task.
    ///
    /// The returned handle may be cloned for every connection task. Dropping
    /// every handle closes the queue; the executor then finishes after draining
    /// any already-queued jobs.
    pub fn start(bus: CommandBus) -> (HostRequestHandle, JoinHandle<()>) {
        Self::spawn(bus, true)
    }

    /// Supervised foreground start: arm channel + typed intentional exit outcome.
    ///
    /// Ordinary [`Self::start`] callers are unchanged. The supervisor must drop the
    /// pending accept listener before acknowledging [`PhysicalExitArmRequest`].
    pub fn start_supervised(bus: CommandBus) -> (HostRequestHandle, SupervisedHostExecutor) {
        let (handle, join, arm_rx) = Self::spawn_supervised(bus, true);
        (handle, SupervisedHostExecutor { arm_rx, join })
    }

    /// Test-only: same executor as [`Self::start`], but without the automatic
    /// maintenance timer so explicit [`HostRequestHandle::run_maintenance_once`]
    /// calls are the only cleanup/teardown driver.
    #[cfg(test)]
    fn start_without_automatic_maintenance(bus: CommandBus) -> (HostRequestHandle, JoinHandle<()>) {
        Self::spawn(bus, false)
    }

    /// Test-only supervised start without the automatic maintenance timer.
    #[cfg(test)]
    fn start_supervised_without_automatic_maintenance(
        bus: CommandBus,
    ) -> (HostRequestHandle, SupervisedHostExecutor) {
        let (handle, join, arm_rx) = Self::spawn_supervised(bus, false);
        (handle, SupervisedHostExecutor { arm_rx, join })
    }

    fn spawn_supervised(
        bus: CommandBus,
        schedule_automatic_maintenance: bool,
    ) -> (
        HostRequestHandle,
        JoinHandle<Result<HostExecutorOutcome, StoreError>>,
        mpsc::Receiver<PhysicalExitArmRequest>,
    ) {
        let (tx, rx) = mpsc::channel(HOST_REQUEST_QUEUE_CAPACITY);
        let (control_tx, control_rx) = mpsc::channel(HOST_REQUEST_QUEUE_CAPACITY);
        let (arm_tx, arm_rx) = mpsc::channel(1);
        let handle = HostRequestHandle {
            tx,
            control_tx,
            output_id: None,
        };
        let mut executor = Self {
            bus,
            rx,
            control_rx,
            control_closed: false,
            registry: SnapshotRegistry::new(),
            replay_registry: EventReplayRegistry::new(),
            artifact_content_registry: ArtifactContentRegistry::new(),
            outputs: HashMap::new(),
            pending_quit_receipt_acks: HashMap::new(),
            arm_tx: Some(arm_tx),
        };
        let join = tokio::spawn(async move {
            executor
                .run_supervised(schedule_automatic_maintenance)
                .await
        });
        (handle, join, arm_rx)
    }

    fn spawn(
        bus: CommandBus,
        schedule_automatic_maintenance: bool,
    ) -> (HostRequestHandle, JoinHandle<()>) {
        let (tx, rx) = mpsc::channel(HOST_REQUEST_QUEUE_CAPACITY);
        let (control_tx, control_rx) = mpsc::channel(HOST_REQUEST_QUEUE_CAPACITY);
        let handle = HostRequestHandle {
            tx,
            control_tx,
            output_id: None,
        };
        let mut executor = Self {
            bus,
            rx,
            control_rx,
            control_closed: false,
            registry: SnapshotRegistry::new(),
            replay_registry: EventReplayRegistry::new(),
            artifact_content_registry: ArtifactContentRegistry::new(),
            outputs: HashMap::new(),
            pending_quit_receipt_acks: HashMap::new(),
            arm_tx: None,
        };
        let join = tokio::spawn(async move {
            executor.run(schedule_automatic_maintenance).await;
        });
        (handle, join)
    }

    async fn run(&mut self, schedule_automatic_maintenance: bool) {
        // `interval` ticks immediately. Delay the first maintenance pass so
        // startup does not race an eager teardown scan.
        let period = SNAPSHOT_REAPER_PERIOD.min(EVENT_REPLAY_REAPER_PERIOD);
        let mut reaper = interval_at(tokio::time::Instant::now() + period, period);
        reaper.set_missed_tick_behavior(MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                job = self.rx.recv() => {
                    let Some(job) = job else {
                        break;
                    };
                    let result = self.dispatch_job(job.negotiated, job.request, job.output_id, job.routing);
                    // If the connection task went away, drop the reply; do not panic.
                    let _ = job.reply.send(result);
                }
                control = self.control_rx.recv(), if !self.control_closed => {
                    let Some(control) = control else {
                        // Do not busy-spin: stop polling a closed control channel.
                        self.control_closed = true;
                        continue;
                    };
                    self.handle_control(control);
                }
                _ = reaper.tick(), if schedule_automatic_maintenance => {
                    let now = Instant::now();
                    self.registry.reap_idle(now);
                    self.replay_registry.reap_idle(now);
                    self.artifact_content_registry.reap(now);
                    // Missed unregister try_send must not leave completed live
                    // metadata forever once the connection has requested shutdown.
                    self.reap_shutdown_outputs();
                    // While Open: at most one process-empty teardown per tick.
                    // While Closing: advance exactly one durable host-cleanup unit.
                    // StoreError fails closed so host supervision sees unexpected exit.
                    if self.run_one_cleanup_or_teardown_unit().is_err() {
                        break;
                    }
                }
            }
        }
    }

    async fn run_supervised(
        &mut self,
        schedule_automatic_maintenance: bool,
    ) -> Result<HostExecutorOutcome, StoreError> {
        let period = SNAPSHOT_REAPER_PERIOD.min(EVENT_REPLAY_REAPER_PERIOD);
        let mut reaper = interval_at(tokio::time::Instant::now() + period, period);
        reaper.set_missed_tick_behavior(MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                job = self.rx.recv() => {
                    let Some(job) = job else {
                        return Err(StoreError::Io(
                            "supervised executor request queue closed unexpectedly".into(),
                        ));
                    };
                    let result = self.dispatch_job(job.negotiated, job.request, job.output_id, job.routing);
                    let _ = job.reply.send(result);
                }
                control = self.control_rx.recv(), if !self.control_closed => {
                    let Some(control) = control else {
                        self.control_closed = true;
                        continue;
                    };
                    if let Some(outcome) = self.handle_control_supervised(control).await? {
                        return Ok(outcome);
                    }
                }
                _ = reaper.tick(), if schedule_automatic_maintenance => {
                    let now = Instant::now();
                    self.registry.reap_idle(now);
                    self.replay_registry.reap_idle(now);
                    self.artifact_content_registry.reap(now);
                    self.reap_shutdown_outputs();
                    if let Some(outcome) = self.drive_supervised_maintenance_unit().await? {
                        return Ok(outcome);
                    }
                }
            }
        }
    }

    fn handle_control(&mut self, control: ExecutorControl) {
        match control {
            ExecutorControl::RegisterOutput { id, output, ack } => {
                self.outputs.insert(id, output);
                if ack.send(()).is_err() {
                    self.detach_output(id);
                }
            }
            ExecutorControl::UnregisterOutput { id } => {
                self.detach_output(id);
            }
            #[cfg(test)]
            ExecutorControl::InspectOutput { id, ack } => {
                let registered = self.outputs.contains_key(&id);
                let live_bound = self
                    .replay_registry
                    .entries
                    .values()
                    .any(|entry| entry.live.as_ref().is_some_and(|live| live.output_id == id));
                let _ = ack.send(OutputInspection {
                    registered,
                    live_bound,
                });
            }
            #[cfg(test)]
            ExecutorControl::RunMaintenanceOnce { ack } => {
                let result = self.run_one_cleanup_or_teardown_unit();
                let _ = ack.send(result);
            }
            #[cfg(test)]
            ExecutorControl::TakePendingQuitReceiptAck { id, ack } => {
                let taken = self
                    .pending_quit_receipt_acks
                    .remove(&id)
                    .map(|pending| (pending.operation_id, pending.ack));
                let _ = ack.send(taken);
            }
        }
    }

    async fn handle_control_supervised(
        &mut self,
        control: ExecutorControl,
    ) -> Result<Option<HostExecutorOutcome>, StoreError> {
        match control {
            #[cfg(test)]
            ExecutorControl::RunMaintenanceOnce { ack } => {
                match self.drive_supervised_maintenance_unit().await {
                    Ok(Some(outcome)) => {
                        let _ = ack.send(Ok(()));
                        Ok(Some(outcome))
                    }
                    Ok(None) => {
                        let _ = ack.send(Ok(()));
                        Ok(None)
                    }
                    Err(error) => {
                        let _ = ack.send(Err(error.clone()));
                        Err(error)
                    }
                }
            }
            other => {
                self.handle_control(other);
                Ok(None)
            }
        }
    }

    /// Advance one Open/Closing unit; on supervised ReadyToExit, arm+settle+exit.
    async fn drive_supervised_maintenance_unit(
        &mut self,
    ) -> Result<Option<HostExecutorOutcome>, StoreError> {
        let closing = self.bus.host_admission_is_closing()?;
        if !closing {
            match ProcessEmptyTeardownWorker::run_once(&mut self.bus)? {
                ProcessEmptyTeardown::Idle => Ok(None),
                ProcessEmptyTeardown::Settled { .. } => {
                    self.fan_out_live_durable_events();
                    Ok(None)
                }
            }
        } else {
            match HostCleanupWorker::run_once(&mut self.bus)? {
                HostCleanupProgress::Idle => Ok(None),
                HostCleanupProgress::ReadyToExit {
                    operation_id,
                    action_epoch,
                } => self
                    .arm_and_complete_intentional_quit(operation_id, action_epoch)
                    .await
                    .map(Some),
                HostCleanupProgress::Progressed { .. }
                | HostCleanupProgress::BranchCompleted { .. }
                | HostCleanupProgress::Failed { .. } => {
                    self.fan_out_live_durable_events();
                    Ok(None)
                }
            }
        }
    }

    async fn arm_and_complete_intentional_quit(
        &mut self,
        operation_id: OperationId,
        action_epoch: u64,
    ) -> Result<HostExecutorOutcome, StoreError> {
        let arm_tx = self.arm_tx.as_ref().ok_or_else(|| {
            StoreError::Io("supervised executor missing physical-exit arm sender".into())
        })?;
        let (ack_tx, ack_rx) = oneshot::channel();
        arm_tx
            .send(PhysicalExitArmRequest {
                operation_id,
                action_epoch,
                ack: ack_tx,
            })
            .await
            .map_err(|_| {
                StoreError::Io("physical-exit arm request rejected by supervisor".into())
            })?;
        ack_rx.await.map_err(|_| {
            StoreError::Io("physical-exit arm acknowledgement dropped by supervisor".into())
        })?;

        self.quiesce_intake();
        // Fail closed on receipt lineage before any durable settle can persist Closed.
        self.reap_shutdown_outputs();
        let high_water = std::mem::take(&mut self.pending_quit_receipt_acks);
        for pending in high_water.values() {
            if pending.operation_id != operation_id {
                return Err(StoreError::Corruption);
            }
        }

        let settlement = HostCleanupWorker::settle_success(&mut self.bus)?;
        if settlement.operation_id != operation_id || settlement.action_epoch != action_epoch {
            return Err(StoreError::Corruption);
        }

        self.deliver_terminal_and_await_high_water(
            settlement.terminal_event,
            operation_id,
            high_water,
        )
        .await?;

        for output in self.outputs.values() {
            output.request_shutdown();
        }

        Ok(HostExecutorOutcome::Intentional {
            operation_id,
            action_epoch,
        })
    }

    fn quiesce_intake(&mut self) {
        self.rx.close();
        self.control_rx.close();
        self.control_closed = true;
        while let Ok(job) = self.rx.try_recv() {
            let _ = job.reply.send(Err(IpcError::Unavailable));
        }
        while let Ok(control) = self.control_rx.try_recv() {
            match control {
                ExecutorControl::RegisterOutput { output, ack, .. } => {
                    output.request_shutdown();
                    drop(ack);
                }
                ExecutorControl::UnregisterOutput { id } => {
                    self.detach_output(id);
                }
                #[cfg(test)]
                ExecutorControl::InspectOutput { ack, .. } => {
                    drop(ack);
                }
                #[cfg(test)]
                ExecutorControl::RunMaintenanceOnce { ack } => {
                    let _ = ack.send(Err(StoreError::Io(
                        "maintenance rejected after quit intake quiesce".into(),
                    )));
                }
                #[cfg(test)]
                ExecutorControl::TakePendingQuitReceiptAck { ack, .. } => {
                    let _ = ack.send(None);
                }
            }
        }
    }

    async fn deliver_terminal_and_await_high_water(
        &mut self,
        terminal_event: DomainEvent,
        quit_operation_id: OperationId,
        mut high_water: HashMap<ConnectionOutputId, PendingQuitReceiptAck>,
    ) -> Result<(), StoreError> {
        self.reap_shutdown_outputs();
        let deadline = Instant::now() + QUIT_TERMINAL_ACK_TIMEOUT;

        // Snapshot each live tail's stream + last admitted sequence, grouped by
        // output. Subscription IDs are sorted for deterministic terminal order.
        let mut live_bindings: Vec<(ConnectionOutputId, SubscriptionId)> = self
            .replay_registry
            .entries
            .iter()
            .filter_map(|(subscription_id, entry)| {
                entry
                    .live
                    .as_ref()
                    .map(|live| (live.output_id, *subscription_id))
            })
            .collect();
        live_bindings.sort_unstable();

        let mut by_output: BTreeMap<
            ConnectionOutputId,
            Vec<(SubscriptionId, Arc<LiveStreamState>, u64)>,
        > = BTreeMap::new();
        for (output_id, subscription_id) in live_bindings {
            let Some(live) = self
                .replay_registry
                .entries
                .get(&subscription_id)
                .and_then(|entry| entry.live.as_ref())
            else {
                continue;
            };
            by_output.entry(output_id).or_default().push((
                subscription_id,
                Arc::clone(&live.stream),
                live.last_admitted_sequence,
            ));
        }

        let mut pending_outputs: HashMap<ConnectionOutputId, Vec<SubscriptionId>> = HashMap::new();
        let mut fences = FuturesUnordered::new();
        for (output_id, tails) in by_output {
            let subscription_ids: Vec<SubscriptionId> = tails
                .iter()
                .map(|(subscription_id, _, _)| *subscription_id)
                .collect();
            pending_outputs.insert(output_id, subscription_ids.clone());
            fences.push(async move {
                for (_, stream, target) in tails {
                    stream.wait_until_physically_written(target).await;
                }
                (output_id, subscription_ids)
            });
        }

        while !fences.is_empty() {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            match tokio::time::timeout(remaining, fences.next()).await {
                Ok(Some((output_id, subscription_ids))) => {
                    pending_outputs.remove(&output_id);
                    // Durable high-water reached: cancel only these live tails, then
                    // admit ordered terminal CRITICAL (never after skipped history).
                    for subscription_id in &subscription_ids {
                        self.replay_registry.remove(*subscription_id);
                    }
                    let Some(output) = self.outputs.get(&output_id).cloned() else {
                        high_water.remove(&output_id);
                        continue;
                    };
                    let mut last_terminal_ack = None;
                    let mut admit_ok = true;
                    for subscription_id in &subscription_ids {
                        match output.try_enqueue_critical_tracked(ServerMessage::DurableEvent {
                            subscription_id: *subscription_id,
                            event: terminal_event.clone(),
                        }) {
                            Ok(ack) => last_terminal_ack = Some(ack),
                            Err(_) => {
                                admit_ok = false;
                                break;
                            }
                        }
                    }
                    if !admit_ok {
                        if let Some(output) = self.outputs.get(&output_id) {
                            output.request_shutdown();
                        }
                        high_water.remove(&output_id);
                        continue;
                    }
                    if let Some(ack) = last_terminal_ack {
                        high_water.insert(
                            output_id,
                            PendingQuitReceiptAck {
                                operation_id: quit_operation_id,
                                ack,
                            },
                        );
                    } else {
                        high_water.remove(&output_id);
                    }
                }
                Ok(None) => break,
                Err(_) => break,
            }
        }

        // Shared deadline expired or fences dropped: never settle after skipped history.
        for (output_id, subscription_ids) in pending_outputs.drain() {
            self.abort_quit_output_chain(output_id, &subscription_ids, &mut high_water);
        }
        drop(fences);

        // Receipt-only / final-terminal high-waters use only the remainder of the
        // same absolute deadline — no per-client fresh timeout.
        let remaining = deadline.saturating_duration_since(Instant::now());
        if !remaining.is_zero() {
            let _ = tokio::time::timeout(remaining, async {
                for (_, pending) in high_water {
                    let _ = pending.ack.wait().await;
                }
            })
            .await;
        }
        Ok(())
    }

    fn abort_quit_output_chain(
        &mut self,
        output_id: ConnectionOutputId,
        subscription_ids: &[SubscriptionId],
        high_water: &mut HashMap<ConnectionOutputId, PendingQuitReceiptAck>,
    ) {
        for subscription_id in subscription_ids {
            self.replay_registry.remove(*subscription_id);
        }
        if let Some(output) = self.outputs.get(&output_id) {
            output.request_shutdown();
        }
        high_water.remove(&output_id);
    }

    /// Advance exactly one Open teardown or Closing cleanup unit and fan out on progress.
    fn run_one_cleanup_or_teardown_unit(&mut self) -> Result<(), StoreError> {
        let closing = self.bus.host_admission_is_closing()?;
        let fan_out = if closing {
            match HostCleanupWorker::run_once(&mut self.bus)? {
                HostCleanupProgress::Idle | HostCleanupProgress::ReadyToExit { .. } => false,
                HostCleanupProgress::Progressed { .. }
                | HostCleanupProgress::BranchCompleted { .. }
                | HostCleanupProgress::Failed { .. } => true,
            }
        } else {
            match ProcessEmptyTeardownWorker::run_once(&mut self.bus)? {
                ProcessEmptyTeardown::Idle => false,
                ProcessEmptyTeardown::Settled { .. } => true,
            }
        };
        if fan_out {
            self.fan_out_live_durable_events();
        }
        Ok(())
    }

    fn detach_output(&mut self, id: ConnectionOutputId) {
        self.pending_quit_receipt_acks.remove(&id);
        if let Some(output) = self.outputs.remove(&id) {
            output.request_shutdown();
        }
        self.replay_registry.remove_for_output(id);
    }

    /// Remove one output registration for an acknowledged detach without
    /// requesting shutdown yet (ack must be physically written first).
    fn release_output_for_detach(
        &mut self,
        id: ConnectionOutputId,
    ) -> Option<ConnectionOutputHandle> {
        self.pending_quit_receipt_acks.remove(&id);
        let output = self.outputs.remove(&id);
        self.replay_registry.remove_for_output(id);
        output
    }

    fn serve_detach(
        &mut self,
        negotiated: NegotiatedParameters,
        request: DetachRequest,
        output_id: Option<ConnectionOutputId>,
    ) -> Result<ServerMessage, IpcError> {
        if !negotiated.capabilities.contains(Capability::ExplicitDetach) {
            return Err(IpcError::UnsupportedCapability);
        }
        if request.client_id != negotiated.client_id {
            return Err(IpcError::Unauthorized);
        }
        let Some(registered_id) = output_id else {
            return Err(IpcError::Unauthorized);
        };
        let requested_id = ConnectionOutputId::from_uuid(request.connection_id);
        if requested_id != registered_id {
            return Err(IpcError::Unauthorized);
        }
        if self.release_output_for_detach(registered_id).is_none() {
            return Err(IpcError::Unauthorized);
        }
        Ok(ServerMessage::Detached(DetachAck {
            request_id: request.request_id,
            connection_id: request.connection_id,
        }))
    }

    fn reap_shutdown_outputs(&mut self) {
        let dead = self
            .outputs
            .iter()
            .filter(|(_, output)| output.is_shutdown_requested())
            .map(|(id, _)| *id)
            .collect::<Vec<_>>();
        for id in dead {
            self.detach_output(id);
        }
    }

    fn dispatch_job(
        &mut self,
        negotiated: NegotiatedParameters,
        request: ClientRequest,
        output_id: Option<ConnectionOutputId>,
        routing: HostRequestCompletionRouting,
    ) -> Result<DuplexExecuteCompletion, IpcError> {
        let is_confirm_host_quit = matches!(
            &request,
            ClientRequest::Command(envelope)
                if matches!(envelope.command, Command::ConfirmHostQuit(_))
        );
        let response = self.dispatch(negotiated, request, output_id)?;
        if !matches!(
            routing,
            HostRequestCompletionRouting::ExecutorOwnsAcceptedHostQuitReceipt
        ) {
            return Ok(DuplexExecuteCompletion::CallerMustWrite(response));
        }
        // lookup_receipt is command_id-keyed: a ConfirmHostQuit-shaped request can
        // surface a prior non-quit Accepted. Own only the durable host-admission
        // receipt shape (task_revision None, exactly one event_id).
        let operation_id = match (&response, is_confirm_host_quit) {
            (
                ServerMessage::CommandReceipt(CommandReceipt::Accepted {
                    operation_id,
                    task_revision: None,
                    event_ids,
                    ..
                }),
                true,
            ) if event_ids.len() == 1 => *operation_id,
            _ => return Ok(DuplexExecuteCompletion::CallerMustWrite(response)),
        };
        let Some(output_id) = output_id else {
            return Err(IpcError::Unavailable);
        };
        let Some(output) = self.outputs.get(&output_id) else {
            return Err(IpcError::Unavailable);
        };
        let ack = output.try_enqueue_critical_tracked(response)?;
        self.pending_quit_receipt_acks
            .insert(output_id, PendingQuitReceiptAck { operation_id, ack });
        Ok(DuplexExecuteCompletion::ExecutorAdmittedQuitReceipt { operation_id })
    }

    fn dispatch(
        &mut self,
        negotiated: NegotiatedParameters,
        request: ClientRequest,
        output_id: Option<ConnectionOutputId>,
    ) -> Result<ServerMessage, IpcError> {
        match request {
            ClientRequest::Command(envelope) => {
                if envelope.client_id != negotiated.client_id {
                    return Err(IpcError::Unauthorized);
                }
                if matches!(envelope.command, Command::ConfirmHostQuit(_))
                    && !negotiated.capabilities.contains(Capability::HostShutdown)
                {
                    return Err(IpcError::UnsupportedCapability);
                }
                let receipt = self.bus.execute(envelope).map_err(map_store_error)?;
                self.fan_out_live_durable_events();
                Ok(ServerMessage::CommandReceipt(receipt))
            }
            ClientRequest::Query(envelope) => {
                if envelope.client_id != negotiated.client_id {
                    return Err(IpcError::Unauthorized);
                }
                let reply = self.dispatch_query(negotiated, envelope, output_id)?;
                Ok(ServerMessage::QueryReply(reply))
            }
            ClientRequest::Detach(request) => self.serve_detach(negotiated, request, output_id),
        }
    }

    fn dispatch_query(
        &mut self,
        negotiated: NegotiatedParameters,
        envelope: QueryEnvelope,
        output_id: Option<ConnectionOutputId>,
    ) -> Result<QueryReply, IpcError> {
        let task_id = envelope.task_id;
        match envelope.query {
            Query::SnapshotPage {
                section,
                snapshot_id,
                resume_cursor,
            } => {
                if !negotiated.capabilities.contains(Capability::PagedSnapshots) {
                    return Ok(QueryReply {
                        request_id: envelope.request_id,
                        outcome: QueryOutcome::Err(QueryError::UnsupportedCapability),
                    });
                }
                let outcome =
                    self.serve_snapshot_page(negotiated, section, snapshot_id, resume_cursor)?;
                Ok(QueryReply {
                    request_id: envelope.request_id,
                    outcome,
                })
            }
            Query::ReleaseSnapshot { snapshot_id } => {
                if !negotiated.capabilities.contains(Capability::PagedSnapshots) {
                    return Ok(QueryReply {
                        request_id: envelope.request_id,
                        outcome: QueryOutcome::Err(QueryError::UnsupportedCapability),
                    });
                }
                let outcome = self.serve_release_snapshot(negotiated.client_id, snapshot_id);
                Ok(QueryReply {
                    request_id: envelope.request_id,
                    outcome,
                })
            }
            Query::OpenEventReplay { after_sequence } => {
                if task_id.is_some() {
                    return Ok(QueryReply {
                        request_id: envelope.request_id,
                        outcome: QueryOutcome::Err(QueryError::InvalidRequest),
                    });
                }
                if !negotiated.capabilities.contains(Capability::EventReplay) {
                    return Ok(QueryReply {
                        request_id: envelope.request_id,
                        outcome: QueryOutcome::Err(QueryError::UnsupportedCapability),
                    });
                }
                let outcome =
                    self.serve_open_event_replay(negotiated, after_sequence, output_id)?;
                Ok(QueryReply {
                    request_id: envelope.request_id,
                    outcome,
                })
            }
            Query::ContinueEventReplay {
                subscription_id,
                resume_cursor,
            } => {
                if task_id.is_some() {
                    return Ok(QueryReply {
                        request_id: envelope.request_id,
                        outcome: QueryOutcome::Err(QueryError::InvalidRequest),
                    });
                }
                if !negotiated.capabilities.contains(Capability::EventReplay) {
                    return Ok(QueryReply {
                        request_id: envelope.request_id,
                        outcome: QueryOutcome::Err(QueryError::UnsupportedCapability),
                    });
                }
                let outcome = self.serve_continue_event_replay(
                    negotiated,
                    subscription_id,
                    resume_cursor,
                    output_id,
                )?;
                Ok(QueryReply {
                    request_id: envelope.request_id,
                    outcome,
                })
            }
            Query::ReleaseEventReplay { subscription_id } => {
                if task_id.is_some() {
                    return Ok(QueryReply {
                        request_id: envelope.request_id,
                        outcome: QueryOutcome::Err(QueryError::InvalidRequest),
                    });
                }
                if !negotiated.capabilities.contains(Capability::EventReplay) {
                    return Ok(QueryReply {
                        request_id: envelope.request_id,
                        outcome: QueryOutcome::Err(QueryError::UnsupportedCapability),
                    });
                }
                let outcome =
                    self.serve_release_event_replay(negotiated.client_id, subscription_id);
                Ok(QueryReply {
                    request_id: envelope.request_id,
                    outcome,
                })
            }
            Query::OpenArtifactContent { artifact_id } => {
                let Some(task_id) = task_id else {
                    return Ok(QueryReply {
                        request_id: envelope.request_id,
                        outcome: QueryOutcome::Err(QueryError::InvalidRequest),
                    });
                };
                if !negotiated.capabilities.contains(Capability::ChunkResume) {
                    return Ok(QueryReply {
                        request_id: envelope.request_id,
                        outcome: QueryOutcome::Err(QueryError::UnsupportedCapability),
                    });
                }
                let outcome = self.serve_open_artifact_content(negotiated, task_id, artifact_id)?;
                Ok(QueryReply {
                    request_id: envelope.request_id,
                    outcome,
                })
            }
            Query::ContinueArtifactContent {
                subscription_id,
                resume_cursor,
            } => {
                let Some(task_id) = task_id else {
                    return Ok(QueryReply {
                        request_id: envelope.request_id,
                        outcome: QueryOutcome::Err(QueryError::InvalidRequest),
                    });
                };
                if !negotiated.capabilities.contains(Capability::ChunkResume) {
                    return Ok(QueryReply {
                        request_id: envelope.request_id,
                        outcome: QueryOutcome::Err(QueryError::UnsupportedCapability),
                    });
                }
                let outcome = self.serve_continue_artifact_content(
                    negotiated,
                    task_id,
                    subscription_id,
                    resume_cursor,
                )?;
                Ok(QueryReply {
                    request_id: envelope.request_id,
                    outcome,
                })
            }
            Query::ReleaseArtifactContent { subscription_id } => {
                let Some(task_id) = task_id else {
                    return Ok(QueryReply {
                        request_id: envelope.request_id,
                        outcome: QueryOutcome::Err(QueryError::InvalidRequest),
                    });
                };
                if !negotiated.capabilities.contains(Capability::ChunkResume) {
                    return Ok(QueryReply {
                        request_id: envelope.request_id,
                        outcome: QueryOutcome::Err(QueryError::UnsupportedCapability),
                    });
                }
                let outcome = self.serve_release_artifact_content(
                    negotiated.client_id,
                    task_id,
                    subscription_id,
                )?;
                Ok(QueryReply {
                    request_id: envelope.request_id,
                    outcome,
                })
            }
            Query::InspectHostQuit => {
                if task_id.is_some() {
                    return Ok(QueryReply {
                        request_id: envelope.request_id,
                        outcome: QueryOutcome::Err(QueryError::InvalidRequest),
                    });
                }
                if !negotiated.capabilities.contains(Capability::HostShutdown) {
                    return Ok(QueryReply {
                        request_id: envelope.request_id,
                        outcome: QueryOutcome::Err(QueryError::UnsupportedCapability),
                    });
                }
                self.bus.query(envelope).map_err(map_store_error)
            }
            Query::OperationStatus { .. } | Query::TaskSnapshot => {
                self.bus.query(envelope).map_err(map_store_error)
            }
        }
    }

    fn serve_snapshot_page(
        &mut self,
        negotiated: NegotiatedParameters,
        section: SnapshotSection,
        snapshot_id: Option<SnapshotId>,
        resume_cursor: Option<Vec<u8>>,
    ) -> Result<QueryOutcome, IpcError> {
        match (snapshot_id, resume_cursor) {
            (None, None) => self.open_snapshot_page(negotiated, section),
            (Some(snapshot_id), None) => {
                self.begin_snapshot_section(negotiated, section, snapshot_id)
            }
            (Some(snapshot_id), Some(resume_cursor)) => {
                self.resume_snapshot_page(negotiated, section, snapshot_id, resume_cursor)
            }
            (None, Some(_)) => Ok(QueryOutcome::Err(QueryError::InvalidRequest)),
        }
    }

    fn open_snapshot_page(
        &mut self,
        negotiated: NegotiatedParameters,
        section: SnapshotSection,
    ) -> Result<QueryOutcome, IpcError> {
        let now = Instant::now();
        self.registry.reap_idle(now);
        let limits = page_limits_from_negotiated(negotiated)?;
        let session = self
            .bus
            .begin_snapshot(limits)
            .map_err(map_snapshot_error_transport)?;
        let page = match session.page(section, None) {
            Ok(page) => page,
            Err(error) => return map_snapshot_error(error),
        };
        // Retain the pinned session for every valid open page, including empty
        // or single-page first sections, until explicit release / TTL / eviction.
        self.registry
            .insert(negotiated.client_id, session, limits, Instant::now());
        Ok(QueryOutcome::Ok(QueryResult::SnapshotPage { page }))
    }

    fn begin_snapshot_section(
        &mut self,
        negotiated: NegotiatedParameters,
        section: SnapshotSection,
        snapshot_id: SnapshotId,
    ) -> Result<QueryOutcome, IpcError> {
        let now = Instant::now();
        self.registry.reap_idle(now);
        if let Some(entry) = self.registry.entries.get(&snapshot_id) {
            if now.duration_since(entry.last_touch) >= SNAPSHOT_IDLE_TTL {
                self.registry.remove(snapshot_id);
                return Ok(QueryOutcome::Err(QueryError::NotFound));
            }
        }
        let limits = page_limits_from_negotiated(negotiated)?;
        let session = match self
            .registry
            .get(snapshot_id, negotiated.client_id, limits, now)
        {
            Ok(session) => session,
            Err(error) => return Ok(QueryOutcome::Err(error)),
        };
        let page = match session.page(section, None) {
            Ok(page) => page,
            Err(error) => return map_snapshot_error(error),
        };
        self.registry.touch(snapshot_id, Instant::now());
        Ok(QueryOutcome::Ok(QueryResult::SnapshotPage { page }))
    }

    fn resume_snapshot_page(
        &mut self,
        negotiated: NegotiatedParameters,
        section: SnapshotSection,
        snapshot_id: SnapshotId,
        resume_cursor: Vec<u8>,
    ) -> Result<QueryOutcome, IpcError> {
        let now = Instant::now();
        self.registry.reap_idle(now);
        // Expire idle entries before serving so TTL maps to NotFound.
        if let Some(entry) = self.registry.entries.get(&snapshot_id) {
            if now.duration_since(entry.last_touch) >= SNAPSHOT_IDLE_TTL {
                self.registry.remove(snapshot_id);
                return Ok(QueryOutcome::Err(QueryError::NotFound));
            }
        }
        let limits = page_limits_from_negotiated(negotiated)?;
        let session = match self
            .registry
            .get(snapshot_id, negotiated.client_id, limits, now)
        {
            Ok(session) => session,
            Err(error) => return Ok(QueryOutcome::Err(error)),
        };
        let page = match session.page(section, Some(resume_cursor.as_slice())) {
            Ok(page) => page,
            Err(error) => {
                // Cursor/shape failures leave a valid retained session intact.
                return map_snapshot_error(error);
            }
        };
        // Finished sections stay pinned; only release / TTL / eviction drops them.
        self.registry.touch(snapshot_id, Instant::now());
        Ok(QueryOutcome::Ok(QueryResult::SnapshotPage { page }))
    }

    fn serve_release_snapshot(
        &mut self,
        requester: ClientId,
        snapshot_id: SnapshotId,
    ) -> QueryOutcome {
        let now = Instant::now();
        self.registry.reap_idle(now);
        match self.registry.entries.get(&snapshot_id) {
            None => QueryOutcome::Ok(QueryResult::SnapshotReleased { snapshot_id }),
            Some(entry) if entry.owner != requester => QueryOutcome::Err(QueryError::Unauthorized),
            Some(_) => {
                self.registry.remove(snapshot_id);
                QueryOutcome::Ok(QueryResult::SnapshotReleased { snapshot_id })
            }
        }
    }

    fn serve_open_event_replay(
        &mut self,
        negotiated: NegotiatedParameters,
        after_sequence: u64,
        output_id: Option<ConnectionOutputId>,
    ) -> Result<QueryOutcome, IpcError> {
        let now = Instant::now();
        self.replay_registry.reap_idle(now);
        let limits = page_limits_from_negotiated(negotiated)?;
        let session = match self.bus.begin_event_replay(after_sequence, limits) {
            Ok(session) => session,
            Err(error) => return map_replay_error(error),
        };
        let subscription_id = session.subscription_id();
        let page = match session.page(None) {
            Ok(page) => page,
            Err(error) => return map_replay_error(error),
        };
        let retain_frozen = page.next_cursor.is_some();
        let live = output_id.map(|output_id| LiveTail::new(output_id, page.through_sequence));
        if retain_frozen || live.is_some() {
            self.replay_registry.insert_open(
                negotiated.client_id,
                session,
                limits,
                live,
                retain_frozen,
                Instant::now(),
            )?;
            if output_id.is_some() {
                self.catch_up_subscription(subscription_id);
            }
        }
        Ok(QueryOutcome::Ok(QueryResult::EventReplayPage {
            subscription_id,
            page,
        }))
    }

    fn serve_continue_event_replay(
        &mut self,
        negotiated: NegotiatedParameters,
        subscription_id: SubscriptionId,
        resume_cursor: Vec<u8>,
        output_id: Option<ConnectionOutputId>,
    ) -> Result<QueryOutcome, IpcError> {
        let now = Instant::now();
        self.replay_registry.reap_idle(now);
        if let Some(entry) = self.replay_registry.entries.get(&subscription_id) {
            if entry.frozen.is_some()
                && now.duration_since(entry.last_touch) >= EVENT_REPLAY_IDLE_TTL
            {
                self.replay_registry.remove(subscription_id);
                return Ok(QueryOutcome::Err(QueryError::NotFound));
            }
        }
        let limits = page_limits_from_negotiated(negotiated)?;
        let session = match self.replay_registry.get_frozen(
            subscription_id,
            negotiated.client_id,
            limits,
            now,
        ) {
            Ok(session) => session,
            Err(error) => return Ok(QueryOutcome::Err(error)),
        };
        let page = match session.page(Some(resume_cursor.as_slice())) {
            Ok(page) => page,
            Err(error) => {
                // Cursor/shape failures leave a valid retained session intact.
                return map_replay_error(error);
            }
        };
        let through_sequence = page.through_sequence;
        let finished = page.next_cursor.is_none();
        if finished {
            // Drop the SQLite read view but retain lightweight live metadata.
            if let Some(entry) = self.replay_registry.entries.get_mut(&subscription_id) {
                entry.frozen = None;
                entry.last_touch = Instant::now();
                Self::bind_live_preserving_admitted(entry, output_id, through_sequence);
            }
            if self
                .replay_registry
                .entries
                .get(&subscription_id)
                .is_some_and(|entry| entry.live.is_some())
            {
                self.catch_up_subscription(subscription_id);
            } else {
                self.replay_registry.remove(subscription_id);
            }
        } else {
            self.replay_registry.touch(subscription_id, Instant::now());
            if let Some(entry) = self.replay_registry.entries.get_mut(&subscription_id) {
                Self::bind_live_preserving_admitted(entry, output_id, through_sequence);
            }
        }
        Ok(QueryOutcome::Ok(QueryResult::EventReplayPage {
            subscription_id,
            page,
        }))
    }

    /// Preserve an existing live admitted cursor on the same output. When the
    /// output identity changes, cancel the old stream and attach a fresh live
    /// tail on the new output from the frozen baseline. Attach a reconnecting
    /// output at the frozen baseline when no live binding remains.
    fn bind_live_preserving_admitted(
        entry: &mut EventReplayRegistryEntry,
        output_id: Option<ConnectionOutputId>,
        frozen_through: u64,
    ) {
        let Some(output_id) = output_id else {
            return;
        };
        match entry.live.as_ref().map(|live| live.output_id == output_id) {
            Some(true) => {
                // Keep last_admitted_sequence / stream progress intact.
            }
            Some(false) => {
                if let Some(old) = entry.live.take() {
                    old.stream.cancel();
                }
                entry.live = Some(LiveTail::new(output_id, frozen_through));
            }
            None => {
                entry.live = Some(LiveTail::new(output_id, frozen_through));
            }
        }
    }

    fn serve_release_event_replay(
        &mut self,
        requester: ClientId,
        subscription_id: SubscriptionId,
    ) -> QueryOutcome {
        let now = Instant::now();
        self.replay_registry.reap_idle(now);
        match self.replay_registry.entries.get(&subscription_id) {
            None => QueryOutcome::Ok(QueryResult::EventReplayReleased { subscription_id }),
            Some(entry) if entry.owner != requester => QueryOutcome::Err(QueryError::Unauthorized),
            Some(_) => {
                // remove() cancels the live stream generation so queued durables
                // for this subscription are skipped after the release reply.
                self.replay_registry.remove(subscription_id);
                QueryOutcome::Ok(QueryResult::EventReplayReleased { subscription_id })
            }
        }
    }

    fn serve_open_artifact_content(
        &mut self,
        negotiated: NegotiatedParameters,
        task_id: TaskId,
        artifact_id: ArtifactId,
    ) -> Result<QueryOutcome, IpcError> {
        let now = Instant::now();
        self.artifact_content_registry.reap(now);
        let limits = page_limits_from_negotiated(negotiated)?;
        let session = match self.bus.begin_artifact_content(
            negotiated.client_id,
            task_id,
            artifact_id,
            limits,
            negotiated.limits.max_reassembled_message_bytes,
            negotiated.limits.max_physical_frame_bytes,
        ) {
            Ok(session) => session,
            Err(error) => return map_artifact_content_error(error),
        };
        let subscription_id = session.subscription_id();
        let page = match session.page(None) {
            Ok(page) => page,
            Err(error) => return map_artifact_content_error(error),
        };
        self.artifact_content_registry
            .insert(session, Instant::now());
        Ok(QueryOutcome::Ok(QueryResult::ArtifactContentPage {
            subscription_id,
            page,
        }))
    }

    fn serve_continue_artifact_content(
        &mut self,
        negotiated: NegotiatedParameters,
        task_id: TaskId,
        subscription_id: SubscriptionId,
        resume_cursor: Vec<u8>,
    ) -> Result<QueryOutcome, IpcError> {
        let now = Instant::now();
        self.artifact_content_registry.reap(now);
        let limits = page_limits_from_negotiated(negotiated)?;
        let session = match self.artifact_content_registry.get(
            subscription_id,
            negotiated.client_id,
            task_id,
            limits,
            negotiated.limits.max_reassembled_message_bytes,
            negotiated.limits.max_physical_frame_bytes,
            now,
        ) {
            Ok(session) => session,
            Err(error) => return map_artifact_content_error(error),
        };
        let page = match session.page(Some(resume_cursor.as_slice())) {
            Ok(page) => page,
            Err(error) => {
                // Cursor/shape failures leave a valid retained session intact.
                return map_artifact_content_error(error);
            }
        };
        self.artifact_content_registry
            .touch(subscription_id, Instant::now());
        Ok(QueryOutcome::Ok(QueryResult::ArtifactContentPage {
            subscription_id,
            page,
        }))
    }

    fn serve_release_artifact_content(
        &mut self,
        requester: ClientId,
        task_id: TaskId,
        subscription_id: SubscriptionId,
    ) -> Result<QueryOutcome, IpcError> {
        let now = Instant::now();
        self.artifact_content_registry.reap(now);
        match self
            .artifact_content_registry
            .release(subscription_id, requester, task_id)
        {
            Ok(()) => Ok(QueryOutcome::Ok(QueryResult::ArtifactContentReleased {
                subscription_id,
            })),
            Err(error) => map_artifact_content_error(error),
        }
    }

    fn fan_out_live_durable_events(&mut self) {
        let subscription_ids = self
            .replay_registry
            .entries
            .iter()
            .filter_map(|(id, entry)| entry.live.as_ref().map(|_| *id))
            .collect::<Vec<_>>();
        for subscription_id in subscription_ids {
            self.catch_up_subscription(subscription_id);
        }
    }

    fn catch_up_subscription(&mut self, subscription_id: SubscriptionId) {
        let (output_id, mut after_sequence, limits, stream) = {
            let Some(entry) = self.replay_registry.entries.get(&subscription_id) else {
                return;
            };
            let Some(live) = entry.live.as_ref() else {
                return;
            };
            (
                live.output_id,
                live.last_admitted_sequence,
                entry.limits,
                Arc::clone(&live.stream),
            )
        };
        let Some(output) = self.outputs.get(&output_id).cloned() else {
            self.replay_registry.remove(subscription_id);
            return;
        };
        if output.is_shutdown_requested() {
            self.replay_registry.remove(subscription_id);
            return;
        }

        loop {
            let session = match self.bus.begin_event_replay(after_sequence, limits) {
                Ok(session) => session,
                Err(error) => {
                    self.fail_live_replay(subscription_id, &output, &stream, after_sequence, error);
                    return;
                }
            };
            let page = match session.page(None) {
                Ok(page) => page,
                Err(error) => {
                    drop(session);
                    self.fail_live_replay(subscription_id, &output, &stream, after_sequence, error);
                    return;
                }
            };
            drop(session);

            if page.events.is_empty() {
                return;
            }

            let newest_sequence = page.through_sequence;
            for event in page.events {
                let sequence = event.sequence;
                match output.try_enqueue_durable_event(
                    subscription_id,
                    event,
                    &stream,
                    newest_sequence,
                ) {
                    DurableAdmitResult::Admitted => {
                        after_sequence = sequence;
                        if let Some(entry) = self.replay_registry.entries.get_mut(&subscription_id)
                        {
                            if let Some(live) = entry.live.as_mut() {
                                live.last_admitted_sequence = sequence;
                            }
                        }
                    }
                    DurableAdmitResult::ResyncAdmitted { .. } => {
                        self.replay_registry.remove(subscription_id);
                        return;
                    }
                    DurableAdmitResult::ShutdownRequested => {
                        self.replay_registry.remove(subscription_id);
                        return;
                    }
                }
            }

            if page.next_cursor.is_none() {
                return;
            }
        }
    }

    fn fail_live_replay(
        &mut self,
        subscription_id: SubscriptionId,
        output: &ConnectionOutputHandle,
        stream: &Arc<LiveStreamState>,
        last_admitted: u64,
        error: ReplayError,
    ) {
        let newest_sequence = newest_sequence_hint_from_replay_error(
            &error,
            last_admitted,
            stream.last_physically_written(),
        );
        let _ = output.force_live_resync(subscription_id, stream, newest_sequence);
        self.replay_registry.remove(subscription_id);
    }
}

/// Conservative newest-sequence hint for ResyncRequired after a live replay error.
fn newest_sequence_hint_from_replay_error(
    error: &ReplayError,
    last_admitted: u64,
    last_physically_written: u64,
) -> u64 {
    let floor = last_admitted.max(last_physically_written);
    match error {
        ReplayError::ReplayUnavailable {
            newest_sequence, ..
        } => (*newest_sequence).max(floor),
        ReplayError::InvalidRange {
            through_sequence, ..
        } => (*through_sequence).max(floor),
        ReplayError::PageItemTooLarge { sequence, .. } => (*sequence).max(floor),
        ReplayError::Store(_)
        | ReplayError::InvalidLimits(_)
        | ReplayError::EntropyUnavailable
        | ReplayError::InvalidCursor
        | ReplayError::CursorContextMismatch
        | ReplayError::PageEnvelopeTooLarge { .. } => floor,
    }
}

fn page_limits_from_negotiated(negotiated: NegotiatedParameters) -> Result<PageLimits, IpcError> {
    PageLimits::new(
        negotiated.limits.max_page_items,
        negotiated.limits.max_page_encoded_bytes,
    )
    .map_err(|_| IpcError::Unavailable)
}

fn map_store_error(error: StoreError) -> IpcError {
    match error {
        StoreError::Busy => IpcError::Busy,
        _ => IpcError::Unavailable,
    }
}

fn map_snapshot_error_transport(error: SnapshotError) -> IpcError {
    match error {
        SnapshotError::Store(StoreError::Busy) => IpcError::Busy,
        SnapshotError::InvalidCursor | SnapshotError::CursorContextMismatch => {
            // Open path should not produce cursor errors; treat as unavailable.
            IpcError::Unavailable
        }
        _ => IpcError::Unavailable,
    }
}

fn map_snapshot_error(error: SnapshotError) -> Result<QueryOutcome, IpcError> {
    match error {
        SnapshotError::InvalidCursor | SnapshotError::CursorContextMismatch => {
            Ok(QueryOutcome::Err(QueryError::InvalidRequest))
        }
        SnapshotError::Store(StoreError::Busy) => Err(IpcError::Busy),
        SnapshotError::Store(_)
        | SnapshotError::InvalidLimits(_)
        | SnapshotError::EntropyUnavailable
        | SnapshotError::PageEnvelopeTooLarge { .. }
        | SnapshotError::PageItemTooLarge { .. } => Err(IpcError::Unavailable),
    }
}

fn map_replay_error(error: ReplayError) -> Result<QueryOutcome, IpcError> {
    match error {
        ReplayError::ReplayUnavailable {
            oldest_sequence,
            newest_sequence,
        } => Ok(QueryOutcome::Err(QueryError::ReplayUnavailable {
            oldest_sequence,
            newest_sequence,
        })),
        ReplayError::InvalidRange { .. }
        | ReplayError::InvalidCursor
        | ReplayError::CursorContextMismatch => Ok(QueryOutcome::Err(QueryError::InvalidRequest)),
        ReplayError::Store(StoreError::Busy) => Err(IpcError::Busy),
        ReplayError::Store(_)
        | ReplayError::InvalidLimits(_)
        | ReplayError::EntropyUnavailable
        | ReplayError::PageEnvelopeTooLarge { .. }
        | ReplayError::PageItemTooLarge { .. } => Err(IpcError::Unavailable),
    }
}

fn map_artifact_content_error(error: ArtifactContentError) -> Result<QueryOutcome, IpcError> {
    match error {
        ArtifactContentError::NotFound => Ok(QueryOutcome::Err(QueryError::NotFound)),
        ArtifactContentError::Unauthorized => Ok(QueryOutcome::Err(QueryError::Unauthorized)),
        ArtifactContentError::InvalidRequest
        | ArtifactContentError::InvalidCursor
        | ArtifactContentError::CursorContextMismatch
        | ArtifactContentError::ContentDigestMismatch
        | ArtifactContentError::BodyTooLarge { .. } => {
            Ok(QueryOutcome::Err(QueryError::InvalidRequest))
        }
        ArtifactContentError::Store(StoreError::Busy) => Err(IpcError::Busy),
        ArtifactContentError::Store(_)
        | ArtifactContentError::InvalidLimits(_)
        | ArtifactContentError::EntropyUnavailable
        | ArtifactContentError::PageEnvelopeTooLarge { .. } => Err(IpcError::Unavailable),
    }
}

/// Authenticated client_id check plus CommandBus execute/query dispatch.
///
/// Used by the exclusive [`super::ipc::HostConnection::serve_request`]
/// compatibility path. Registry-backed snapshot and event-replay queries are
/// unsupported here; the single executor owns those registries.
///
/// `capabilities` are the negotiated grant set from Hello; capability-gated
/// bus queries (currently [`Query::InspectHostQuit`]) fail closed here the
/// same way [`HostRequestExecutor`] does.
pub(crate) fn dispatch_authenticated_request(
    authenticated_client_id: ClientId,
    capabilities: CapabilitySet,
    bus: &mut CommandBus,
    request: ClientRequest,
) -> Result<ServerMessage, IpcError> {
    match request {
        ClientRequest::Command(envelope) => {
            if envelope.client_id != authenticated_client_id {
                return Err(IpcError::Unauthorized);
            }
            if matches!(envelope.command, Command::ConfirmHostQuit(_))
                && !capabilities.contains(Capability::HostShutdown)
            {
                return Err(IpcError::UnsupportedCapability);
            }
            let receipt = bus.execute(envelope).map_err(map_store_error)?;
            Ok(ServerMessage::CommandReceipt(receipt))
        }
        ClientRequest::Query(envelope) => {
            if envelope.client_id != authenticated_client_id {
                return Err(IpcError::Unauthorized);
            }
            match &envelope.query {
                Query::SnapshotPage { .. }
                | Query::ReleaseSnapshot { .. }
                | Query::OpenEventReplay { .. }
                | Query::ContinueEventReplay { .. }
                | Query::ReleaseEventReplay { .. }
                | Query::OpenArtifactContent { .. }
                | Query::ContinueArtifactContent { .. }
                | Query::ReleaseArtifactContent { .. } => {
                    return Ok(ServerMessage::QueryReply(QueryReply {
                        request_id: envelope.request_id,
                        outcome: QueryOutcome::Err(QueryError::UnsupportedCapability),
                    }));
                }
                Query::InspectHostQuit => {
                    if envelope.task_id.is_some() {
                        return Ok(ServerMessage::QueryReply(QueryReply {
                            request_id: envelope.request_id,
                            outcome: QueryOutcome::Err(QueryError::InvalidRequest),
                        }));
                    }
                    if !capabilities.contains(Capability::HostShutdown) {
                        return Ok(ServerMessage::QueryReply(QueryReply {
                            request_id: envelope.request_id,
                            outcome: QueryOutcome::Err(QueryError::UnsupportedCapability),
                        }));
                    }
                }
                Query::OperationStatus { .. } | Query::TaskSnapshot => {}
            }
            let reply = bus.query(envelope).map_err(map_store_error)?;
            Ok(ServerMessage::QueryReply(reply))
        }
        ClientRequest::Detach(_) => Err(IpcError::Unavailable),
    }
}

/// Stable id for one duplex connection's executor-facing output handle.
///
/// Production registrations use the wire [`ServerHello::connection_id`] so host
/// and client share one identity. Unit tests may generate ids via [`Self::new`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct ConnectionOutputId(Uuid);

impl ConnectionOutputId {
    /// Test constructor: allocate a fresh UUIDv7 identity.
    #[cfg(test)]
    pub(crate) fn new() -> Self {
        Self(Uuid::now_v7())
    }

    pub(crate) fn from_uuid(id: Uuid) -> Self {
        Self(id)
    }

    #[cfg(test)]
    pub(crate) fn as_uuid(self) -> Uuid {
        self.0
    }
}

/// Result of admitting one durable event onto a connection output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DurableAdmitResult {
    Admitted,
    ResyncAdmitted {
        last_delivered_sequence: u64,
        newest_sequence: u64,
    },
    ShutdownRequested,
}

/// Outcome observed when polling a [`PhysicalWriteAck`] without awaiting.
#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PhysicalWriteAckStatus {
    Pending,
    Succeeded,
    Aborted,
}

/// Per-frame, non-Clone wait handle for one successful physical write.
///
/// Success is reported only after [`PrioritizedOutbound::after_successful_write`].
/// Dropping the outbound acknowledger (encode/write/cancel/drop without success)
/// reports aborted.
#[derive(Debug)]
pub(crate) struct PhysicalWriteAck {
    rx: oneshot::Receiver<()>,
}

impl PhysicalWriteAck {
    pub(crate) async fn wait(self) -> Result<(), ()> {
        self.rx.await.map_err(|_| ())
    }

    #[cfg(test)]
    pub(crate) fn status(&mut self) -> PhysicalWriteAckStatus {
        match self.rx.try_recv() {
            Ok(()) => PhysicalWriteAckStatus::Succeeded,
            Err(oneshot::error::TryRecvError::Empty) => PhysicalWriteAckStatus::Pending,
            Err(oneshot::error::TryRecvError::Closed) => PhysicalWriteAckStatus::Aborted,
        }
    }
}

/// Private sender half; drop aborts the paired [`PhysicalWriteAck`].
struct PhysicalWriteAcknowledger {
    tx: oneshot::Sender<()>,
}

impl PhysicalWriteAcknowledger {
    fn pair() -> (PhysicalWriteAck, Self) {
        let (tx, rx) = oneshot::channel();
        (PhysicalWriteAck { rx }, Self { tx })
    }

    fn acknowledge(self) {
        let _ = self.tx.send(());
    }
}

/// Critical outbound keeps the owned semaphore permit alive until dropped after
/// the physical write returns (success or failure).
pub(crate) struct CriticalOutbound {
    message: ServerMessage,
    _permit: OwnedSemaphorePermit,
    /// Live resync only: finalize `last_delivered_sequence` immediately before
    /// encode/write so an earlier in-flight durable can advance the baseline.
    live_resync: Option<LiveResyncMaterialization>,
    /// Explicit detach: request connection shutdown only after the ack write.
    shutdown_after_successful_write: Option<ConnectionOutputHandle>,
    write_ack: Option<PhysicalWriteAcknowledger>,
}

struct LiveResyncMaterialization {
    stream: Arc<LiveStreamState>,
    newest_sequence_hint: u64,
}

impl CriticalOutbound {
    fn prepare_for_write(&mut self) {
        let Some(materialize) = self.live_resync.take() else {
            return;
        };
        let last_delivered_sequence = materialize.stream.last_physically_written();
        let newest_sequence = materialize
            .newest_sequence_hint
            .max(last_delivered_sequence);
        if let ServerMessage::ResyncRequired {
            last_delivered_sequence: last,
            newest_sequence: newest,
            ..
        } = &mut self.message
        {
            *last = last_delivered_sequence;
            *newest = newest_sequence;
        }
    }
}

/// Durable outbound carries a live-stream generation so cancel/resync can skip
/// already-queued events without poisoning unrelated subscriptions.
pub(crate) struct DurableOutbound {
    message: ServerMessage,
    stream: Arc<LiveStreamState>,
    generation: u64,
    sequence: u64,
}

impl DurableOutbound {
    fn is_current(&self) -> bool {
        self.generation == self.stream.current_generation()
    }

    fn commit_physical_write(self) {
        // Always record: generation only suppresses queued frames that never
        // started writing. An in-flight cancel must not erase a completed write.
        self.stream.record_physical_write(self.sequence);
    }
}

/// Writer-facing prioritized outbound with RAII admission lifetime.
pub(crate) enum PrioritizedOutbound {
    Critical(CriticalOutbound),
    Durable(DurableOutbound),
    Ephemeral(EphemeralOutbound),
}

impl PrioritizedOutbound {
    pub(crate) fn message(&self) -> &ServerMessage {
        match self {
            Self::Critical(outbound) => &outbound.message,
            Self::Durable(outbound) => &outbound.message,
            Self::Ephemeral(outbound) => outbound
                .message
                .as_ref()
                .expect("message() requires should_write"),
        }
    }

    pub(crate) fn should_write(&self) -> bool {
        match self {
            Self::Critical(_) => true,
            Self::Durable(outbound) => outbound.is_current(),
            Self::Ephemeral(outbound) => outbound.should_write(),
        }
    }

    /// Finalize any write-time fields (live ResyncRequired baseline) before encode.
    pub(crate) fn prepare_for_write(&mut self) {
        match self {
            Self::Critical(outbound) => outbound.prepare_for_write(),
            Self::Durable(_) | Self::Ephemeral(_) => {}
        }
    }

    pub(crate) fn after_successful_write(self) {
        match self {
            Self::Critical(outbound) => {
                if let Some(handle) = outbound.shutdown_after_successful_write {
                    handle.request_shutdown();
                }
                if let Some(ack) = outbound.write_ack {
                    ack.acknowledge();
                }
            }
            Self::Durable(outbound) => outbound.commit_physical_write(),
            Self::Ephemeral(mut outbound) => outbound.commit_successful_write(),
        }
    }
}

/// Cloneable materializer invoked only when the writer drains an ephemeral token.
pub(crate) type StreamMaterializer = Arc<dyn Fn() -> Option<StreamFrame> + Send + Sync + 'static>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EphemeralAdmitResult {
    Queued,
    Coalesced,
    StaleGeneration,
    CapacityDrop,
    ShutdownRequested,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum EphemeralPhase {
    Queued,
    InFlight {
        taken_generation: u64,
        taken_dirty_revision: u64,
    },
}

struct EphemeralSlot {
    generation: u64,
    dirty_revision: u64,
    materializer: StreamMaterializer,
    phase: EphemeralPhase,
}

struct EphemeralLaneInner {
    capacity: usize,
    slots: HashMap<StreamKey, EphemeralSlot>,
    pending: VecDeque<StreamKey>,
}

struct EphemeralControl {
    shutdown: bool,
    lane: EphemeralLaneInner,
}

impl EphemeralLaneInner {
    fn occupied(&self) -> usize {
        self.slots.len()
    }

    fn clear(&mut self) {
        self.slots.clear();
        self.pending.clear();
    }

    fn admit(
        &mut self,
        stream: StreamKey,
        generation: u64,
        materializer: StreamMaterializer,
    ) -> (EphemeralAdmitResult, bool) {
        if let Some(slot) = self.slots.get_mut(&stream) {
            if generation < slot.generation {
                return (EphemeralAdmitResult::StaleGeneration, false);
            }
            slot.generation = generation;
            slot.dirty_revision = slot.dirty_revision.saturating_add(1);
            slot.materializer = materializer;
            let wake = matches!(slot.phase, EphemeralPhase::Queued)
                && !self.pending.iter().any(|key| *key == stream);
            if wake {
                self.pending.push_back(stream);
            }
            return (EphemeralAdmitResult::Coalesced, wake);
        }
        if self.slots.len() >= self.capacity {
            return (EphemeralAdmitResult::CapacityDrop, false);
        }
        self.slots.insert(
            stream,
            EphemeralSlot {
                generation,
                dirty_revision: 1,
                materializer,
                phase: EphemeralPhase::Queued,
            },
        );
        self.pending.push_back(stream);
        (EphemeralAdmitResult::Queued, true)
    }

    fn take_pending(&mut self) -> Option<(StreamKey, u64, u64, StreamMaterializer)> {
        while let Some(stream) = self.pending.pop_front() {
            let Some(slot) = self.slots.get_mut(&stream) else {
                continue;
            };
            if !matches!(slot.phase, EphemeralPhase::Queued) {
                continue;
            }
            let taken_generation = slot.generation;
            let taken_dirty_revision = slot.dirty_revision;
            slot.phase = EphemeralPhase::InFlight {
                taken_generation,
                taken_dirty_revision,
            };
            return Some((
                stream,
                taken_generation,
                taken_dirty_revision,
                Arc::clone(&slot.materializer),
            ));
        }
        None
    }

    fn finish(
        &mut self,
        stream: StreamKey,
        taken_generation: u64,
        taken_dirty_revision: u64,
    ) -> bool {
        let Some(slot) = self.slots.get_mut(&stream) else {
            return false;
        };
        let EphemeralPhase::InFlight {
            taken_generation: phase_generation,
            taken_dirty_revision: phase_dirty,
        } = slot.phase
        else {
            return false;
        };
        if phase_generation != taken_generation || phase_dirty != taken_dirty_revision {
            return false;
        }
        if slot.dirty_revision > taken_dirty_revision {
            slot.phase = EphemeralPhase::Queued;
            if !self.pending.iter().any(|key| *key == stream) {
                self.pending.push_back(stream);
            }
            true
        } else {
            self.slots.remove(&stream);
            false
        }
    }
}

fn wake_ephemeral(tx: &mpsc::Sender<()>) -> Result<(), ()> {
    match tx.try_send(()) {
        Ok(()) => Ok(()),
        Err(mpsc::error::TrySendError::Full(_)) => Ok(()),
        Err(mpsc::error::TrySendError::Closed(_)) => Err(()),
    }
}

/// Ephemeral outbound token: materializes at drain time, frees/requeues on completion.
pub(crate) struct EphemeralOutbound {
    message: Option<ServerMessage>,
    stream: StreamKey,
    taken_generation: u64,
    taken_dirty_revision: u64,
    control: Arc<Mutex<EphemeralControl>>,
    wake_tx: mpsc::Sender<()>,
    shutdown: watch::Sender<bool>,
    completed: bool,
}

impl EphemeralOutbound {
    pub(crate) fn should_write(&self) -> bool {
        self.message.is_some()
    }

    pub(crate) fn message(&self) -> &ServerMessage {
        self.message
            .as_ref()
            .expect("message() requires should_write")
    }

    fn commit_successful_write(&mut self) {
        self.completed = true;
        self.release_or_requeue();
    }

    fn release_or_requeue(&self) {
        let requeue = {
            let mut control = self.control.lock().expect("ephemeral control");
            if control.shutdown {
                control.lane.clear();
                false
            } else {
                control.lane.finish(
                    self.stream,
                    self.taken_generation,
                    self.taken_dirty_revision,
                )
            }
        };
        if requeue && wake_ephemeral(&self.wake_tx).is_err() {
            let mut control = self.control.lock().expect("ephemeral control");
            control.shutdown = true;
            control.lane.clear();
            let _ = self.shutdown.send_replace(true);
        }
    }
}

impl Drop for EphemeralOutbound {
    fn drop(&mut self) {
        if !self.completed {
            self.release_or_requeue();
        }
    }
}

/// Dual-lane host→client output for one duplex connection.
#[derive(Clone)]
pub(crate) struct ConnectionOutputHandle {
    id: ConnectionOutputId,
    critical_slots: Arc<Semaphore>,
    critical_tx: mpsc::UnboundedSender<CriticalOutbound>,
    durable_tx: mpsc::Sender<DurableOutbound>,
    ephemeral: Arc<Mutex<EphemeralControl>>,
    ephemeral_wake_tx: mpsc::Sender<()>,
    shutdown: watch::Sender<bool>,
}

/// Writer-side receivers for one connection output.
pub(crate) struct ConnectionOutputPorts {
    critical_rx: mpsc::UnboundedReceiver<CriticalOutbound>,
    durable_rx: mpsc::Receiver<DurableOutbound>,
    ephemeral: Arc<Mutex<EphemeralControl>>,
    ephemeral_wake_rx: mpsc::Receiver<()>,
    ephemeral_wake_tx: mpsc::Sender<()>,
    shutdown: watch::Sender<bool>,
    shutdown_rx: watch::Receiver<bool>,
}

impl ConnectionOutputHandle {
    /// Allocate an output with a generated connection identity (unit tests).
    #[cfg(test)]
    pub(crate) fn new(
        critical_capacity: usize,
        durable_capacity: usize,
        ephemeral_capacity: usize,
    ) -> (Self, ConnectionOutputPorts) {
        Self::with_connection_id(
            ConnectionOutputId::new().as_uuid(),
            critical_capacity,
            durable_capacity,
            ephemeral_capacity,
        )
    }

    /// Allocate an output whose id is the wire `ServerHello.connection_id`.
    pub(crate) fn with_connection_id(
        connection_id: Uuid,
        critical_capacity: usize,
        durable_capacity: usize,
        ephemeral_capacity: usize,
    ) -> (Self, ConnectionOutputPorts) {
        let (critical_tx, critical_rx) = mpsc::unbounded_channel();
        let (durable_tx, durable_rx) = mpsc::channel(durable_capacity.max(1));
        let (ephemeral_wake_tx, ephemeral_wake_rx) = mpsc::channel(1);
        let (shutdown, shutdown_rx) = watch::channel(false);
        let ephemeral = Arc::new(Mutex::new(EphemeralControl {
            shutdown: false,
            lane: EphemeralLaneInner {
                capacity: ephemeral_capacity.max(1),
                slots: HashMap::new(),
                pending: VecDeque::new(),
            },
        }));
        let handle = Self {
            id: ConnectionOutputId::from_uuid(connection_id),
            critical_slots: Arc::new(Semaphore::new(critical_capacity.max(1))),
            critical_tx,
            durable_tx,
            ephemeral: Arc::clone(&ephemeral),
            ephemeral_wake_tx: ephemeral_wake_tx.clone(),
            shutdown: shutdown.clone(),
        };
        (
            handle,
            ConnectionOutputPorts {
                critical_rx,
                durable_rx,
                ephemeral,
                ephemeral_wake_rx,
                ephemeral_wake_tx,
                shutdown,
                shutdown_rx,
            },
        )
    }

    pub(crate) fn id(&self) -> ConnectionOutputId {
        self.id
    }

    pub(crate) fn subscribe_shutdown(&self) -> watch::Receiver<bool> {
        self.shutdown.subscribe()
    }

    pub(crate) fn is_shutdown_requested(&self) -> bool {
        let control = self.ephemeral.lock().expect("ephemeral control");
        control.shutdown || *self.shutdown.borrow()
    }

    pub(crate) fn request_shutdown(&self) {
        {
            let mut control = self.ephemeral.lock().expect("ephemeral control");
            control.shutdown = true;
            control.lane.clear();
        }
        let _ = self.shutdown.send_replace(true);
        let _ = wake_ephemeral(&self.ephemeral_wake_tx);
    }

    #[cfg(test)]
    pub(crate) fn critical_permits_available(&self) -> usize {
        self.critical_slots.available_permits()
    }

    #[cfg(test)]
    pub(crate) fn ephemeral_slots_occupied(&self) -> usize {
        self.ephemeral
            .lock()
            .expect("ephemeral control")
            .lane
            .occupied()
    }

    #[cfg(test)]
    pub(crate) fn ephemeral_pending_len(&self) -> usize {
        self.ephemeral
            .lock()
            .expect("ephemeral control")
            .lane
            .pending
            .len()
    }

    #[cfg(test)]
    pub(crate) fn registration_guard_for_test(&self) -> ConnectionOutputRegistration {
        ConnectionOutputRegistration {
            id: self.id,
            output: self.clone(),
            control_tx: {
                let (tx, _rx) = mpsc::channel(1);
                tx
            },
        }
    }

    pub(crate) fn try_enqueue_critical(&self, message: ServerMessage) -> Result<(), IpcError> {
        self.try_enqueue_critical_outbound(message, None, false, false)
            .map(|_| ())
    }

    /// Admit a critical message that requests shutdown only after a successful write.
    pub(crate) fn try_enqueue_critical_shutdown_after_write(
        &self,
        message: ServerMessage,
    ) -> Result<(), IpcError> {
        self.try_enqueue_critical_outbound(message, None, true, false)
            .map(|_| ())
    }

    /// Tracked critical admission: returns a [`PhysicalWriteAck`] while preserving
    /// synchronous nonblocking permit/channel admission.
    pub(crate) fn try_enqueue_critical_tracked(
        &self,
        message: ServerMessage,
    ) -> Result<PhysicalWriteAck, IpcError> {
        self.try_enqueue_critical_outbound(message, None, false, true)?
            .ok_or(IpcError::Unavailable)
    }

    fn try_enqueue_critical_outbound(
        &self,
        message: ServerMessage,
        live_resync: Option<LiveResyncMaterialization>,
        shutdown_after_successful_write: bool,
        tracked: bool,
    ) -> Result<Option<PhysicalWriteAck>, IpcError> {
        if self.is_shutdown_requested() {
            return Err(IpcError::Unavailable);
        }
        let permit = self
            .critical_slots
            .clone()
            .try_acquire_owned()
            .map_err(|_| {
                self.request_shutdown();
                IpcError::Unavailable
            })?;
        let shutdown_after_successful_write = shutdown_after_successful_write.then(|| self.clone());
        let (ack, write_ack) = if tracked {
            let (ack, acknowledger) = PhysicalWriteAcknowledger::pair();
            (Some(ack), Some(acknowledger))
        } else {
            (None, None)
        };
        self.critical_tx
            .send(CriticalOutbound {
                message,
                _permit: permit,
                live_resync,
                shutdown_after_successful_write,
                write_ack,
            })
            .map_err(|_| {
                self.request_shutdown();
                IpcError::Unavailable
            })?;
        Ok(ack)
    }

    /// Cancel the live stream generation and attempt one critical ResyncRequired.
    ///
    /// The provisional baseline is snapshotted for admission results; the writer
    /// finalizes `last_delivered_sequence` via [`PrioritizedOutbound::prepare_for_write`]
    /// immediately before encoding so an in-flight durable can advance it.
    pub(crate) fn force_live_resync(
        &self,
        subscription_id: SubscriptionId,
        stream: &Arc<LiveStreamState>,
        newest_sequence: u64,
    ) -> DurableAdmitResult {
        if self.is_shutdown_requested() {
            return DurableAdmitResult::ShutdownRequested;
        }
        stream.cancel();
        let last_delivered_sequence = stream.last_physically_written();
        let newest_sequence = newest_sequence.max(last_delivered_sequence);
        let resync = ServerMessage::ResyncRequired {
            subscription_id,
            last_delivered_sequence,
            newest_sequence,
        };
        match self.try_enqueue_critical_outbound(
            resync,
            Some(LiveResyncMaterialization {
                stream: Arc::clone(stream),
                newest_sequence_hint: newest_sequence,
            }),
            false,
            false,
        ) {
            Ok(_) => DurableAdmitResult::ResyncAdmitted {
                last_delivered_sequence,
                newest_sequence,
            },
            Err(_) => DurableAdmitResult::ShutdownRequested,
        }
    }

    pub(crate) fn try_enqueue_durable_event(
        &self,
        subscription_id: SubscriptionId,
        event: crate::domain::event::DomainEvent,
        stream: &Arc<LiveStreamState>,
        newest_sequence: u64,
    ) -> DurableAdmitResult {
        if self.is_shutdown_requested() {
            return DurableAdmitResult::ShutdownRequested;
        }
        let sequence = event.sequence;
        let generation = stream.current_generation();
        let outbound = DurableOutbound {
            message: ServerMessage::DurableEvent {
                subscription_id,
                event,
            },
            stream: Arc::clone(stream),
            generation,
            sequence,
        };
        match self.durable_tx.try_send(outbound) {
            Ok(()) => DurableAdmitResult::Admitted,
            Err(mpsc::error::TrySendError::Full(_)) | Err(mpsc::error::TrySendError::Closed(_)) => {
                self.force_live_resync(subscription_id, stream, newest_sequence)
            }
        }
    }

    pub(crate) fn try_admit_ephemeral_stream(
        &self,
        stream: StreamKey,
        generation: u64,
        materializer: StreamMaterializer,
    ) -> EphemeralAdmitResult {
        let mut control = self.ephemeral.lock().expect("ephemeral control");
        if control.shutdown {
            return EphemeralAdmitResult::ShutdownRequested;
        }
        let (result, wake) = control.lane.admit(stream, generation, materializer);
        if wake {
            if wake_ephemeral(&self.ephemeral_wake_tx).is_err() {
                control.shutdown = true;
                control.lane.clear();
                drop(control);
                let _ = self.shutdown.send_replace(true);
                return EphemeralAdmitResult::ShutdownRequested;
            }
        }
        result
    }
}

impl ConnectionOutputPorts {
    fn shutdown_requested(&self) -> bool {
        let control = self.ephemeral.lock().expect("ephemeral control");
        control.shutdown || *self.shutdown_rx.borrow()
    }

    fn take_ephemeral_outbound(&mut self) -> Option<EphemeralOutbound> {
        let (stream, taken_generation, taken_dirty_revision, materializer) = {
            let mut control = self.ephemeral.lock().expect("ephemeral control");
            if control.shutdown {
                return None;
            }
            control.lane.take_pending()?
        };
        let frame = materializer();
        let message = match frame {
            Some(frame) if frame.stream == stream && frame.generation == taken_generation => {
                Some(ServerMessage::Stream(frame))
            }
            _ => None,
        };
        Some(EphemeralOutbound {
            message,
            stream,
            taken_generation,
            taken_dirty_revision,
            control: Arc::clone(&self.ephemeral),
            wake_tx: self.ephemeral_wake_tx.clone(),
            shutdown: self.shutdown.clone(),
            completed: false,
        })
    }

    /// Prefer critical, then durable, then ephemeral; never blocks.
    #[cfg(test)]
    pub(crate) fn try_recv_prioritized(&mut self) -> Option<PrioritizedOutbound> {
        if let Ok(outbound) = self.critical_rx.try_recv() {
            return Some(PrioritizedOutbound::Critical(outbound));
        }
        if let Ok(outbound) = self.durable_rx.try_recv() {
            return Some(PrioritizedOutbound::Durable(outbound));
        }
        self.take_ephemeral_outbound()
            .map(PrioritizedOutbound::Ephemeral)
    }

    /// Count coalesced wake tokens without permanently consuming them.
    #[cfg(test)]
    pub(crate) fn ephemeral_wake_pending_count(&mut self) -> usize {
        match self.ephemeral_wake_rx.try_recv() {
            Ok(()) => {
                let _ = self.ephemeral_wake_tx.try_send(());
                1
            }
            Err(_) => 0,
        }
    }

    /// Blocking receive that prefers critical then durable then ephemeral.
    pub(crate) async fn recv_prioritized(&mut self) -> Option<PrioritizedOutbound> {
        loop {
            if self.shutdown_requested() {
                return None;
            }
            if let Ok(outbound) = self.critical_rx.try_recv() {
                return Some(PrioritizedOutbound::Critical(outbound));
            }
            if let Ok(outbound) = self.durable_rx.try_recv() {
                return Some(PrioritizedOutbound::Durable(outbound));
            }
            if let Some(outbound) = self.take_ephemeral_outbound() {
                return Some(PrioritizedOutbound::Ephemeral(outbound));
            }
            tokio::select! {
                biased;
                changed = self.shutdown_rx.changed() => {
                    if changed.is_err() || self.shutdown_requested() {
                        return None;
                    }
                }
                critical = self.critical_rx.recv() => {
                    return critical.map(PrioritizedOutbound::Critical);
                }
                durable = self.durable_rx.recv() => {
                    return durable.map(PrioritizedOutbound::Durable);
                }
                wake = self.ephemeral_wake_rx.recv() => {
                    if wake.is_none() {
                        return None;
                    }
                }
            }
        }
    }

    /// Debug-only: drain critical traffic only; never consume durable or ephemeral.
    #[cfg(debug_assertions)]
    pub(crate) async fn recv_critical_only(&mut self) -> Option<PrioritizedOutbound> {
        loop {
            if self.shutdown_requested() {
                return None;
            }
            if let Ok(outbound) = self.critical_rx.try_recv() {
                return Some(PrioritizedOutbound::Critical(outbound));
            }
            tokio::select! {
                biased;
                changed = self.shutdown_rx.changed() => {
                    if changed.is_err() || self.shutdown_requested() {
                        return None;
                    }
                }
                critical = self.critical_rx.recv() => {
                    return critical.map(PrioritizedOutbound::Critical);
                }
            }
        }
    }
}

#[cfg(test)]
mod output_tests {
    use std::time::Duration;

    use super::{
        ConnectionOutputHandle, DuplexExecuteCompletion, DurableAdmitResult, EphemeralAdmitResult,
        HostRequestExecutor, HostRequestHandle, LiveStreamState, PhysicalWriteAckStatus,
        PrioritizedOutbound, StreamMaterializer,
    };
    use crate::domain::event::{DomainEvent, Event};
    use crate::domain::id::{EventId, RequestId, ResourceId, SubscriptionId};
    use crate::domain::query::{QueryOutcome, QueryReply};
    use crate::domain::ClientId;
    use crate::protocol::{
        ClientRequest, NegotiatedParameters, ServerMessage, StreamFrame, StreamKey,
        StreamPayloadKind,
    };
    use std::sync::Arc;

    fn sample_event(sequence: u64) -> DomainEvent {
        DomainEvent {
            id: EventId::new(),
            task_id: None,
            sequence,
            task_revision: None,
            occurred_at_ms: 1_725_000_000_000,
            payload: Event::TaskRenamed {
                title: format!("seq-{sequence}"),
            },
        }
    }

    fn sample_reply() -> ServerMessage {
        ServerMessage::QueryReply(QueryReply {
            request_id: RequestId::new(),
            outcome: QueryOutcome::Err(crate::domain::query::QueryError::NotFound),
        })
    }

    #[cfg(debug_assertions)]
    #[tokio::test]
    async fn critical_only_receiver_never_consumes_durable_or_ephemeral() {
        let subscription_id = SubscriptionId::new();
        let stream = LiveStreamState::new(0);
        let (handle, mut ports) = ConnectionOutputHandle::new(2, 1, 1);

        assert!(matches!(
            handle.try_enqueue_durable_event(subscription_id, sample_event(1), &stream, 1),
            DurableAdmitResult::Admitted
        ));
        let ephemeral_stream = sample_stream_key(0x70);
        assert!(matches!(
            handle.try_admit_ephemeral_stream(
                ephemeral_stream,
                1,
                Arc::new(move || Some(sample_stream_frame(ephemeral_stream, 1, 1, 7))),
            ),
            EphemeralAdmitResult::Queued | EphemeralAdmitResult::Coalesced
        ));
        handle
            .try_enqueue_critical(sample_reply())
            .expect("critical must admit while durable and ephemeral are held");

        let first = tokio::time::timeout(Duration::from_secs(1), ports.recv_critical_only())
            .await
            .expect("critical-only recv stayed bounded")
            .expect("critical outbound");
        assert!(matches!(first, PrioritizedOutbound::Critical(_)));

        let still_waiting =
            tokio::time::timeout(Duration::from_millis(50), ports.recv_critical_only()).await;
        assert!(
            still_waiting.is_err(),
            "critical-only must not surface durable or ephemeral while waiting"
        );
        let durable = ports
            .try_recv_prioritized()
            .expect("durable must remain queued");
        assert!(matches!(durable, PrioritizedOutbound::Durable(_)));
        let ephemeral = ports
            .try_recv_prioritized()
            .expect("ephemeral must remain queued");
        assert!(matches!(ephemeral, PrioritizedOutbound::Ephemeral(_)));
    }

    #[cfg(debug_assertions)]
    #[tokio::test]
    async fn critical_only_receiver_completes_none_on_shutdown() {
        let (handle, mut ports) = ConnectionOutputHandle::new(1, 1, 1);
        let pending = tokio::spawn(async move { ports.recv_critical_only().await });
        // Yield until the spawned receive is pending on critical/shutdown.
        for _ in 0..16 {
            if pending.is_finished() {
                break;
            }
            tokio::task::yield_now().await;
        }
        handle.request_shutdown();
        let outcome = tokio::time::timeout(Duration::from_secs(1), pending)
            .await
            .expect("critical-only shutdown wakeup stayed bounded")
            .expect("critical-only shutdown join");
        assert!(
            outcome.is_none(),
            "shutdown must complete critical-only receive with None"
        );
    }

    #[test]
    fn full_durable_lane_preserves_critical_admission_resync_and_connection_local_shutdown() {
        let subscription_id = SubscriptionId::new();
        let stream = LiveStreamState::new(0);
        // Two critical slots: one remains held as RAII through a simulated write
        // while overflow resync still admits on the second slot.
        let (alpha, mut alpha_ports) = ConnectionOutputHandle::new(2, 1, 1);
        let (beta, mut beta_ports) = ConnectionOutputHandle::new(1, 1, 1);

        assert!(matches!(
            alpha.try_enqueue_durable_event(subscription_id, sample_event(1), &stream, 2),
            DurableAdmitResult::Admitted
        ));

        alpha
            .try_enqueue_critical(sample_reply())
            .expect("full durable must not consume or block critical admission");
        let held = alpha_ports
            .try_recv_prioritized()
            .expect("critical reply must be dequeued as RAII outbound");
        assert!(matches!(held.message(), ServerMessage::QueryReply(_)));
        assert_eq!(
            alpha.critical_permits_available(),
            1,
            "held RAII outbound must keep its slot until dropped after write completion"
        );

        assert!(matches!(
            alpha.try_enqueue_durable_event(subscription_id, sample_event(2), &stream, 2),
            DurableAdmitResult::ResyncAdmitted {
                last_delivered_sequence: 0,
                newest_sequence: 2,
            }
        ));
        assert_eq!(alpha.critical_permits_available(), 0);

        assert!(
            alpha.try_enqueue_critical(sample_reply()).is_err(),
            "critical exhaustion must fail closed for this connection"
        );
        assert!(
            alpha.is_shutdown_requested(),
            "critical exhaustion must request only this connection's shutdown"
        );
        drop(held);

        let mut saw_resync = false;
        let mut saw_stale_durable = false;
        while let Some(outbound) = alpha_ports.try_recv_prioritized() {
            match outbound {
                PrioritizedOutbound::Critical(critical) => match &critical.message {
                    ServerMessage::ResyncRequired {
                        subscription_id: got_sub,
                        last_delivered_sequence,
                        newest_sequence,
                    } => {
                        assert_eq!(*got_sub, subscription_id);
                        assert_eq!(*last_delivered_sequence, 0);
                        assert_eq!(*newest_sequence, 2);
                        saw_resync = true;
                    }
                    other => panic!("expected ResyncRequired, got {other:?}"),
                },
                PrioritizedOutbound::Durable(durable) => {
                    assert!(!durable.is_current());
                    saw_stale_durable = true;
                }
                PrioritizedOutbound::Ephemeral(_) => {
                    panic!("unexpected ephemeral outbound in durable/critical test")
                }
            }
        }
        assert!(saw_resync);
        assert!(saw_stale_durable);

        beta.try_enqueue_critical(sample_reply())
            .expect("peer connection must remain writable");
        assert!(!beta.is_shutdown_requested());
        assert!(beta_ports.try_recv_prioritized().is_some());
    }

    #[test]
    fn durable_overflow_resync_uses_physical_baseline_and_suppresses_queued_event() {
        let subscription_id = SubscriptionId::new();
        let stream = LiveStreamState::new(10);
        let (handle, mut ports) = ConnectionOutputHandle::new(1, 1, 1);

        assert!(matches!(
            handle.try_enqueue_durable_event(subscription_id, sample_event(11), &stream, 12),
            DurableAdmitResult::Admitted
        ));
        assert!(matches!(
            handle.try_enqueue_durable_event(subscription_id, sample_event(12), &stream, 12),
            DurableAdmitResult::ResyncAdmitted {
                last_delivered_sequence: 10,
                newest_sequence: 12,
            }
        ));

        let mut saw_stale = false;
        let mut saw_resync = false;
        while let Some(outbound) = ports.try_recv_prioritized() {
            match outbound {
                PrioritizedOutbound::Durable(durable) => {
                    assert!(
                        !durable.is_current(),
                        "queued durable must be suppressed after resync cancel"
                    );
                    saw_stale = true;
                }
                PrioritizedOutbound::Critical(critical) => match &critical.message {
                    ServerMessage::ResyncRequired {
                        last_delivered_sequence,
                        newest_sequence,
                        ..
                    } => {
                        assert_eq!(*last_delivered_sequence, 10);
                        assert_eq!(*newest_sequence, 12);
                        saw_resync = true;
                    }
                    other => panic!("unexpected critical {other:?}"),
                },
                PrioritizedOutbound::Ephemeral(_) => {
                    panic!("unexpected ephemeral outbound in durable resync test")
                }
            }
        }
        assert!(
            saw_resync,
            "priority resync must be present on critical lane"
        );
        assert!(
            saw_stale,
            "stale queued durable must still be observable as cancelled"
        );
        assert_eq!(stream.last_physically_written(), 10);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn shutdown_wakes_idle_output_receive_without_sleep() {
        let (handle, mut ports) = ConnectionOutputHandle::new(1, 1, 1);
        let recv = tokio::spawn(async move { ports.recv_prioritized().await });
        tokio::task::yield_now().await;
        handle.request_shutdown();
        let result = tokio::time::timeout(Duration::from_millis(200), recv)
            .await
            .expect("shutdown must wake promptly")
            .expect("join");
        assert!(result.is_none());
    }

    #[test]
    fn shutdown_send_replace_retains_when_receivers_already_dropped() {
        let (handle, ports) = ConnectionOutputHandle::new(1, 1, 1);
        drop(ports);
        handle.request_shutdown();
        assert!(
            handle.is_shutdown_requested(),
            "send_replace must retain shutdown for executor reaper observation"
        );
    }

    #[test]
    fn force_live_resync_cancels_queued_frames_and_reports_physical_baseline() {
        let subscription_id = SubscriptionId::new();
        let stream = LiveStreamState::new(3);
        let (handle, mut ports) = ConnectionOutputHandle::new(1, 1, 1);
        assert!(matches!(
            handle.try_enqueue_durable_event(subscription_id, sample_event(4), &stream, 9),
            DurableAdmitResult::Admitted
        ));
        assert!(matches!(
            handle.force_live_resync(subscription_id, &stream, 9),
            DurableAdmitResult::ResyncAdmitted {
                last_delivered_sequence: 3,
                newest_sequence: 9,
            }
        ));

        let mut saw_stale = false;
        let mut saw_resync = false;
        while let Some(outbound) = ports.try_recv_prioritized() {
            match outbound {
                PrioritizedOutbound::Durable(durable) => {
                    assert!(!durable.is_current());
                    saw_stale = true;
                }
                PrioritizedOutbound::Critical(critical) => match &critical.message {
                    ServerMessage::ResyncRequired {
                        last_delivered_sequence,
                        newest_sequence,
                        ..
                    } => {
                        assert_eq!(*last_delivered_sequence, 3);
                        assert_eq!(*newest_sequence, 9);
                        saw_resync = true;
                    }
                    other => panic!("unexpected critical {other:?}"),
                },
                PrioritizedOutbound::Ephemeral(_) => {
                    panic!("unexpected ephemeral outbound in force_live_resync test")
                }
            }
        }
        assert!(saw_resync);
        assert!(saw_stale);
    }

    #[test]
    fn in_flight_durable_write_advances_prepared_resync_baseline() {
        let subscription_id = SubscriptionId::new();
        let stream = LiveStreamState::new(3);
        let (handle, mut ports) = ConnectionOutputHandle::new(1, 1, 1);

        assert!(matches!(
            handle.try_enqueue_durable_event(subscription_id, sample_event(4), &stream, 9),
            DurableAdmitResult::Admitted
        ));
        let in_flight = ports
            .try_recv_prioritized()
            .expect("dequeue durable as if write already started");
        let PrioritizedOutbound::Durable(durable) = in_flight else {
            panic!("expected durable outbound held in flight");
        };
        assert!(durable.is_current());

        assert!(matches!(
            handle.force_live_resync(subscription_id, &stream, 9),
            DurableAdmitResult::ResyncAdmitted {
                last_delivered_sequence: 3,
                newest_sequence: 9,
            }
        ));
        assert!(
            !durable.is_current(),
            "resync cancel must bump generation while durable is in flight"
        );

        // Physical write completed after cancel raced mid-flight.
        PrioritizedOutbound::Durable(durable).after_successful_write();
        assert_eq!(stream.last_physically_written(), 4);

        let mut resync = ports
            .try_recv_prioritized()
            .expect("critical resync must remain queued");
        resync.prepare_for_write();
        match resync.message() {
            ServerMessage::ResyncRequired {
                last_delivered_sequence,
                newest_sequence,
                ..
            } => {
                assert_eq!(*last_delivered_sequence, 4);
                assert_eq!(*newest_sequence, 9);
            }
            other => panic!("expected prepared ResyncRequired, got {other:?}"),
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn live_stream_wait_until_physically_written_returns_immediately_when_already_advanced() {
        let stream = LiveStreamState::new(10);
        assert_eq!(stream.last_physically_written(), 10);
        tokio::time::timeout(
            Duration::from_millis(50),
            stream.wait_until_physically_written(10),
        )
        .await
        .expect("already-advanced wait must complete without blocking");
        tokio::time::timeout(
            Duration::from_millis(50),
            stream.wait_until_physically_written(7),
        )
        .await
        .expect("lower target must also complete immediately");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn live_stream_wait_until_physically_written_observes_progress_without_lost_wakeup() {
        let stream = LiveStreamState::new(1);
        let waiter_stream = Arc::clone(&stream);
        let waiter = tokio::spawn(async move {
            waiter_stream.wait_until_physically_written(5).await;
        });
        // Yield so the waiter can register Notified::enable before progress.
        tokio::task::yield_now().await;
        stream.record_physical_write(3);
        assert_eq!(stream.last_physically_written(), 3);
        assert!(
            !waiter.is_finished(),
            "waiter must remain pending below target"
        );
        stream.record_physical_write(5);
        tokio::time::timeout(Duration::from_millis(200), waiter)
            .await
            .expect("progress notify must wake waiter promptly")
            .expect("join");
        assert_eq!(stream.last_physically_written(), 5);

        // Stale / equal sequences must not notify (high-water does not advance).
        let stalled = Arc::clone(&stream);
        let stalled_waiter = tokio::spawn(async move {
            stalled.wait_until_physically_written(6).await;
        });
        tokio::task::yield_now().await;
        stream.record_physical_write(4);
        stream.record_physical_write(5);
        assert!(
            !stalled_waiter.is_finished(),
            "non-advancing writes must not wake a higher target waiter"
        );
        stalled_waiter.abort();
    }

    fn sample_stream_key(tail: u8) -> StreamKey {
        StreamKey::from(
            ResourceId::from_bytes([
                0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00,
                0x00, tail,
            ])
            .expect("resource"),
        )
    }

    fn sample_stream_frame(
        stream: StreamKey,
        generation: u64,
        sequence: u64,
        marker: u8,
    ) -> StreamFrame {
        StreamFrame {
            subscription_id: SubscriptionId::new(),
            stream,
            generation,
            sequence,
            payload_kind: StreamPayloadKind::new(1).expect("kind"),
            schema_version: 1,
            payload: vec![marker],
        }
    }

    #[test]
    fn ephemeral_many_dirty_notifications_occupy_one_slot_and_first_drain_materializes_latest() {
        // Catches: repeated dirtiness for one stream must coalesce to a single
        // queued/in-flight slot and materialize only the latest state on drain.
        let stream = sample_stream_key(0x01);
        let (handle, mut ports) = ConnectionOutputHandle::new(1, 1, 1);
        let markers = std::sync::Arc::new(std::sync::atomic::AtomicU8::new(0));
        for marker in [1u8, 2, 3, 4, 5] {
            markers.store(marker, std::sync::atomic::Ordering::SeqCst);
            let markers = Arc::clone(&markers);
            let materializer: StreamMaterializer = Arc::new(move || {
                Some(sample_stream_frame(
                    stream,
                    1,
                    u64::from(markers.load(std::sync::atomic::Ordering::SeqCst)),
                    markers.load(std::sync::atomic::Ordering::SeqCst),
                ))
            });
            let result = handle.try_admit_ephemeral_stream(stream, 1, materializer);
            if marker == 1 {
                assert!(matches!(result, EphemeralAdmitResult::Queued));
            } else {
                assert!(matches!(result, EphemeralAdmitResult::Coalesced));
            }
        }
        assert_eq!(handle.ephemeral_slots_occupied(), 1);

        let outbound = ports
            .try_recv_prioritized()
            .expect("one coalesced ephemeral token must drain");
        let PrioritizedOutbound::Ephemeral(ephemeral) = outbound else {
            panic!("expected ephemeral outbound");
        };
        assert!(ephemeral.should_write());
        match ephemeral.message() {
            ServerMessage::Stream(frame) => {
                assert_eq!(frame.stream, stream);
                assert_eq!(frame.generation, 1);
                assert_eq!(frame.payload, vec![5]);
            }
            other => panic!("expected Stream frame, got {other:?}"),
        }
        assert!(ports.try_recv_prioritized().is_none());
        PrioritizedOutbound::Ephemeral(ephemeral).after_successful_write();
        assert_eq!(handle.ephemeral_slots_occupied(), 0);
    }

    #[test]
    fn ephemeral_dirty_during_in_flight_write_requeues_exactly_once_for_new_state() {
        // Catches: dirtiness while a materialized frame is in flight must requeue
        // exactly one token after successful write so the next drain regenerates.
        let stream = sample_stream_key(0x02);
        let (handle, mut ports) = ConnectionOutputHandle::new(1, 1, 1);
        let markers = std::sync::Arc::new(std::sync::atomic::AtomicU8::new(1));
        let markers_admit = Arc::clone(&markers);
        assert!(matches!(
            handle.try_admit_ephemeral_stream(
                stream,
                1,
                Arc::new(move || {
                    Some(sample_stream_frame(
                        stream,
                        1,
                        1,
                        markers_admit.load(std::sync::atomic::Ordering::SeqCst),
                    ))
                }),
            ),
            EphemeralAdmitResult::Queued
        ));
        let first = ports.try_recv_prioritized().expect("first ephemeral drain");
        let PrioritizedOutbound::Ephemeral(first) = first else {
            panic!("expected ephemeral");
        };
        assert!(
            matches!(first.message(), ServerMessage::Stream(frame) if frame.payload == vec![1])
        );

        markers.store(9, std::sync::atomic::Ordering::SeqCst);
        let markers_second = Arc::clone(&markers);
        assert!(matches!(
            handle.try_admit_ephemeral_stream(
                stream,
                1,
                Arc::new(move || {
                    Some(sample_stream_frame(
                        stream,
                        1,
                        2,
                        markers_second.load(std::sync::atomic::Ordering::SeqCst),
                    ))
                }),
            ),
            EphemeralAdmitResult::Coalesced
        ));
        assert!(
            ports.try_recv_prioritized().is_none(),
            "in-flight stream must not queue a second token before write completion"
        );
        assert_eq!(handle.ephemeral_slots_occupied(), 1);

        PrioritizedOutbound::Ephemeral(first).after_successful_write();
        let second = ports
            .try_recv_prioritized()
            .expect("exactly one requeue after in-flight dirtiness");
        let PrioritizedOutbound::Ephemeral(second) = second else {
            panic!("expected ephemeral requeue");
        };
        assert!(
            matches!(second.message(), ServerMessage::Stream(frame) if frame.payload == vec![9])
        );
        assert!(ports.try_recv_prioritized().is_none());
        PrioritizedOutbound::Ephemeral(second).after_successful_write();
        assert_eq!(handle.ephemeral_slots_occupied(), 0);
    }

    #[test]
    fn ephemeral_capacity_drops_overflow_without_blocking_critical_or_durable() {
        // Catches: distinct-stream capacity is hard-bounded; overflow drops only
        // ephemeral work while critical/durable remain admissible and prioritized.
        let (handle, mut ports) = ConnectionOutputHandle::new(1, 1, 1);
        let stream_a = sample_stream_key(0x10);
        let stream_b = sample_stream_key(0x11);
        assert!(matches!(
            handle.try_admit_ephemeral_stream(
                stream_a,
                1,
                Arc::new(move || Some(sample_stream_frame(stream_a, 1, 1, 1))),
            ),
            EphemeralAdmitResult::Queued
        ));
        assert!(matches!(
            handle.try_admit_ephemeral_stream(
                stream_b,
                1,
                Arc::new(move || Some(sample_stream_frame(stream_b, 1, 1, 2))),
            ),
            EphemeralAdmitResult::CapacityDrop
        ));
        assert_eq!(handle.ephemeral_slots_occupied(), 1);
        assert!(!handle.is_shutdown_requested());

        let stream = LiveStreamState::new(0);
        let subscription_id = SubscriptionId::new();
        assert!(matches!(
            handle.try_enqueue_durable_event(subscription_id, sample_event(1), &stream, 1),
            DurableAdmitResult::Admitted
        ));
        handle
            .try_enqueue_critical(sample_reply())
            .expect("ephemeral capacity must not consume critical/durable capacity");

        let first = ports.try_recv_prioritized().expect("critical first");
        assert!(matches!(first, PrioritizedOutbound::Critical(_)));
        let second = ports.try_recv_prioritized().expect("durable second");
        assert!(matches!(second, PrioritizedOutbound::Durable(_)));
        let third = ports.try_recv_prioritized().expect("ephemeral third");
        assert!(matches!(third, PrioritizedOutbound::Ephemeral(_)));
    }

    #[test]
    fn ephemeral_stale_generation_cannot_replace_newer_source() {
        // Catches: a lower generation must be rejected as stale and must not
        // replace a newer materializer/generation already occupying the slot.
        let stream = sample_stream_key(0x20);
        let (handle, mut ports) = ConnectionOutputHandle::new(1, 1, 1);
        assert!(matches!(
            handle.try_admit_ephemeral_stream(
                stream,
                5,
                Arc::new(move || Some(sample_stream_frame(stream, 5, 1, 5))),
            ),
            EphemeralAdmitResult::Queued
        ));
        assert!(matches!(
            handle.try_admit_ephemeral_stream(
                stream,
                4,
                Arc::new(move || Some(sample_stream_frame(stream, 4, 1, 4))),
            ),
            EphemeralAdmitResult::StaleGeneration
        ));
        let outbound = ports.try_recv_prioritized().expect("drain");
        let PrioritizedOutbound::Ephemeral(ephemeral) = outbound else {
            panic!("expected ephemeral");
        };
        match ephemeral.message() {
            ServerMessage::Stream(frame) => {
                assert_eq!(frame.generation, 5);
                assert_eq!(frame.payload, vec![5]);
            }
            other => panic!("expected stream, got {other:?}"),
        }
    }

    #[test]
    fn ephemeral_none_or_mismatched_materialization_emits_nothing_and_frees_capacity() {
        // Catches: None/mismatched/stale materialization must emit no frame,
        // must not busy-loop, and must release capacity consistently.
        let stream = sample_stream_key(0x30);
        let other = sample_stream_key(0x31);
        let (handle, mut ports) = ConnectionOutputHandle::new(1, 1, 2);

        assert!(matches!(
            handle.try_admit_ephemeral_stream(stream, 1, Arc::new(|| None)),
            EphemeralAdmitResult::Queued
        ));
        let none_out = ports.try_recv_prioritized().expect("drain none token");
        let PrioritizedOutbound::Ephemeral(none_out) = none_out else {
            panic!("expected ephemeral");
        };
        assert!(!none_out.should_write());
        drop(none_out);
        assert_eq!(handle.ephemeral_slots_occupied(), 0);
        assert!(ports.try_recv_prioritized().is_none());

        assert!(matches!(
            handle.try_admit_ephemeral_stream(
                stream,
                2,
                Arc::new(move || Some(sample_stream_frame(other, 2, 1, 1))),
            ),
            EphemeralAdmitResult::Queued
        ));
        let mismatch = ports.try_recv_prioritized().expect("drain mismatch");
        let PrioritizedOutbound::Ephemeral(mismatch) = mismatch else {
            panic!("expected ephemeral");
        };
        assert!(!mismatch.should_write());
        drop(mismatch);
        assert_eq!(handle.ephemeral_slots_occupied(), 0);

        assert!(matches!(
            handle.try_admit_ephemeral_stream(
                stream,
                3,
                Arc::new(move || Some(sample_stream_frame(stream, 99, 1, 1))),
            ),
            EphemeralAdmitResult::Queued
        ));
        let stale = ports.try_recv_prioritized().expect("drain stale frame");
        let PrioritizedOutbound::Ephemeral(stale) = stale else {
            panic!("expected ephemeral");
        };
        assert!(!stale.should_write());
        drop(stale);
        assert_eq!(handle.ephemeral_slots_occupied(), 0);
        assert!(ports.try_recv_prioritized().is_none());
    }

    #[test]
    fn ephemeral_wake_notification_stays_one_slot_under_repeated_admit_drain() {
        // Catches: unbounded ephemeral wake tokens must not accumulate across
        // repeated admissions and eager try_recv drains.
        let (handle, mut ports) = ConnectionOutputHandle::new(1, 1, 8);
        for tail in 0..8u8 {
            let stream = sample_stream_key(tail);
            assert!(matches!(
                handle.try_admit_ephemeral_stream(
                    stream,
                    1,
                    Arc::new(move || Some(sample_stream_frame(stream, 1, 1, tail))),
                ),
                EphemeralAdmitResult::Queued
            ));
        }
        let wake_pending = ports.ephemeral_wake_pending_count();
        assert!(
            wake_pending <= 1,
            "wake notifications must coalesce to at most one pending token, got {wake_pending}"
        );
        for _ in 0..8 {
            let outbound = ports
                .try_recv_prioritized()
                .expect("drain queued ephemeral");
            let PrioritizedOutbound::Ephemeral(ephemeral) = outbound else {
                panic!("expected ephemeral");
            };
            PrioritizedOutbound::Ephemeral(ephemeral).after_successful_write();
        }
        for _ in 0..64 {
            let stream = sample_stream_key(0x40);
            let _ = handle.try_admit_ephemeral_stream(
                stream,
                2,
                Arc::new(move || Some(sample_stream_frame(stream, 2, 1, 7))),
            );
        }
        assert!(handle.ephemeral_slots_occupied() <= 8);
        assert!(handle.ephemeral_pending_len() <= 8);
        let wake_pending = ports.ephemeral_wake_pending_count();
        assert!(
            wake_pending <= 1,
            "repeated coalesce admits must not grow the wake queue, got {wake_pending}"
        );
        while let Some(outbound) = ports.try_recv_prioritized() {
            if let PrioritizedOutbound::Ephemeral(ephemeral) = outbound {
                PrioritizedOutbound::Ephemeral(ephemeral).after_successful_write();
            }
        }
        assert_eq!(handle.ephemeral_slots_occupied(), 0);
        assert_eq!(handle.ephemeral_pending_len(), 0);
        assert!(ports.ephemeral_wake_pending_count() <= 1);
    }

    #[test]
    fn ephemeral_shutdown_linearizes_admission_and_clears_slots() {
        // Catches: shutdown and ephemeral admission must share one sync point so
        // post-shutdown admits return ShutdownRequested and leave zero slots.
        let (handle, _ports) = ConnectionOutputHandle::new(1, 1, 2);
        let stream = sample_stream_key(0x50);
        assert!(matches!(
            handle.try_admit_ephemeral_stream(
                stream,
                1,
                Arc::new(move || Some(sample_stream_frame(stream, 1, 1, 1))),
            ),
            EphemeralAdmitResult::Queued
        ));
        assert_eq!(handle.ephemeral_slots_occupied(), 1);

        let registration = handle.registration_guard_for_test();
        drop(registration);

        assert!(
            handle.is_shutdown_requested(),
            "registration drop must request synchronized shutdown"
        );
        assert_eq!(
            handle.ephemeral_slots_occupied(),
            0,
            "shutdown must clear ephemeral slots/pending"
        );
        assert_eq!(handle.ephemeral_pending_len(), 0);
        assert!(matches!(
            handle.try_admit_ephemeral_stream(
                sample_stream_key(0x51),
                1,
                Arc::new(|| Some(sample_stream_frame(sample_stream_key(0x51), 1, 1, 2))),
            ),
            EphemeralAdmitResult::ShutdownRequested
        ));
        assert_eq!(handle.ephemeral_slots_occupied(), 0);
    }

    #[test]
    fn ephemeral_closed_wake_on_in_flight_dirty_completion_requests_shutdown() {
        // Catches: finish-requeue after dirty in-flight must not strand a slot when
        // the wake receiver is already closed; Closed wake must synchronize shutdown.
        let (handle, mut ports) = ConnectionOutputHandle::new(1, 1, 1);
        let stream = sample_stream_key(0x60);
        assert!(matches!(
            handle.try_admit_ephemeral_stream(
                stream,
                1,
                Arc::new(move || Some(sample_stream_frame(stream, 1, 1, 1))),
            ),
            EphemeralAdmitResult::Queued
        ));
        let in_flight = ports
            .try_recv_prioritized()
            .expect("drain token into in-flight");
        let PrioritizedOutbound::Ephemeral(in_flight) = in_flight else {
            panic!("expected ephemeral");
        };
        assert!(matches!(
            handle.try_admit_ephemeral_stream(
                stream,
                1,
                Arc::new(move || Some(sample_stream_frame(stream, 1, 2, 2))),
            ),
            EphemeralAdmitResult::Coalesced
        ));
        drop(ports);

        PrioritizedOutbound::Ephemeral(in_flight).after_successful_write();

        assert!(
            handle.is_shutdown_requested(),
            "closed wake after requeue must request synchronized shutdown"
        );
        assert_eq!(handle.ephemeral_slots_occupied(), 0);
        assert_eq!(handle.ephemeral_pending_len(), 0);
        assert!(matches!(
            handle.try_admit_ephemeral_stream(
                sample_stream_key(0x61),
                1,
                Arc::new(|| Some(sample_stream_frame(sample_stream_key(0x61), 1, 1, 3))),
            ),
            EphemeralAdmitResult::ShutdownRequested
        ));
    }

    #[test]
    fn replay_error_newest_hint_uses_error_fields_and_admitted_floor() {
        use super::newest_sequence_hint_from_replay_error;
        use crate::kernel::ReplayError;

        assert_eq!(
            newest_sequence_hint_from_replay_error(
                &ReplayError::ReplayUnavailable {
                    oldest_sequence: 2,
                    newest_sequence: 11,
                },
                5,
                4
            ),
            11
        );
        assert_eq!(
            newest_sequence_hint_from_replay_error(
                &ReplayError::InvalidRange {
                    after_sequence: 20,
                    through_sequence: 7,
                },
                5,
                4
            ),
            7
        );
        assert_eq!(
            newest_sequence_hint_from_replay_error(
                &ReplayError::PageItemTooLarge {
                    sequence: 6,
                    encoded_bytes: 100,
                    max_encoded_bytes: 50,
                },
                5,
                4
            ),
            6
        );
        assert_eq!(
            newest_sequence_hint_from_replay_error(&ReplayError::InvalidCursor, 5, 8),
            8
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn detach_removes_exact_output_and_live_binding_before_ack_shutdown() {
        use super::{ConnectionOutputId, HostRequestExecutor, OutputInspection};
        use crate::domain::command::{Command, CommandEnvelope, CreateTaskIntent};
        use crate::domain::id::{CommandId, EnvironmentId, ProjectId, RequestId, TaskId};
        use crate::domain::query::{Query, QueryEnvelope};
        use crate::domain::task::{
            ReviewReadiness, TaskActivity, TaskAssignment, TaskAttention, TaskConnectivity,
            WorkspaceRef,
        };
        use crate::domain::ClientId;
        use crate::kernel::CommandBus;
        use crate::protocol::{
            Capability, CapabilitySet, ClientRequest, DetachRequest, FrameLimits,
            NegotiatedParameters, ProtocolVersion, ServerMessage,
        };
        use uuid::Uuid;

        let dir = tempfile::tempdir().expect("tempdir");
        let bus = CommandBus::open(&dir.path().join("detach.db")).expect("bus");
        let (requests, executor) = HostRequestExecutor::start(bus);

        let id_a = Uuid::from_bytes([
            0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0xd1,
        ]);
        let id_b = Uuid::from_bytes([
            0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0xd2,
        ]);
        let (out_a, mut ports_a) = ConnectionOutputHandle::with_connection_id(id_a, 2, 4, 1);
        let (out_b, _ports_b) = ConnectionOutputHandle::with_connection_id(id_b, 2, 4, 1);
        let shutdown_a = out_a.subscribe_shutdown();
        let reg_a = requests
            .register_output(out_a.clone())
            .await
            .expect("register a");
        let reg_b = requests
            .register_output(out_b.clone())
            .await
            .expect("register b");
        assert_eq!(reg_a.id().as_uuid(), id_a);
        assert_eq!(reg_b.id().as_uuid(), id_b);

        let client = ClientId::from_bytes([
            0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0xd3,
        ])
        .expect("client");
        let negotiated = NegotiatedParameters {
            version: ProtocolVersion::current(),
            client_id: client,
            capabilities: CapabilitySet::from_capabilities([
                Capability::PagedSnapshots,
                Capability::EventReplay,
                Capability::OperationSettlement,
                Capability::ExplicitDetach,
            ]),
            limits: FrameLimits::v1_default(),
        };
        let handle_a = requests.with_output(reg_a.id());
        let handle_b = requests.with_output(reg_b.id());

        let task_id = TaskId::from_bytes([
            0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0xd4,
        ])
        .expect("task");
        let create = CommandEnvelope {
            command_id: CommandId::from_bytes([
                0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00,
                0x00, 0xd5,
            ])
            .expect("command"),
            client_id: client,
            task_id: None,
            issued_at_ms: 1,
            expected_task_revision: None,
            command: Command::CreateTask(CreateTaskIntent {
                id: task_id,
                environment_id: EnvironmentId::from_bytes([
                    0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00, 0x00,
                    0x00, 0x00, 0xd6,
                ])
                .expect("env"),
                title: "detach live".into(),
                description: None,
                project_id: ProjectId::from_bytes([
                    0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00, 0x00,
                    0x00, 0x00, 0xd7,
                ])
                .expect("project"),
                workspace: WorkspaceRef::Main,
                assignment: TaskAssignment::LocalOwner,
                created_at_ms: 1,
                connectivity: TaskConnectivity::Connected,
                attention: TaskAttention::None,
                activity: TaskActivity::Idle,
                review_readiness: ReviewReadiness::NotReady,
            }),
        };
        handle_a
            .execute(negotiated, ClientRequest::Command(create))
            .await
            .expect("create task");

        let open = handle_a
            .execute(
                negotiated,
                ClientRequest::Query(QueryEnvelope {
                    request_id: RequestId::from_bytes([
                        0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00,
                        0x00, 0x00, 0x00, 0xd8,
                    ])
                    .expect("open req"),
                    client_id: client,
                    task_id: None,
                    query: Query::OpenEventReplay { after_sequence: 0 },
                }),
            )
            .await
            .expect("open replay");
        assert!(matches!(open, ServerMessage::QueryReply(_)));

        let before = requests
            .inspect_output(ConnectionOutputId::from_uuid(id_a))
            .await
            .expect("inspect before");
        assert_eq!(
            before,
            OutputInspection {
                registered: true,
                live_bound: true,
            }
        );

        let denied = handle_a
            .execute(
                NegotiatedParameters {
                    capabilities: CapabilitySet::from_capabilities([
                        Capability::PagedSnapshots,
                        Capability::EventReplay,
                        Capability::OperationSettlement,
                    ]),
                    ..negotiated
                },
                ClientRequest::Detach(DetachRequest {
                    request_id: RequestId::from_bytes([
                        0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00,
                        0x00, 0x00, 0x00, 0xcf,
                    ])
                    .expect("denied req"),
                    client_id: client,
                    connection_id: id_a,
                }),
            )
            .await;
        assert!(matches!(
            denied,
            Err(super::super::ipc::IpcError::UnsupportedCapability)
        ));
        assert_eq!(
            requests
                .inspect_output(ConnectionOutputId::from_uuid(id_a))
                .await
                .expect("inspect after deny"),
            before,
            "unsupported detach must leave output and live binding intact"
        );

        let sibling_before = requests
            .inspect_output(ConnectionOutputId::from_uuid(id_b))
            .await
            .expect("inspect b");
        assert!(sibling_before.registered);

        let request_id = RequestId::from_bytes([
            0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0xd9,
        ])
        .expect("detach req");
        let ack_message = handle_a
            .execute(
                negotiated,
                ClientRequest::Detach(DetachRequest {
                    request_id,
                    client_id: client,
                    connection_id: id_a,
                }),
            )
            .await
            .expect("detach");
        assert_eq!(
            ack_message,
            ServerMessage::Detached(crate::protocol::DetachAck {
                request_id,
                connection_id: id_a,
            })
        );

        let after = requests
            .inspect_output(ConnectionOutputId::from_uuid(id_a))
            .await
            .expect("inspect after");
        assert_eq!(
            after,
            OutputInspection {
                registered: false,
                live_bound: false,
            },
            "detach must remove output and live binding before ack write"
        );
        let sibling_after = requests
            .inspect_output(ConnectionOutputId::from_uuid(id_b))
            .await
            .expect("inspect b after");
        assert!(
            sibling_after.registered,
            "sibling output must remain usable"
        );

        assert!(
            !*shutdown_a.borrow(),
            "shutdown must not run before ack write"
        );
        out_a
            .try_enqueue_critical_shutdown_after_write(ack_message.clone())
            .expect("admit detach ack");
        assert!(!*shutdown_a.borrow());
        let outbound = ports_a
            .try_recv_prioritized()
            .expect("detach ack on critical lane");
        assert_eq!(outbound.message(), &ack_message);
        outbound.after_successful_write();
        assert!(
            *shutdown_a.borrow(),
            "successful ack write must request shutdown"
        );
        assert!(
            matches!(
                out_a.try_enqueue_critical(ServerMessage::QueryReply(
                    crate::domain::query::QueryReply {
                        request_id: RequestId::new(),
                        outcome: crate::domain::query::QueryOutcome::Err(
                            crate::domain::query::QueryError::NotFound
                        ),
                    }
                )),
                Err(super::super::ipc::IpcError::Unavailable)
            ),
            "no later critical traffic after detach shutdown"
        );

        let stale = handle_b
            .execute(
                negotiated,
                ClientRequest::Detach(DetachRequest {
                    request_id: RequestId::new(),
                    client_id: client,
                    connection_id: id_a,
                }),
            )
            .await;
        assert!(
            matches!(stale, Err(super::super::ipc::IpcError::Unauthorized)),
            "stale connection identity must not detach the sibling"
        );
        assert!(
            requests
                .inspect_output(ConnectionOutputId::from_uuid(id_b))
                .await
                .expect("b still registered")
                .registered
        );

        let wrong_client = ClientId::from_bytes([
            0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0xda,
        ])
        .expect("foreign");
        let wrong = handle_b
            .execute(
                negotiated,
                ClientRequest::Detach(DetachRequest {
                    request_id: RequestId::new(),
                    client_id: wrong_client,
                    connection_id: id_b,
                }),
            )
            .await;
        assert!(matches!(
            wrong,
            Err(super::super::ipc::IpcError::Unauthorized)
        ));
        assert!(
            requests
                .inspect_output(ConnectionOutputId::from_uuid(id_b))
                .await
                .expect("b survives wrong client")
                .registered
        );

        handle_b
            .execute(
                negotiated,
                ClientRequest::Query(QueryEnvelope {
                    request_id: RequestId::new(),
                    client_id: client,
                    task_id: Some(task_id),
                    query: Query::TaskSnapshot,
                }),
            )
            .await
            .expect("sibling remains usable after failed detaches");

        drop(reg_a);
        drop(reg_b);
        drop(requests);
        executor.abort();
        let _ = executor.await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn register_output_cancel_while_awaiting_ack_still_requests_shutdown() {
        use super::HostRequestExecutor;
        use crate::kernel::CommandBus;

        let dir = tempfile::tempdir().expect("tempdir");
        let bus = CommandBus::open(&dir.path().join("reg-cancel.db")).expect("bus");
        let (requests, executor) = HostRequestExecutor::start(bus);
        let (output, _ports) = ConnectionOutputHandle::new(1, 1, 1);
        let shutdown_rx = output.subscribe_shutdown();

        {
            let register = requests.register_output(output);
            tokio::pin!(register);
            // Poll registration first (biased) so the RAII guard is armed, then
            // cancel by dropping the future while send/ack may still be pending.
            tokio::select! {
                biased;
                result = &mut register => {
                    let registration = result.expect("register");
                    drop(registration);
                }
                _ = tokio::task::yield_now() => {
                    drop(register);
                }
            }
        }
        assert!(
            *shutdown_rx.borrow(),
            "cancel before ack observation must still request output shutdown"
        );

        drop(requests);
        executor.abort();
        let _ = executor.await;
    }

    #[test]
    fn dispatch_authenticated_inspect_host_quit_without_host_shutdown_rejects_before_bus_query() {
        use super::dispatch_authenticated_request;
        use crate::domain::id::{RequestId, TaskId};
        use crate::domain::query::{Query, QueryEnvelope, QueryError, QueryOutcome, QueryReply};
        use crate::domain::ClientId;
        use crate::kernel::CommandBus;
        use crate::protocol::{Capability, CapabilitySet, ClientRequest, ServerMessage};

        let dir = tempfile::tempdir().expect("tempdir");
        let mut bus = CommandBus::open(&dir.path().join("inspect-auth-gate.db")).expect("bus");
        let client = ClientId::from_bytes([
            0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0xe0,
        ])
        .expect("client");
        let request_id = RequestId::from_bytes([
            0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0xe1,
        ])
        .expect("request");
        let inspect_envelope = || {
            ClientRequest::Query(QueryEnvelope {
                request_id,
                client_id: client,
                task_id: None,
                query: Query::InspectHostQuit,
            })
        };

        // Control: with HostShutdown the compatibility path reaches bus.query and succeeds.
        let granted = dispatch_authenticated_request(
            client,
            CapabilitySet::from_capabilities([Capability::HostShutdown]),
            &mut bus,
            inspect_envelope(),
        )
        .expect("granted inspect transport");
        assert!(
            matches!(
                granted,
                ServerMessage::QueryReply(QueryReply {
                    request_id: rid,
                    outcome: QueryOutcome::Ok(_),
                }) if rid == request_id
            ),
            "HostShutdown must still allow InspectHostQuit on the auth path; got {granted:?}"
        );

        // Regression: missing HostShutdown must fail closed before bus.query.
        // The same envelope just succeeded above, so UnsupportedCapability here
        // cannot be a bus-level failure — the capability gate returned first.
        let denied = dispatch_authenticated_request(
            client,
            CapabilitySet::from_capabilities([Capability::OperationSettlement]),
            &mut bus,
            inspect_envelope(),
        )
        .expect("denied inspect transport");
        assert_eq!(
            denied,
            ServerMessage::QueryReply(QueryReply {
                request_id,
                outcome: QueryOutcome::Err(QueryError::UnsupportedCapability),
            })
        );

        // Global-only scope still wins before capability when task_id is set.
        let scoped = dispatch_authenticated_request(
            client,
            CapabilitySet::from_capabilities([Capability::OperationSettlement]),
            &mut bus,
            ClientRequest::Query(QueryEnvelope {
                request_id,
                client_id: client,
                task_id: Some(
                    TaskId::from_bytes([
                        0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00,
                        0x00, 0x00, 0x00, 0xe2,
                    ])
                    .expect("task"),
                ),
                query: Query::InspectHostQuit,
            }),
        )
        .expect("scoped inspect transport");
        assert_eq!(
            scoped,
            ServerMessage::QueryReply(QueryReply {
                request_id,
                outcome: QueryOutcome::Err(QueryError::InvalidRequest),
            })
        );
    }

    #[test]
    fn dispatch_authenticated_confirm_host_quit_capability_and_scope_gates() {
        use super::dispatch_authenticated_request;
        use crate::domain::command::{
            Command, CommandEnvelope, CommandReceipt, ConfirmHostQuitIntent, RejectionCode,
        };
        use crate::domain::id::{CommandId, TaskId};
        use crate::domain::ClientId;
        use crate::kernel::CommandBus;
        use crate::protocol::{Capability, CapabilitySet, ClientRequest, ServerMessage};

        let dir = tempfile::tempdir().expect("tempdir");
        let mut bus = CommandBus::open(&dir.path().join("confirm-auth-gate.db")).expect("bus");
        let client = ClientId::from_bytes([
            0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0xf0,
        ])
        .expect("client");
        let events_before: i64 = {
            let conn =
                rusqlite::Connection::open(dir.path().join("confirm-auth-gate.db")).expect("raw");
            conn.query_row("SELECT COUNT(*) FROM events", [], |row| row.get(0))
                .expect("count")
        };

        let denied = dispatch_authenticated_request(
            client,
            CapabilitySet::from_capabilities([Capability::OperationSettlement]),
            &mut bus,
            ClientRequest::Command(CommandEnvelope {
                command_id: CommandId::from_bytes([
                    0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00, 0x00,
                    0x00, 0x00, 0xf1,
                ])
                .expect("cmd"),
                client_id: client,
                task_id: None,
                issued_at_ms: 1_725_000_000_400,
                expected_task_revision: None,
                command: Command::ConfirmHostQuit(ConfirmHostQuitIntent {
                    inspection_id: 0,
                    allow_uninspected_worktrees: true,
                }),
            }),
        );
        assert!(matches!(
            denied,
            Err(crate::host::IpcError::UnsupportedCapability)
        ));
        let events_after_deny: i64 = {
            let conn =
                rusqlite::Connection::open(dir.path().join("confirm-auth-gate.db")).expect("raw");
            conn.query_row("SELECT COUNT(*) FROM events", [], |row| row.get(0))
                .expect("count")
        };
        assert_eq!(events_after_deny, events_before, "denied must not mutate");

        let inspection = bus.inspect_host_quit().expect("inspect");
        let granted = dispatch_authenticated_request(
            client,
            CapabilitySet::from_capabilities([Capability::HostShutdown]),
            &mut bus,
            ClientRequest::Command(CommandEnvelope {
                command_id: CommandId::from_bytes([
                    0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00, 0x00,
                    0x00, 0x00, 0xf2,
                ])
                .expect("cmd"),
                client_id: client,
                task_id: None,
                issued_at_ms: 1_725_000_000_401,
                expected_task_revision: None,
                command: Command::ConfirmHostQuit(ConfirmHostQuitIntent {
                    inspection_id: inspection.inspection_id,
                    allow_uninspected_worktrees: true,
                }),
            }),
        )
        .expect("granted confirm");
        assert!(
            matches!(
                granted,
                ServerMessage::CommandReceipt(CommandReceipt::Accepted { .. })
            ),
            "granted ConfirmHostQuit must Accept, got {granted:?}"
        );

        // Fresh Open store for task-scope invalidation without Closing interference.
        let mut scoped_bus =
            CommandBus::open(&dir.path().join("confirm-scope-gate.db")).expect("scoped bus");
        let scoped_inspection = scoped_bus.inspect_host_quit().expect("scoped inspect");
        let scoped = dispatch_authenticated_request(
            client,
            CapabilitySet::from_capabilities([Capability::HostShutdown]),
            &mut scoped_bus,
            ClientRequest::Command(CommandEnvelope {
                command_id: CommandId::from_bytes([
                    0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00, 0x00,
                    0x00, 0x00, 0xf3,
                ])
                .expect("cmd"),
                client_id: client,
                task_id: Some(
                    TaskId::from_bytes([
                        0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00,
                        0x00, 0x00, 0x00, 0xf4,
                    ])
                    .expect("task"),
                ),
                issued_at_ms: 1_725_000_000_402,
                expected_task_revision: None,
                command: Command::ConfirmHostQuit(ConfirmHostQuitIntent {
                    inspection_id: scoped_inspection.inspection_id,
                    allow_uninspected_worktrees: true,
                }),
            }),
        )
        .expect("scoped confirm transport");
        assert!(
            matches!(
                scoped,
                ServerMessage::CommandReceipt(CommandReceipt::Rejected {
                    code: RejectionCode::InvalidTransition,
                    ..
                })
            ),
            "task scope must InvalidTransition via CommandBus, got {scoped:?}"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn host_request_executor_confirm_host_quit_capability_gate() {
        use super::HostRequestExecutor;
        use crate::domain::command::{
            Command, CommandEnvelope, CommandReceipt, ConfirmHostQuitIntent,
        };
        use crate::domain::id::CommandId;
        use crate::domain::ClientId;
        use crate::kernel::CommandBus;
        use crate::protocol::{
            Capability, CapabilitySet, ClientRequest, FrameLimits, NegotiatedParameters,
            ProtocolVersion, ServerMessage,
        };

        let dir = tempfile::tempdir().expect("tempdir");
        let bus = CommandBus::open(&dir.path().join("confirm-exec-gate.db")).expect("bus");
        let (requests, executor) = HostRequestExecutor::start(bus);
        let client = ClientId::from_bytes([
            0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0xf5,
        ])
        .expect("client");
        let denied_negotiated = NegotiatedParameters {
            version: ProtocolVersion::current(),
            client_id: client,
            capabilities: CapabilitySet::from_capabilities([Capability::OperationSettlement]),
            limits: FrameLimits::v1_default(),
        };
        let denied = requests
            .execute(
                denied_negotiated,
                ClientRequest::Command(CommandEnvelope {
                    command_id: CommandId::from_bytes([
                        0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00,
                        0x00, 0x00, 0x00, 0xf6,
                    ])
                    .expect("cmd"),
                    client_id: client,
                    task_id: None,
                    issued_at_ms: 1_725_000_000_410,
                    expected_task_revision: None,
                    command: Command::ConfirmHostQuit(ConfirmHostQuitIntent {
                        inspection_id: 0,
                        allow_uninspected_worktrees: true,
                    }),
                }),
            )
            .await;
        assert!(matches!(
            denied,
            Err(crate::host::IpcError::UnsupportedCapability)
        ));

        let granted_negotiated = NegotiatedParameters {
            version: ProtocolVersion::current(),
            client_id: client,
            capabilities: CapabilitySet::from_capabilities([Capability::HostShutdown]),
            limits: FrameLimits::v1_default(),
        };
        // Empty host: COALESCE(MAX(sequence),0) == 0 matches inspection_id 0.
        let granted = requests
            .execute(
                granted_negotiated,
                ClientRequest::Command(CommandEnvelope {
                    command_id: CommandId::from_bytes([
                        0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00,
                        0x00, 0x00, 0x00, 0xf7,
                    ])
                    .expect("cmd"),
                    client_id: client,
                    task_id: None,
                    issued_at_ms: 1_725_000_000_411,
                    expected_task_revision: None,
                    command: Command::ConfirmHostQuit(ConfirmHostQuitIntent {
                        inspection_id: 0,
                        allow_uninspected_worktrees: true,
                    }),
                }),
            )
            .await
            .expect("granted confirm");
        assert!(
            matches!(
                granted,
                ServerMessage::CommandReceipt(CommandReceipt::Accepted { .. })
            ),
            "HostRequestExecutor granted ConfirmHostQuit must Accept, got {granted:?}"
        );

        drop(requests);
        executor.abort();
        let _ = executor.await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn host_cleanup_one_unit_per_maintenance_tick_with_two_registered_outputs() {
        use super::{ConnectionOutputId, HostRequestExecutor, OutputInspection};
        use crate::domain::command::{
            Command, CommandEnvelope, CommandReceipt, ConfirmHostQuitIntent,
        };
        use crate::domain::host::HostCleanupBranch;
        use crate::domain::id::CommandId;
        use crate::domain::ClientId;
        use crate::kernel::CommandBus;
        use crate::protocol::{
            Capability, CapabilitySet, ClientRequest, FrameLimits, NegotiatedParameters,
            ProtocolVersion, ServerMessage,
        };
        use rusqlite::Connection;
        use std::time::Duration;
        use uuid::Uuid;

        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("cleanup-tick.db");
        let bus = CommandBus::open(&db_path).expect("bus");
        let (requests, executor) = HostRequestExecutor::start_without_automatic_maintenance(bus);

        let id_a = Uuid::from_bytes([
            0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0xe1,
        ]);
        let id_b = Uuid::from_bytes([
            0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0xe2,
        ]);
        let (out_a, _ports_a) = ConnectionOutputHandle::with_connection_id(id_a, 2, 4, 1);
        let (out_b, _ports_b) = ConnectionOutputHandle::with_connection_id(id_b, 2, 4, 1);
        let _reg_a = requests.register_output(out_a).await.expect("register a");
        let _reg_b = requests.register_output(out_b).await.expect("register b");
        let output_id_a = ConnectionOutputId::from_uuid(id_a);
        let output_id_b = ConnectionOutputId::from_uuid(id_b);
        assert_ne!(output_id_a, output_id_b);

        let client = ClientId::from_bytes([
            0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0xe3,
        ])
        .expect("client");
        let negotiated = NegotiatedParameters {
            version: ProtocolVersion::current(),
            client_id: client,
            capabilities: CapabilitySet::from_capabilities([Capability::HostShutdown]),
            limits: FrameLimits::v1_default(),
        };
        let confirm = requests
            .execute(
                negotiated,
                ClientRequest::Command(CommandEnvelope {
                    command_id: CommandId::from_bytes([
                        0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00,
                        0x00, 0x00, 0x00, 0xe4,
                    ])
                    .expect("cmd"),
                    client_id: client,
                    task_id: None,
                    issued_at_ms: 1_725_000_000_500,
                    expected_task_revision: None,
                    command: Command::ConfirmHostQuit(ConfirmHostQuitIntent {
                        inspection_id: 0,
                        allow_uninspected_worktrees: true,
                    }),
                }),
            )
            .await
            .expect("confirm");
        assert!(matches!(
            confirm,
            ServerMessage::CommandReceipt(CommandReceipt::Accepted { .. })
        ));

        fn cleanup_branches(path: &std::path::Path) -> Vec<String> {
            let conn =
                Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
                    .expect("readonly");
            let mut stmt = conn
                .prepare(
                    "SELECT branch FROM host_cleanup_branches
                     ORDER BY
                       CASE branch
                         WHEN 'agent_sessions' THEN 0
                         WHEN 'resources' THEN 1
                         WHEN 'outstanding_effects' THEN 2
                         WHEN 'task_teardowns' THEN 3
                         ELSE 99
                       END",
                )
                .expect("prepare");
            stmt.query_map([], |row| row.get::<_, String>(0))
                .expect("query")
                .map(|row| row.expect("row"))
                .collect()
        }

        assert!(cleanup_branches(&db_path).is_empty());
        requests.run_maintenance_once().await.expect("tick 1");
        assert_eq!(
            cleanup_branches(&db_path),
            vec![HostCleanupBranch::AgentSessions.as_str().to_string()]
        );
        requests.run_maintenance_once().await.expect("tick 2");
        assert_eq!(
            cleanup_branches(&db_path),
            vec![
                HostCleanupBranch::AgentSessions.as_str().to_string(),
                HostCleanupBranch::Resources.as_str().to_string(),
            ]
        );
        requests.run_maintenance_once().await.expect("tick 3");
        requests.run_maintenance_once().await.expect("tick 4");
        assert_eq!(
            cleanup_branches(&db_path),
            HostCleanupBranch::ORDER
                .iter()
                .map(|branch| branch.as_str().to_string())
                .collect::<Vec<_>>()
        );
        requests.run_maintenance_once().await.expect("idle tick");
        assert_eq!(cleanup_branches(&db_path).len(), 4);

        // Wait longer than the production maintenance period without scheduling
        // automatic ticks; only explicit invocations may advance rows.
        tokio::time::sleep(super::SNAPSHOT_REAPER_PERIOD + Duration::from_millis(250)).await;
        assert_eq!(cleanup_branches(&db_path).len(), 4);

        let inspect_a = requests
            .inspect_output(output_id_a)
            .await
            .expect("inspect a");
        let inspect_b = requests
            .inspect_output(output_id_b)
            .await
            .expect("inspect b");
        assert_eq!(
            inspect_a,
            OutputInspection {
                registered: true,
                live_bound: false,
            }
        );
        assert_eq!(
            inspect_b,
            OutputInspection {
                registered: true,
                live_bound: false,
            }
        );

        drop(requests);
        executor.abort();
        let _ = executor.await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn host_cleanup_executor_maintenance_fans_out_cleanup_failed() {
        use super::{
            ConnectionOutputId, HostRequestExecutor, OutputInspection, PrioritizedOutbound,
        };
        use crate::domain::agent::{AgentRole, AgentSessionFacts, AgentSessionLifecycle};
        use crate::domain::command::{
            Command, CommandEnvelope, CommandReceipt, ConfirmHostQuitIntent, CreateTaskIntent,
        };
        use crate::domain::event::Event;
        use crate::domain::id::{
            AgentSessionId, CommandId, EnvironmentId, ProjectId, RequestId, TaskId,
        };
        use crate::domain::operation::OperationErrorCode;
        use crate::domain::query::{Query, QueryEnvelope};
        use crate::domain::task::{
            ReviewReadiness, TaskActivity, TaskAssignment, TaskAttention, TaskConnectivity,
            TaskLifecycle, WorkspaceRef,
        };
        use crate::domain::ClientId;
        use crate::kernel::CommandBus;
        use crate::protocol::{
            Capability, CapabilitySet, ClientRequest, FrameLimits, NegotiatedParameters,
            ProtocolVersion, ServerMessage,
        };
        use crate::providers::ProviderKind;
        use uuid::Uuid;

        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("cleanup-failed-fanout.db");
        {
            let mut bus = CommandBus::open(&db_path).expect("seed");
            let task = TaskId::from_bytes([
                0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00,
                0x00, 0x41,
            ])
            .expect("task");
            bus.execute(CommandEnvelope {
                command_id: CommandId::from_bytes([
                    0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00, 0x00,
                    0x00, 0x00, 0x42,
                ])
                .expect("create cmd"),
                client_id: ClientId::from_bytes([
                    0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00, 0x00,
                    0x00, 0x00, 0x20,
                ])
                .expect("client"),
                task_id: None,
                issued_at_ms: 1_725_000_000_100,
                expected_task_revision: None,
                command: Command::CreateTask(CreateTaskIntent {
                    id: task,
                    environment_id: EnvironmentId::from_bytes([
                        0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00,
                        0x00, 0x00, 0x00, 0x21,
                    ])
                    .expect("env"),
                    title: "cleanup failed fanout".into(),
                    description: None,
                    project_id: ProjectId::from_bytes([
                        0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00,
                        0x00, 0x00, 0x00, 0x22,
                    ])
                    .expect("project"),
                    workspace: WorkspaceRef::Main,
                    assignment: TaskAssignment::LocalOwner,
                    created_at_ms: 1_725_000_000_000,
                    connectivity: TaskConnectivity::Connected,
                    attention: TaskAttention::None,
                    activity: TaskActivity::Idle,
                    review_readiness: ReviewReadiness::NotReady,
                }),
            })
            .expect("create");
            bus.execute(CommandEnvelope {
                command_id: CommandId::from_bytes([
                    0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00, 0x00,
                    0x00, 0x00, 0x43,
                ])
                .expect("agent cmd"),
                client_id: ClientId::from_bytes([
                    0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00, 0x00,
                    0x00, 0x00, 0x20,
                ])
                .expect("client"),
                task_id: Some(task),
                issued_at_ms: 1_725_000_000_100,
                expected_task_revision: Some(1),
                command: Command::RegisterAgentSession {
                    agent: AgentSessionFacts {
                        id: AgentSessionId::from_bytes([
                            0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00,
                            0x00, 0x00, 0x00, 0xa1,
                        ])
                        .expect("agent"),
                        task_id: task,
                        role: AgentRole::Primary,
                        provider_kind: ProviderKind::ClaudeCode,
                        provider_session_id: Some(
                            "session-fanout".parse().expect("provider session"),
                        ),
                        lifecycle: AgentSessionLifecycle::Open,
                        runtime_generation: 0,
                        revision: 0,
                    },
                },
            })
            .expect("register agent");
        }

        let bus = CommandBus::open(&db_path).expect("bus");
        let (requests, executor) = HostRequestExecutor::start_without_automatic_maintenance(bus);
        let id = Uuid::from_bytes([
            0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0xf1,
        ]);
        let (out, mut ports) = ConnectionOutputHandle::with_connection_id(id, 4, 8, 1);
        let reg = requests.register_output(out).await.expect("register");
        let client = ClientId::from_bytes([
            0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0xf2,
        ])
        .expect("client");
        let negotiated = NegotiatedParameters {
            version: ProtocolVersion::current(),
            client_id: client,
            capabilities: CapabilitySet::from_capabilities([
                Capability::HostShutdown,
                Capability::EventReplay,
                Capability::PagedSnapshots,
            ]),
            limits: FrameLimits::v1_default(),
        };
        let handle = requests.with_output(reg.id());
        let open = handle
            .execute(
                negotiated.clone(),
                ClientRequest::Query(QueryEnvelope {
                    request_id: RequestId::from_bytes([
                        0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00,
                        0x00, 0x00, 0x00, 0xf4,
                    ])
                    .expect("req"),
                    client_id: client,
                    task_id: None,
                    query: Query::OpenEventReplay { after_sequence: 0 },
                }),
            )
            .await
            .expect("open replay");
        assert!(matches!(open, ServerMessage::QueryReply(_)));

        let inspect = handle
            .execute(
                negotiated.clone(),
                ClientRequest::Query(QueryEnvelope {
                    request_id: RequestId::from_bytes([
                        0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00,
                        0x00, 0x00, 0x00, 0xf5,
                    ])
                    .expect("inspect req"),
                    client_id: client,
                    task_id: None,
                    query: Query::InspectHostQuit,
                }),
            )
            .await
            .expect("inspect quit");
        let inspection_id = match inspect {
            ServerMessage::QueryReply(reply) => match reply.outcome {
                crate::domain::query::QueryOutcome::Ok(
                    crate::domain::query::QueryResult::HostQuitInspection { inspection },
                ) => inspection.inspection_id,
                other => panic!("expected HostQuitInspection, got {other:?}"),
            },
            other => panic!("expected QueryReply, got {other:?}"),
        };

        let confirm = handle
            .execute(
                negotiated,
                ClientRequest::Command(CommandEnvelope {
                    command_id: CommandId::from_bytes([
                        0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00,
                        0x00, 0x00, 0x00, 0xf3,
                    ])
                    .expect("cmd"),
                    client_id: client,
                    task_id: None,
                    issued_at_ms: 1_725_000_000_500,
                    expected_task_revision: None,
                    command: Command::ConfirmHostQuit(ConfirmHostQuitIntent {
                        inspection_id,
                        allow_uninspected_worktrees: true,
                    }),
                }),
            )
            .await
            .expect("confirm");
        let quit_op = match confirm {
            ServerMessage::CommandReceipt(CommandReceipt::Accepted { operation_id, .. }) => {
                operation_id
            }
            other => panic!("expected Accepted, got {other:?}"),
        };

        for _ in 0..4 {
            requests.run_maintenance_once().await.expect("branch tick");
        }
        while let Some(outbound) = ports.try_recv_prioritized() {
            let _ = outbound;
        }

        requests
            .run_maintenance_once()
            .await
            .expect("failure terminal tick");
        let mut saw_failed = false;
        while let Some(outbound) = ports.try_recv_prioritized() {
            if matches!(&outbound, PrioritizedOutbound::Durable(_)) {
                if let ServerMessage::DurableEvent { event, .. } = outbound.message() {
                    if let Event::OperationFailed(fact) = &event.payload {
                        assert_eq!(fact.operation_id, quit_op);
                        assert_eq!(fact.code, OperationErrorCode::CleanupFailed);
                        assert_eq!(fact.action_epoch, Some(1));
                        saw_failed = true;
                    }
                }
            }
        }
        assert!(
            saw_failed,
            "Failed terminalization must fan out OperationFailed"
        );

        let inspect = requests
            .inspect_output(ConnectionOutputId::from_uuid(id))
            .await
            .expect("inspect");
        assert_eq!(
            inspect,
            OutputInspection {
                registered: true,
                live_bound: true,
            }
        );

        requests
            .run_maintenance_once()
            .await
            .expect("post-terminal idle");
        assert!(
            ports.try_recv_prioritized().is_none(),
            "idempotent Idle must not invent additional durables"
        );

        drop(requests);
        executor.abort();
        let _ = executor.await;
        let _ = TaskLifecycle::Open;
    }

    fn host_shutdown_negotiated(client: ClientId) -> NegotiatedParameters {
        use crate::protocol::{Capability, CapabilitySet, FrameLimits, ProtocolVersion};
        NegotiatedParameters {
            version: ProtocolVersion::current(),
            client_id: client,
            capabilities: CapabilitySet::from_capabilities([Capability::HostShutdown]),
            limits: FrameLimits::v1_default(),
        }
    }

    fn inspect_quit_request(client: ClientId) -> ClientRequest {
        use crate::domain::id::RequestId;
        use crate::domain::query::{Query, QueryEnvelope};
        ClientRequest::Query(QueryEnvelope {
            request_id: RequestId::new(),
            client_id: client,
            task_id: None,
            query: Query::InspectHostQuit,
        })
    }

    fn confirm_quit_request(
        client: ClientId,
        command_id: crate::domain::id::CommandId,
        inspection_id: u64,
    ) -> ClientRequest {
        use crate::domain::command::{Command, CommandEnvelope, ConfirmHostQuitIntent};
        ClientRequest::Command(CommandEnvelope {
            command_id,
            client_id: client,
            task_id: None,
            issued_at_ms: 1_725_000_000_700,
            expected_task_revision: None,
            command: Command::ConfirmHostQuit(ConfirmHostQuitIntent {
                inspection_id,
                allow_uninspected_worktrees: true,
            }),
        })
    }

    async fn inspection_id_for(
        handle: &HostRequestHandle,
        negotiated: NegotiatedParameters,
        client: ClientId,
    ) -> u64 {
        match handle
            .execute(negotiated, inspect_quit_request(client))
            .await
            .expect("inspect")
        {
            ServerMessage::QueryReply(reply) => match reply.outcome {
                crate::domain::query::QueryOutcome::Ok(
                    crate::domain::query::QueryResult::HostQuitInspection { inspection },
                ) => inspection.inspection_id,
                other => panic!("expected HostQuitInspection, got {other:?}"),
            },
            other => panic!("expected QueryReply, got {other:?}"),
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn tracked_critical_ack_pending_until_after_successful_write_and_aborts_on_drop() {
        let (handle, mut ports) = ConnectionOutputHandle::new(2, 1, 1);
        let mut ack = handle
            .try_enqueue_critical_tracked(sample_reply())
            .expect("tracked critical admit");
        assert_eq!(ack.status(), PhysicalWriteAckStatus::Pending);
        let outbound = ports
            .try_recv_prioritized()
            .expect("dequeue tracked critical");
        assert_eq!(ack.status(), PhysicalWriteAckStatus::Pending);
        outbound.after_successful_write();
        assert!(ack.wait().await.is_ok());

        let mut aborted = handle
            .try_enqueue_critical_tracked(sample_reply())
            .expect("second tracked critical");
        drop(ports.try_recv_prioritized().expect("dequeue second"));
        assert_eq!(aborted.status(), PhysicalWriteAckStatus::Aborted);
    }

    #[test]
    fn tracked_critical_full_and_closed_fail_closed_without_ack() {
        let (handle, mut ports) = ConnectionOutputHandle::new(1, 1, 1);
        handle.try_enqueue_critical(sample_reply()).expect("fill");
        assert!(handle.try_enqueue_critical_tracked(sample_reply()).is_err());
        assert!(handle.is_shutdown_requested());
        let _ = ports.try_recv_prioritized();

        let (closed, ports) = ConnectionOutputHandle::new(1, 1, 1);
        drop(ports);
        closed.request_shutdown();
        assert!(closed.try_enqueue_critical_tracked(sample_reply()).is_err());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn ordinary_execute_accepted_confirm_host_quit_returns_server_message() {
        use crate::domain::command::CommandReceipt;
        use crate::domain::id::CommandId;
        use crate::kernel::CommandBus;

        let dir = tempfile::tempdir().expect("tempdir");
        let bus = CommandBus::open(&dir.path().join("ordinary-quit.db")).expect("bus");
        let (requests, executor) = HostRequestExecutor::start_without_automatic_maintenance(bus);
        let (out, mut ports) = ConnectionOutputHandle::new(4, 8, 1);
        let reg = requests.register_output(out).await.expect("register");
        let client = ClientId::new();
        let negotiated = host_shutdown_negotiated(client);
        let handle = requests.with_output(reg.id());
        let inspection_id = inspection_id_for(&handle, negotiated.clone(), client).await;

        let confirm = handle
            .execute(
                negotiated,
                confirm_quit_request(client, CommandId::new(), inspection_id),
            )
            .await
            .expect("ordinary execute");
        assert!(matches!(
            confirm,
            ServerMessage::CommandReceipt(CommandReceipt::Accepted { .. })
        ));
        assert!(ports.try_recv_prioritized().is_none());
        assert!(requests
            .take_pending_quit_receipt_ack(reg.id())
            .await
            .expect("take")
            .is_none());

        drop(reg);
        drop(requests);
        executor.abort();
        let _ = executor.await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn duplex_accepted_confirm_host_quit_executor_admits_tracked_critical_receipt() {
        use crate::domain::command::CommandReceipt;
        use crate::domain::id::CommandId;
        use crate::kernel::CommandBus;

        let dir = tempfile::tempdir().expect("tempdir");
        let bus = CommandBus::open(&dir.path().join("duplex-quit-admit.db")).expect("bus");
        let (requests, executor) = HostRequestExecutor::start_without_automatic_maintenance(bus);
        let (out, mut ports) = ConnectionOutputHandle::new(4, 8, 1);
        let reg = requests.register_output(out).await.expect("register");
        let client = ClientId::new();
        let negotiated = host_shutdown_negotiated(client);
        let handle = requests.with_output(reg.id());
        let inspection_id = inspection_id_for(&handle, negotiated.clone(), client).await;

        let completion = handle
            .execute_for_duplex(
                negotiated,
                confirm_quit_request(client, CommandId::new(), inspection_id),
            )
            .await
            .expect("duplex quit");
        let DuplexExecuteCompletion::ExecutorAdmittedQuitReceipt { operation_id } = completion
        else {
            panic!("expected ExecutorAdmittedQuitReceipt, got {completion:?}");
        };
        let outbound = ports.try_recv_prioritized().expect("one critical receipt");
        match outbound.message() {
            ServerMessage::CommandReceipt(CommandReceipt::Accepted {
                operation_id: wired,
                task_revision: None,
                event_ids,
                ..
            }) => {
                assert_eq!(*wired, operation_id);
                assert_eq!(event_ids.len(), 1);
            }
            other => panic!("expected host-admission Accepted, got {other:?}"),
        }
        assert!(ports.try_recv_prioritized().is_none());

        let (stored_op, mut stored_ack) = requests
            .take_pending_quit_receipt_ack(reg.id())
            .await
            .expect("take")
            .expect("stored ack");
        assert_eq!(stored_op, operation_id);
        assert_eq!(stored_ack.status(), PhysicalWriteAckStatus::Pending);
        outbound.after_successful_write();
        assert_eq!(stored_ack.status(), PhysicalWriteAckStatus::Succeeded);

        drop(reg);
        drop(requests);
        executor.abort();
        let _ = executor.await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn duplex_non_quit_rejected_quit_and_command_id_collision_remain_caller_owned() {
        use crate::domain::command::{
            Command, CommandEnvelope, CommandReceipt, CreateTaskIntent, RejectionCode,
        };
        use crate::domain::id::{CommandId, EnvironmentId, ProjectId, TaskId};
        use crate::domain::task::{
            ReviewReadiness, TaskActivity, TaskAssignment, TaskAttention, TaskConnectivity,
            WorkspaceRef,
        };
        use crate::kernel::CommandBus;

        let dir = tempfile::tempdir().expect("tempdir");
        let bus = CommandBus::open(&dir.path().join("duplex-caller-owned.db")).expect("bus");
        let (requests, executor) = HostRequestExecutor::start_without_automatic_maintenance(bus);
        let (out, mut ports) = ConnectionOutputHandle::new(4, 8, 1);
        let reg = requests.register_output(out).await.expect("register");
        let client = ClientId::new();
        let negotiated = host_shutdown_negotiated(client);
        let handle = requests.with_output(reg.id());

        let inspect = handle
            .execute_for_duplex(negotiated.clone(), inspect_quit_request(client))
            .await
            .expect("inspect");
        assert!(matches!(
            inspect,
            DuplexExecuteCompletion::CallerMustWrite(ServerMessage::QueryReply(_))
        ));
        assert!(ports.try_recv_prioritized().is_none());

        let rejected = handle
            .execute_for_duplex(
                negotiated.clone(),
                ClientRequest::Command(CommandEnvelope {
                    command_id: CommandId::new(),
                    client_id: client,
                    task_id: Some(TaskId::new()),
                    issued_at_ms: 1_725_000_000_701,
                    expected_task_revision: None,
                    command: Command::ConfirmHostQuit(
                        crate::domain::command::ConfirmHostQuitIntent {
                            inspection_id: 0,
                            allow_uninspected_worktrees: true,
                        },
                    ),
                }),
            )
            .await
            .expect("rejected");
        assert!(matches!(
            rejected,
            DuplexExecuteCompletion::CallerMustWrite(ServerMessage::CommandReceipt(
                CommandReceipt::Rejected {
                    code: RejectionCode::InvalidTransition,
                    ..
                }
            ))
        ));

        let reused_command_id = CommandId::new();
        let created = handle
            .execute(
                negotiated.clone(),
                ClientRequest::Command(CommandEnvelope {
                    command_id: reused_command_id,
                    client_id: client,
                    task_id: None,
                    issued_at_ms: 1_725_000_000_702,
                    expected_task_revision: None,
                    command: Command::CreateTask(CreateTaskIntent {
                        id: TaskId::new(),
                        environment_id: EnvironmentId::new(),
                        title: "collision".into(),
                        description: None,
                        project_id: ProjectId::new(),
                        workspace: WorkspaceRef::Main,
                        assignment: TaskAssignment::LocalOwner,
                        created_at_ms: 1_725_000_000_000,
                        connectivity: TaskConnectivity::Connected,
                        attention: TaskAttention::None,
                        activity: TaskActivity::Idle,
                        review_readiness: ReviewReadiness::NotReady,
                    }),
                }),
            )
            .await
            .expect("create task");
        let ServerMessage::CommandReceipt(CommandReceipt::Accepted {
            task_revision: Some(_),
            ..
        }) = created
        else {
            panic!("expected task Accepted with revision, got {created:?}");
        };

        let collision = handle
            .execute_for_duplex(
                negotiated,
                confirm_quit_request(client, reused_command_id, 0),
            )
            .await
            .expect("collision duplex");
        match collision {
            DuplexExecuteCompletion::CallerMustWrite(ServerMessage::CommandReceipt(
                CommandReceipt::Accepted {
                    task_revision: Some(_),
                    ..
                },
            )) => {}
            other => panic!("collision must stay caller-owned non-quit Accepted, got {other:?}"),
        }
        assert!(ports.try_recv_prioritized().is_none());
        assert!(requests
            .take_pending_quit_receipt_ack(reg.id())
            .await
            .expect("take")
            .is_none());

        drop(reg);
        drop(requests);
        executor.abort();
        let _ = executor.await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn duplex_accepted_quit_missing_then_retry_admits_on_healthy_output() {
        use crate::domain::id::CommandId;
        use crate::kernel::CommandBus;

        let dir = tempfile::tempdir().expect("tempdir");
        let bus = CommandBus::open(&dir.path().join("duplex-quit-missing.db")).expect("bus");
        let (requests, executor) = HostRequestExecutor::start_without_automatic_maintenance(bus);
        let client = ClientId::new();
        let negotiated = host_shutdown_negotiated(client);
        let inspection_id = inspection_id_for(&requests, negotiated.clone(), client).await;
        let command_id = CommandId::new();

        let missing = requests
            .execute_for_duplex(
                negotiated.clone(),
                confirm_quit_request(client, command_id, inspection_id),
            )
            .await;
        assert!(matches!(missing, Err(crate::host::IpcError::Unavailable)));

        let (out, mut ports) = ConnectionOutputHandle::new(4, 8, 1);
        let reg = requests.register_output(out).await.expect("register");
        let output_id = reg.id();
        let handle = requests.with_output(output_id);
        let retry = handle
            .execute_for_duplex(
                negotiated,
                confirm_quit_request(client, command_id, inspection_id),
            )
            .await
            .expect("retry");
        let DuplexExecuteCompletion::ExecutorAdmittedQuitReceipt { operation_id } = retry else {
            panic!("expected ExecutorAdmittedQuitReceipt, got {retry:?}");
        };
        let frame = ports.try_recv_prioritized().expect("one critical");
        match frame.message() {
            ServerMessage::CommandReceipt(crate::domain::command::CommandReceipt::Accepted {
                operation_id: wired,
                ..
            }) => assert_eq!(*wired, operation_id),
            other => panic!("expected Accepted, got {other:?}"),
        }
        assert!(ports.try_recv_prioritized().is_none());
        let (stored_op, mut ack) = requests
            .take_pending_quit_receipt_ack(output_id)
            .await
            .expect("take")
            .expect("pending ack after admit");
        assert_eq!(stored_op, operation_id);
        assert_eq!(ack.status(), PhysicalWriteAckStatus::Pending);

        drop(requests);
        executor.abort();
        let _ = executor.await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn duplex_accepted_quit_full_then_retry_on_fresh_output_and_detach_clears_ack() {
        use crate::domain::id::CommandId;
        use crate::kernel::CommandBus;

        let dir = tempfile::tempdir().expect("tempdir");
        let bus = CommandBus::open(&dir.path().join("duplex-quit-full.db")).expect("bus");
        let (requests, executor) = HostRequestExecutor::start_without_automatic_maintenance(bus);
        let (out, mut ports) = ConnectionOutputHandle::new(1, 8, 1);
        let output_id = out.id();
        let reg = requests
            .register_output(out.clone())
            .await
            .expect("register");
        let client = ClientId::new();
        let negotiated = host_shutdown_negotiated(client);
        let handle = requests.with_output(reg.id());
        out.try_enqueue_critical(sample_reply()).expect("fill");
        let inspection_id = inspection_id_for(&handle, negotiated.clone(), client).await;
        let command_id = CommandId::new();

        let full = handle
            .execute_for_duplex(
                negotiated.clone(),
                confirm_quit_request(client, command_id, inspection_id),
            )
            .await;
        assert!(matches!(full, Err(crate::host::IpcError::Unavailable)));
        assert!(out.is_shutdown_requested());
        assert!(requests
            .take_pending_quit_receipt_ack(output_id)
            .await
            .expect("take")
            .is_none());
        assert!(matches!(
            ports.try_recv_prioritized().expect("filler").message(),
            ServerMessage::QueryReply(_)
        ));
        drop(reg);

        let (healthy, mut healthy_ports) = ConnectionOutputHandle::new(4, 8, 1);
        let healthy_id = healthy.id();
        let reg = requests
            .register_output(healthy)
            .await
            .expect("register healthy");
        let handle = requests.with_output(reg.id());
        let retry = handle
            .execute_for_duplex(
                negotiated,
                confirm_quit_request(client, command_id, inspection_id),
            )
            .await
            .expect("retry");
        assert!(matches!(
            retry,
            DuplexExecuteCompletion::ExecutorAdmittedQuitReceipt { .. }
        ));
        assert!(healthy_ports.try_recv_prioritized().is_some());
        // Detach clears the pending map without requiring a take first.
        drop(reg);
        assert!(requests
            .take_pending_quit_receipt_ack(healthy_id)
            .await
            .expect("cleared")
            .is_none());

        drop(requests);
        executor.abort();
        let _ = executor.await;
    }

    fn event_replay_negotiated(client: ClientId) -> NegotiatedParameters {
        use crate::protocol::{Capability, CapabilitySet, FrameLimits, ProtocolVersion};
        NegotiatedParameters {
            version: ProtocolVersion::current(),
            client_id: client,
            capabilities: CapabilitySet::from_capabilities([
                Capability::HostShutdown,
                Capability::EventReplay,
                Capability::OperationSettlement,
            ]),
            limits: FrameLimits::v1_default(),
        }
    }

    fn count_settled(path: &std::path::Path) -> i64 {
        let conn =
            rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
                .expect("ro");
        conn.query_row(
            "SELECT COUNT(*) FROM events WHERE event_type = 'operation.settled'",
            [],
            |row| row.get(0),
        )
        .expect("count")
    }

    #[tokio::test(flavor = "current_thread")]
    async fn supervised_ready_does_not_settle_until_arm_ack_then_exits_intentional() {
        use crate::domain::host::HostCleanupBranch;
        use crate::domain::id::CommandId;
        use crate::kernel::CommandBus;
        use crate::protocol::ServerMessage;

        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("arm-before-settle.db");
        let bus = CommandBus::open(&db_path).expect("bus");
        let (requests, mut supervised) =
            HostRequestExecutor::start_supervised_without_automatic_maintenance(bus);
        let (out, mut ports) = ConnectionOutputHandle::new(8, 8, 1);
        let reg = requests.register_output(out).await.expect("register");
        let client = ClientId::new();
        let negotiated = event_replay_negotiated(client);
        let handle = requests.with_output(reg.id());
        let inspection_id = inspection_id_for(&handle, negotiated.clone(), client).await;

        let completion = handle
            .execute_for_duplex(
                negotiated.clone(),
                confirm_quit_request(client, CommandId::new(), inspection_id),
            )
            .await
            .expect("duplex quit");
        let DuplexExecuteCompletion::ExecutorAdmittedQuitReceipt { operation_id } = completion
        else {
            panic!("expected admitted quit receipt");
        };
        // Complete receipt write so high-water can succeed.
        ports
            .try_recv_prioritized()
            .expect("receipt critical")
            .after_successful_write();

        // Open a live subscription so terminal CRITICAL fanout has a target.
        let open = handle
            .execute(
                negotiated,
                ClientRequest::Query(crate::domain::query::QueryEnvelope {
                    request_id: RequestId::new(),
                    client_id: client,
                    task_id: None,
                    query: crate::domain::query::Query::OpenEventReplay { after_sequence: 0 },
                }),
            )
            .await
            .expect("open replay");
        let subscription_id = match open {
            ServerMessage::QueryReply(reply) => match reply.outcome {
                crate::domain::query::QueryOutcome::Ok(
                    crate::domain::query::QueryResult::EventReplayPage {
                        subscription_id, ..
                    },
                ) => subscription_id,
                other => panic!("expected EventReplayPage, got {other:?}"),
            },
            other => panic!("expected QueryReply, got {other:?}"),
        };
        // Drain any catch-up durables from open.
        while let Some(outbound) = ports.try_recv_prioritized() {
            outbound.after_successful_write();
        }

        for _ in HostCleanupBranch::ORDER {
            requests.run_maintenance_once().await.expect("branch");
            while let Some(outbound) = ports.try_recv_prioritized() {
                outbound.after_successful_write();
            }
        }
        assert_eq!(count_settled(&db_path), 0);

        // Kick ReadyToExit → arm without acking yet.
        let maintenance = tokio::spawn({
            let requests = requests.clone();
            async move { requests.run_maintenance_once().await }
        });
        let arm = supervised
            .arm_rx
            .recv()
            .await
            .expect("arm request before settle");
        assert_eq!(arm.operation_id, operation_id);
        assert_eq!(arm.action_epoch, 1);
        assert_eq!(
            count_settled(&db_path),
            0,
            "must not settle before arm acknowledgement"
        );

        // Writer drain task so terminal high-water can complete after settle.
        let drain = tokio::spawn(async move {
            while let Some(outbound) = ports.recv_prioritized().await {
                outbound.after_successful_write();
            }
        });

        arm.ack.send(()).expect("ack arm");
        maintenance
            .await
            .expect("maintenance join")
            .expect("maintenance ok");
        let outcome = supervised.join.await.expect("join").expect("intentional");
        assert_eq!(
            outcome,
            super::HostExecutorOutcome::Intentional {
                operation_id,
                action_epoch: 1,
            }
        );
        assert_eq!(count_settled(&db_path), 1);
        let _ = drain.await;
        let _ = subscription_id;
        drop(reg);
        drop(requests);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn supervised_terminal_critical_fanout_high_water_receipt_only_and_live_watcher() {
        use crate::domain::command::CommandReceipt;
        use crate::domain::event::Event;
        use crate::domain::host::HostCleanupBranch;
        use crate::domain::id::CommandId;
        use crate::kernel::CommandBus;
        use crate::protocol::ServerMessage;
        use std::collections::BTreeSet;

        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("terminal-fanout.db");
        let bus = CommandBus::open(&db_path).expect("bus");
        let (requests, mut supervised) =
            HostRequestExecutor::start_supervised_without_automatic_maintenance(bus);

        // Receipt-only initiator: dequeue but do not physically complete until after arm.
        let (receipt_out, mut receipt_ports) = ConnectionOutputHandle::new(8, 8, 1);
        let receipt_reg = requests
            .register_output(receipt_out)
            .await
            .expect("register receipt");
        let receipt_client = ClientId::new();
        let receipt_neg = host_shutdown_negotiated(receipt_client);
        let receipt_handle = requests.with_output(receipt_reg.id());
        let inspection_id =
            inspection_id_for(&receipt_handle, receipt_neg.clone(), receipt_client).await;
        let completion = receipt_handle
            .execute_for_duplex(
                receipt_neg,
                confirm_quit_request(receipt_client, CommandId::new(), inspection_id),
            )
            .await
            .expect("quit");
        let DuplexExecuteCompletion::ExecutorAdmittedQuitReceipt { operation_id } = completion
        else {
            panic!("expected admitted quit");
        };
        let receipt_frame = receipt_ports
            .try_recv_prioritized()
            .expect("receipt critical");
        assert!(matches!(
            receipt_frame.message(),
            ServerMessage::CommandReceipt(CommandReceipt::Accepted { .. })
        ));
        // Hold receipt_frame until after arm (pending high-water).

        // Live-only watcher with TWO subscriptions on one output.
        let (live_out, mut live_ports) = ConnectionOutputHandle::new(8, 8, 1);
        let live_reg = requests
            .register_output(live_out)
            .await
            .expect("register live");
        let live_client = ClientId::new();
        let live_neg = event_replay_negotiated(live_client);
        let live_handle = requests.with_output(live_reg.id());
        let mut live_subs = Vec::new();
        for _ in 0..2 {
            let open = live_handle
                .execute(
                    live_neg.clone(),
                    ClientRequest::Query(crate::domain::query::QueryEnvelope {
                        request_id: RequestId::new(),
                        client_id: live_client,
                        task_id: None,
                        query: crate::domain::query::Query::OpenEventReplay { after_sequence: 0 },
                    }),
                )
                .await
                .expect("open");
            let sub = match open {
                ServerMessage::QueryReply(reply) => match reply.outcome {
                    crate::domain::query::QueryOutcome::Ok(
                        crate::domain::query::QueryResult::EventReplayPage {
                            subscription_id, ..
                        },
                    ) => subscription_id,
                    other => panic!("unexpected {other:?}"),
                },
                other => panic!("unexpected {other:?}"),
            };
            live_subs.push(sub);
            while let Some(outbound) = live_ports.try_recv_prioritized() {
                outbound.after_successful_write();
            }
        }
        assert_eq!(live_subs.len(), 2);
        assert_ne!(live_subs[0], live_subs[1]);

        for _ in HostCleanupBranch::ORDER {
            requests.run_maintenance_once().await.expect("branch");
            while let Some(outbound) = live_ports.try_recv_prioritized() {
                outbound.after_successful_write();
            }
        }
        assert!(receipt_ports.try_recv_prioritized().is_none());

        let maintenance = tokio::spawn({
            let requests = requests.clone();
            async move { requests.run_maintenance_once().await }
        });
        let arm = supervised.arm_rx.recv().await.expect("arm");
        assert_eq!(arm.operation_id, operation_id);

        let expected_subs: BTreeSet<_> = live_subs.iter().copied().collect();
        let receipt_drain = tokio::spawn(async move {
            let mut saw_terminal = false;
            // Complete the held receipt after arm (receipt-only high-water).
            receipt_frame.after_successful_write();
            while let Some(outbound) = receipt_ports.recv_prioritized().await {
                if matches!(
                    outbound.message(),
                    ServerMessage::DurableEvent {
                        event: DomainEvent {
                            payload: Event::OperationSettled(_),
                            ..
                        },
                        ..
                    }
                ) {
                    saw_terminal = true;
                }
                outbound.after_successful_write();
            }
            saw_terminal
        });
        let live_drain = tokio::spawn(async move {
            let mut terminal_subs = BTreeSet::new();
            while let Some(outbound) = live_ports.recv_prioritized().await {
                if let ServerMessage::DurableEvent {
                    subscription_id,
                    event:
                        DomainEvent {
                            payload: Event::OperationSettled(fact),
                            ..
                        },
                } = outbound.message()
                {
                    assert_eq!(fact.operation_id, operation_id);
                    assert_eq!(fact.action_epoch, Some(1));
                    terminal_subs.insert(*subscription_id);
                }
                outbound.after_successful_write();
            }
            terminal_subs
        });

        arm.ack.send(()).expect("ack");
        maintenance.await.expect("join").expect("ok");
        let outcome = supervised.join.await.expect("exec").expect("intentional");
        assert_eq!(
            outcome,
            super::HostExecutorOutcome::Intentional {
                operation_id,
                action_epoch: 1,
            }
        );
        assert!(
            !receipt_drain.await.expect("receipt drain"),
            "receipt-only output must not receive a terminal CRITICAL"
        );
        assert_eq!(
            live_drain.await.expect("live drain"),
            expected_subs,
            "two live subscriptions require two distinct terminal CRITICAL frames"
        );
        drop(receipt_reg);
        drop(live_reg);
        drop(requests);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn supervised_terminal_flushes_ordered_durables_before_settlement_with_slow_output_isolation(
    ) {
        use crate::domain::event::Event;
        use crate::domain::host::HostCleanupBranch;
        use crate::domain::id::CommandId;
        use crate::kernel::CommandBus;
        use crate::protocol::ServerMessage;

        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("ordered-durable-fence.db");
        let bus = CommandBus::open(&db_path).expect("bus");
        let (requests, mut supervised) =
            HostRequestExecutor::start_supervised_without_automatic_maintenance(bus);

        let (healthy_out, mut healthy_ports) = ConnectionOutputHandle::new(8, 8, 1);
        let healthy_reg = requests
            .register_output(healthy_out)
            .await
            .expect("register healthy");
        let healthy_client = ClientId::new();
        let healthy_neg = event_replay_negotiated(healthy_client);
        let healthy_handle = requests.with_output(healthy_reg.id());
        let inspection_id =
            inspection_id_for(&healthy_handle, healthy_neg.clone(), healthy_client).await;
        let completion = healthy_handle
            .execute_for_duplex(
                healthy_neg.clone(),
                confirm_quit_request(healthy_client, CommandId::new(), inspection_id),
            )
            .await
            .expect("quit");
        let DuplexExecuteCompletion::ExecutorAdmittedQuitReceipt { operation_id } = completion
        else {
            panic!("expected admitted quit");
        };
        healthy_ports
            .try_recv_prioritized()
            .expect("receipt")
            .after_successful_write();

        let (slow_out, mut slow_ports) = ConnectionOutputHandle::new(8, 8, 1);
        let slow_probe = slow_out.clone();
        let slow_reg = requests
            .register_output(slow_out)
            .await
            .expect("register slow");
        let slow_client = ClientId::new();
        let slow_neg = event_replay_negotiated(slow_client);
        let slow_handle = requests.with_output(slow_reg.id());

        let open_healthy = healthy_handle
            .execute(
                healthy_neg,
                ClientRequest::Query(crate::domain::query::QueryEnvelope {
                    request_id: RequestId::new(),
                    client_id: healthy_client,
                    task_id: None,
                    query: crate::domain::query::Query::OpenEventReplay { after_sequence: 0 },
                }),
            )
            .await
            .expect("open healthy");
        assert!(matches!(open_healthy, ServerMessage::QueryReply(_)));
        while let Some(outbound) = healthy_ports.try_recv_prioritized() {
            outbound.after_successful_write();
        }

        let open_slow = slow_handle
            .execute(
                slow_neg,
                ClientRequest::Query(crate::domain::query::QueryEnvelope {
                    request_id: RequestId::new(),
                    client_id: slow_client,
                    task_id: None,
                    query: crate::domain::query::Query::OpenEventReplay { after_sequence: 0 },
                }),
            )
            .await
            .expect("open slow");
        assert!(matches!(open_slow, ServerMessage::QueryReply(_)));
        while let Some(outbound) = slow_ports.try_recv_prioritized() {
            outbound.after_successful_write();
        }

        // Admit four HostCleanupBranchCompleted durables; leave them queued (unacked).
        for _ in HostCleanupBranch::ORDER {
            requests.run_maintenance_once().await.expect("branch");
        }

        let (settled_tx, settled_rx) = tokio::sync::oneshot::channel();
        let healthy_drain = tokio::spawn(async move {
            let mut branch_sequences = Vec::new();
            let mut saw_settled = false;
            let mut settled_signal = Some(settled_tx);
            while let Some(outbound) = healthy_ports.recv_prioritized().await {
                match outbound.message() {
                    ServerMessage::DurableEvent {
                        event:
                            DomainEvent {
                                sequence,
                                payload: Event::HostCleanupBranchCompleted { .. },
                                ..
                            },
                        ..
                    } => {
                        assert!(
                            !saw_settled,
                            "HostCleanupBranchCompleted must precede OperationSettled"
                        );
                        branch_sequences.push(*sequence);
                    }
                    ServerMessage::DurableEvent {
                        event:
                            DomainEvent {
                                payload: Event::OperationSettled(fact),
                                ..
                            },
                        ..
                    } => {
                        assert_eq!(fact.operation_id, operation_id);
                        assert_eq!(fact.action_epoch, Some(1));
                        saw_settled = true;
                        if let Some(tx) = settled_signal.take() {
                            let _ = tx.send(());
                        }
                    }
                    _ => {}
                }
                outbound.after_successful_write();
            }
            (branch_sequences, saw_settled)
        });

        let started = std::time::Instant::now();
        let maintenance = tokio::spawn({
            let requests = requests.clone();
            async move { requests.run_maintenance_once().await }
        });
        let arm = supervised.arm_rx.recv().await.expect("arm");
        assert_eq!(arm.operation_id, operation_id);
        arm.ack.send(()).expect("ack");

        // Healthy output must settle promptly; slow output must still be fencing.
        tokio::time::timeout(Duration::from_secs(2), settled_rx)
            .await
            .expect("healthy OperationSettled must arrive well before the 5s slow-output deadline")
            .expect("settled signal");
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "healthy settlement must not wait on the stalled output"
        );
        assert!(
            !maintenance.is_finished(),
            "executor/maintenance path must still be pending while slow output fences"
        );
        assert!(
            !supervised.join.is_finished(),
            "supervised executor must still be pending while slow output fences"
        );

        maintenance.await.expect("join").expect("ok");
        let outcome = supervised.join.await.expect("exec").expect("intentional");
        assert_eq!(
            outcome,
            super::HostExecutorOutcome::Intentional {
                operation_id,
                action_epoch: 1,
            }
        );
        assert!(
            started.elapsed() < std::time::Duration::from_secs(6),
            "host/executor must exit within the one global terminal bound"
        );

        let (branch_sequences, saw_settled) = healthy_drain.await.expect("healthy drain");
        assert_eq!(
            branch_sequences.len(),
            HostCleanupBranch::ORDER.len(),
            "healthy output must physically write all admitted branch durables"
        );
        assert!(
            branch_sequences.windows(2).all(|w| w[0] < w[1]),
            "branch durables must be written in increasing sequence: {branch_sequences:?}"
        );
        assert!(
            saw_settled,
            "healthy output must receive OperationSettled CRITICAL after ordered durables"
        );

        assert!(
            slow_probe.is_shutdown_requested(),
            "slow output must be shut down after durable fence deadline"
        );
        let mut slow_saw_settled = false;
        while let Some(outbound) = slow_ports.try_recv_prioritized() {
            if matches!(
                outbound.message(),
                ServerMessage::DurableEvent {
                    event: DomainEvent {
                        payload: Event::OperationSettled(_),
                        ..
                    },
                    ..
                }
            ) {
                slow_saw_settled = true;
            }
        }
        assert!(
            !slow_saw_settled,
            "slow output must never receive OperationSettled after skipped durable history"
        );

        drop(healthy_reg);
        drop(slow_reg);
        drop(requests);
    }
}
