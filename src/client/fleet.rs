//! Bounded client-layer host fleet ownership for unified local + remote task UI.
//!
//! Each installed host runs an **owned driver** on the existing bounded
//! [`crate::remote::RemoteBackgroundWork`] OS-worker/reaper lane. That driver
//! alone owns [`HostClient`], [`ClientSubscription`], admitted wire awaits, and
//! the bounded outcome ledger. Caller futures only wait on reply channels:
//! cancelling a caller never drops an in-flight wire command / `PendingRegistration`.
//!
//! Stop/fence uses an out-of-band [`BackgroundWorkStop`] (not the bounded command
//! queue). Explicit stop cancels the current wire await, disconnects the exact
//! client, preserves exact admission+`CommandId` as uncertain, drains queued work,
//! then signals completion. Async remove waits without blocking the executor;
//! dropping the remove waiter still leaves the exact worker owned (stop + reaper).

use std::collections::{BTreeMap, VecDeque};
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, oneshot, Notify};
use uuid::Uuid;

use crate::config::paths::AppProfile;
use crate::domain::command::{Command, CommandEnvelope, CommandReceipt};
use crate::domain::event::DomainEvent;
use crate::domain::id::{CommandId, SubscriptionId, TaskId};
use crate::domain::query::{Query, QueryEnvelope, QueryReply};
use crate::domain::ClientId;
use crate::host::{profile_fingerprint_for_named_profile, IpcError};
use crate::protocol::CapabilitySet;
use crate::remote::{BackgroundWorkStop, RemoteBackgroundWork};
use crate::terminal::protocol::{InputAck, TerminalInputRequest};
use crate::updater::UpdateHandoffToken;

use super::connection::ConnectionMetadata;
use super::host_client::{HostClient, TrackedOperation};
use super::model::{ClientModel, TaskInboxPreview};
use super::subscription::{
    ClientSubscription, ClientSubscriptionState, SubscriptionError, SubscriptionUpdate,
};

/// Hard cap on simultaneously registered hosts (reserved + live + draining).
pub const MAX_FLEET_HOSTS: usize = 16;
const MAX_DRIVER_QUEUE: usize = 32;
const MAX_OUTCOME_BUFFER: usize = 32;
const MAX_UNCERTAIN: usize = 64;
const MAX_SUBSCRIPTION_EVENTS: usize = 64;
const MAX_REMOVAL_LEDGERS: usize = 32;
/// Matches [`ClientSubscription`] replay bound; never cram into the live event queue.
const MAX_FLEET_REPLAY_EVENTS: usize = 8_192;

/// Caller-owned factory that produces a connected [`HostClient`].
pub type HostClientFactory = Box<dyn FnOnce() -> HostClientConnectFuture + Send>;

/// Owned connect future returned by [`HostClientFactory`].
pub type HostClientConnectFuture =
    Pin<Box<dyn Future<Output = Result<HostClient, IpcError>> + Send>>;

/// Stable host identity for presentation and fleet registry keys.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", content = "id")]
pub enum HostId {
    LocalProfile(String),
    Remote([u8; 16]),
}

impl HostId {
    pub fn local_profile(raw: impl Into<String>) -> Result<Self, FleetError> {
        let raw = raw.into();
        match AppProfile::named(&raw) {
            Ok(AppProfile::Named(name)) => Ok(Self::LocalProfile(name)),
            Ok(_) => Err(FleetError::InvalidProfile(raw)),
            Err(_) => Err(FleetError::InvalidProfile(raw)),
        }
    }

    pub fn remote(id: [u8; 16]) -> Result<Self, FleetError> {
        if id == [0_u8; 16] {
            return Err(FleetError::InvalidRemoteHostId);
        }
        Ok(Self::Remote(id))
    }

    pub fn as_local_profile(&self) -> Option<&str> {
        match self {
            Self::LocalProfile(name) => Some(name.as_str()),
            Self::Remote(_) => None,
        }
    }

    pub fn as_remote(&self) -> Option<[u8; 16]> {
        match self {
            Self::LocalProfile(_) => None,
            Self::Remote(id) => Some(*id),
        }
    }
}

/// Presentation identity: host-qualified task key. Wire `task_id` is unchanged.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct HostTaskKey {
    pub host: HostId,
    pub task_id: TaskId,
}

impl HostTaskKey {
    pub fn new(host: HostId, task_id: TaskId) -> Self {
        Self { host, task_id }
    }
}

/// Immutable admission ticket captured at enqueue time for later execution.
///
/// `task_id` is `None` for host-global work (config/agent/provider/remote-access,
/// detach, update). Never synthesize a TaskId for global admissions.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct FleetAdmission {
    pub host: HostId,
    pub task_id: Option<TaskId>,
    pub generation: u64,
    pub client_id: ClientId,
}

impl FleetAdmission {
    pub fn host_task_key(&self) -> Option<HostTaskKey> {
        self.task_id.map(|task_id| HostTaskKey {
            host: self.host.clone(),
            task_id,
        })
    }
}

/// Owner-tagged result envelope for fleet result paths.
///
/// `task_id` is the admission-captured scope for command/query/terminal results.
/// Global sync/metadata/reconnect/disconnect results use `None`. Subscription
/// events carry a task when inherent on the update.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FleetOwned<T> {
    pub host: HostId,
    pub generation: u64,
    pub client_id: ClientId,
    pub task_id: Option<TaskId>,
    pub value: T,
}

/// Explicit Connect/local request classes that must not be inferred from bits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FleetUnsupportedKind {
    RawTerminalInput,
    ExplicitDetach,
    PrepareUpdate,
}

/// Retained when the driver cannot confirm settlement (disconnect/remove mid-wire).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FleetUncertainCommand {
    pub admission: FleetAdmission,
    pub command_id: CommandId,
}

/// Bounded retained command result when the caller cancelled before delivery.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FleetRetainedCommand {
    Receipt(CommandReceipt),
    Uncertain(FleetUncertainCommand),
}

/// Removal / reservation lifecycle result. Reserved installs have no client id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FleetRemoval {
    pub host: HostId,
    pub generation: u64,
    pub client_id: Option<ClientId>,
    pub retained: Vec<FleetOwned<FleetRetainedCommand>>,
    /// Non-actionable index of retained `FleetRetainedCommand::Uncertain` entries
    /// also present in [`Self::retained`]. Consumers must process each
    /// `CommandId` once (prefer `retained`); do not double-apply.
    pub uncertain: Vec<FleetUncertainCommand>,
}

#[derive(Debug)]
pub enum FleetError {
    InvalidProfile(String),
    InvalidRemoteHostId,
    HostNotFound,
    HostAlreadyInstalled,
    HostCapacityExceeded,
    HostMetadataMismatch,
    HostBusy,
    HostFenced,
    StaleGeneration,
    StaleClientId,
    StaleReservation,
    DisconnectedReadOnly,
    AdmissionOwnerMismatch,
    UnsupportedRequest(FleetUnsupportedKind),
    QueueFull,
    WorkerGone,
    Subscription(SubscriptionError),
    Ipc(IpcError),
}

impl fmt::Display for FleetError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidProfile(name) => write!(f, "invalid fleet profile name: {name:?}"),
            Self::InvalidRemoteHostId => write!(f, "remote host public id must be nonzero"),
            Self::HostNotFound => write!(f, "host is not installed in the fleet registry"),
            Self::HostAlreadyInstalled => write!(f, "host is already installed in the fleet"),
            Self::HostCapacityExceeded => {
                write!(f, "fleet host capacity ({MAX_FLEET_HOSTS}) exceeded")
            }
            Self::HostMetadataMismatch => {
                write!(f, "host client metadata does not match registry HostId")
            }
            Self::HostBusy => write!(f, "host driver is busy"),
            Self::HostFenced => write!(f, "host is fenced for disconnect or remove"),
            Self::StaleGeneration => write!(f, "fleet admission generation is stale"),
            Self::StaleClientId => write!(f, "fleet admission client_id does not match generation"),
            Self::StaleReservation => {
                write!(f, "install reservation was invalidated before commit")
            }
            Self::DisconnectedReadOnly => {
                write!(f, "host is disconnected; cached model is read-only")
            }
            Self::AdmissionOwnerMismatch => {
                write!(f, "command/query owner does not match fleet admission")
            }
            Self::UnsupportedRequest(kind) => write!(f, "unsupported fleet request: {kind:?}"),
            Self::QueueFull => write!(f, "per-host driver request queue is full"),
            Self::WorkerGone => write!(f, "per-host driver worker is gone"),
            Self::Subscription(error) => write!(f, "fleet subscription error: {error}"),
            Self::Ipc(error) => write!(f, "fleet host ipc error: {error}"),
        }
    }
}

impl std::error::Error for FleetError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Subscription(error) => Some(error),
            Self::Ipc(error) => Some(error),
            _ => None,
        }
    }
}

impl From<IpcError> for FleetError {
    fn from(value: IpcError) -> Self {
        Self::Ipc(value)
    }
}

impl From<SubscriptionError> for FleetError {
    fn from(value: SubscriptionError) -> Self {
        Self::Subscription(value)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HostFence {
    Live,
    /// User disconnect: admissions invalid, host stays installed for reconnect.
    DisconnectRequested,
    /// Mid-reconnect factory: admissions invalid; not a permanent remove.
    Reconnecting,
    RemoveRequested,
}

/// Immutable owner token captured under the owner-state mutex before I/O.
#[derive(Debug, Clone, PartialEq, Eq)]
struct OwnerToken {
    host: HostId,
    generation: u64,
    client_id: ClientId,
    connected: bool,
    fence: HostFence,
    capabilities: CapabilitySet,
}

#[derive(Debug, Clone)]
struct InFlightCommand {
    admission: FleetAdmission,
    command_id: CommandId,
}

struct OutcomeEntry {
    owned: FleetOwned<FleetRetainedCommand>,
    acked: bool,
}

struct OwnerState {
    token: OwnerToken,
    /// Hello/Connect metadata under the same lock as [`OwnerToken`]; refreshed
    /// atomically on successful reconnect.
    metadata: ConnectionMetadata,
    cached_model: Option<Arc<ClientModel>>,
    outcomes: VecDeque<OutcomeEntry>,
    /// Slots reserved for in-flight wire work before the final receipt/uncertain is written.
    outcome_reserved: usize,
    uncertain: VecDeque<FleetUncertainCommand>,
    subscription_events: VecDeque<FleetOwned<SubscriptionUpdate>>,
    events_overflow: bool,
    /// Recoverable gap: receivers must observe NeedsResync (no silent hang).
    subscription_gap: bool,
    /// Canonical subscription id cached by the driver after successful sync.
    subscription_id: Option<SubscriptionId>,
    /// Snapshot-race replay handoff; distinct from the 64-slot live event queue.
    pending_replay: Vec<DomainEvent>,
    in_flight: Option<InFlightCommand>,
    invalidated: bool,
    /// Token captured when disconnect was fenced; used to complete waiters.
    disconnect_token: Option<OwnerToken>,
}

impl OwnerState {
    fn tag<T>(&self, value: T) -> FleetOwned<T> {
        FleetOwned {
            host: self.token.host.clone(),
            generation: self.token.generation,
            client_id: self.token.client_id,
            task_id: None,
            value,
        }
    }

    fn tag_admission<T>(&self, admission: &FleetAdmission, value: T) -> FleetOwned<T> {
        FleetOwned {
            host: self.token.host.clone(),
            generation: self.token.generation,
            client_id: self.token.client_id,
            task_id: admission.task_id,
            value,
        }
    }

    fn admission_matches(&self, admission: &FleetAdmission) -> Result<(), FleetError> {
        if admission.host != self.token.host {
            return Err(FleetError::AdmissionOwnerMismatch);
        }
        if admission.generation != self.token.generation {
            return Err(FleetError::StaleGeneration);
        }
        if admission.client_id != self.token.client_id {
            return Err(FleetError::StaleClientId);
        }
        if self.invalidated || !matches!(self.token.fence, HostFence::Live) || !self.token.connected
        {
            return Err(FleetError::HostFenced);
        }
        Ok(())
    }

    /// Generation+client fence check that does not require Live/connected.
    /// Used so a stale port cannot disconnect a replacement generation.
    fn admission_generation_matches(&self, admission: &FleetAdmission) -> Result<(), FleetError> {
        if admission.host != self.token.host {
            return Err(FleetError::AdmissionOwnerMismatch);
        }
        if admission.generation != self.token.generation {
            return Err(FleetError::StaleGeneration);
        }
        if admission.client_id != self.token.client_id {
            return Err(FleetError::StaleClientId);
        }
        Ok(())
    }

    fn outcome_slots_used(&self) -> usize {
        self.outcomes.iter().filter(|entry| !entry.acked).count() + self.outcome_reserved
    }

    fn try_reserve_outcome_slot(&mut self) -> Result<(), FleetError> {
        if self.outcome_slots_used() >= MAX_OUTCOME_BUFFER || self.uncertain.len() >= MAX_UNCERTAIN
        {
            return Err(FleetError::QueueFull);
        }
        self.outcome_reserved = self.outcome_reserved.saturating_add(1);
        Ok(())
    }

    fn release_outcome_reservation(&mut self) {
        self.outcome_reserved = self.outcome_reserved.saturating_sub(1);
    }

    fn commit_reserved_receipt(
        &mut self,
        token: &OwnerToken,
        admission: &FleetAdmission,
        receipt: CommandReceipt,
    ) -> Result<FleetOwned<FleetRetainedCommand>, FleetError> {
        if self.outcome_reserved == 0 {
            return Err(FleetError::QueueFull);
        }
        self.outcome_reserved -= 1;
        let owned = FleetOwned {
            host: token.host.clone(),
            generation: token.generation,
            client_id: token.client_id,
            task_id: admission.task_id,
            value: FleetRetainedCommand::Receipt(receipt),
        };
        self.outcomes.push_back(OutcomeEntry {
            owned: owned.clone(),
            acked: false,
        });
        Ok(owned)
    }

    fn commit_reserved_uncertain(
        &mut self,
        uncertain: FleetUncertainCommand,
    ) -> Result<(), FleetError> {
        if self.outcome_reserved == 0 {
            return Err(FleetError::QueueFull);
        }
        if self.uncertain.len() >= MAX_UNCERTAIN {
            return Err(FleetError::QueueFull);
        }
        self.outcome_reserved -= 1;
        let owned = FleetOwned {
            host: uncertain.admission.host.clone(),
            generation: uncertain.admission.generation,
            client_id: uncertain.admission.client_id,
            task_id: uncertain.admission.task_id,
            value: FleetRetainedCommand::Uncertain(uncertain.clone()),
        };
        self.uncertain.push_back(uncertain);
        self.outcomes.push_back(OutcomeEntry {
            owned,
            acked: false,
        });
        Ok(())
    }

    fn push_uncertain_exact(&mut self, uncertain: FleetUncertainCommand) -> Result<(), FleetError> {
        // Prefer consuming a reservation when present (admitted wire path).
        if self.outcome_reserved > 0 {
            return self.commit_reserved_uncertain(uncertain);
        }
        let unacked = self.outcomes.iter().filter(|entry| !entry.acked).count();
        if self.uncertain.len() >= MAX_UNCERTAIN || unacked >= MAX_OUTCOME_BUFFER {
            return Err(FleetError::QueueFull);
        }
        let owned = FleetOwned {
            host: uncertain.admission.host.clone(),
            generation: uncertain.admission.generation,
            client_id: uncertain.admission.client_id,
            task_id: uncertain.admission.task_id,
            value: FleetRetainedCommand::Uncertain(uncertain.clone()),
        };
        self.uncertain.push_back(uncertain);
        self.outcomes.push_back(OutcomeEntry {
            owned,
            acked: false,
        });
        Ok(())
    }

    fn push_subscription_event(
        &mut self,
        token: &OwnerToken,
        update: SubscriptionUpdate,
    ) -> Result<(), FleetError> {
        if self.subscription_events.len() >= MAX_SUBSCRIPTION_EVENTS {
            self.events_overflow = true;
            self.subscription_gap = true;
            return Err(FleetError::Subscription(SubscriptionError::NeedsResync));
        }
        self.subscription_events.push_back(FleetOwned {
            host: token.host.clone(),
            generation: token.generation,
            client_id: token.client_id,
            task_id: subscription_update_task_id(&update),
            value: update,
        });
        Ok(())
    }

    fn clear_live_subscription_queue(&mut self) {
        self.subscription_events.clear();
        self.events_overflow = false;
        self.subscription_gap = false;
    }

    /// Retire canonical subscription identity + replay for a generation fence.
    fn retire_subscription_generation(&mut self) {
        self.clear_live_subscription_queue();
        self.subscription_id = None;
        self.pending_replay.clear();
    }

    fn clear_subscription_surface(&mut self) {
        self.clear_live_subscription_queue();
    }

    fn take_ledgers(
        &mut self,
    ) -> (
        Vec<FleetOwned<FleetRetainedCommand>>,
        Vec<FleetUncertainCommand>,
    ) {
        self.outcome_reserved = 0;
        let retained = self.outcomes.drain(..).map(|entry| entry.owned).collect();
        let uncertain = self.uncertain.drain(..).collect();
        (retained, uncertain)
    }
}

struct HostDriverShared {
    state: Mutex<OwnerState>,
    events_notify: Notify,
    /// Out-of-band disconnect control (distinct from permanent worker stop).
    disconnect_notify: Notify,
    disconnect_waiters: Mutex<Vec<oneshot::Sender<Result<FleetOwned<()>, FleetError>>>>,
    /// Exact removal completion published once when ledgers are claimed from the
    /// driver. Removers take a local copy so acknowledge cannot erase their return.
    removal_result: Mutex<Option<FleetRemoval>>,
    /// Set when the driver loop has finished finalize_stop.
    stopped: AtomicBool,
    stopped_notify: Notify,
}

impl HostDriverShared {
    fn with_state<R>(&self, f: impl FnOnce(&mut OwnerState) -> R) -> Result<R, FleetError> {
        let mut guard = self.state.lock().map_err(|_| FleetError::HostBusy)?;
        Ok(f(&mut guard))
    }

    fn snapshot_token(&self) -> Result<OwnerToken, FleetError> {
        self.with_state(|state| state.token.clone())
    }

    fn fence_is_disconnect(&self) -> bool {
        self.with_state(|state| matches!(state.token.fence, HostFence::DisconnectRequested))
            .unwrap_or(false)
    }

    fn publish_removal_once(&self, removal: FleetRemoval) -> Result<FleetRemoval, FleetError> {
        let mut slot = self
            .removal_result
            .lock()
            .map_err(|_| FleetError::HostBusy)?;
        if let Some(existing) = slot.as_ref() {
            return Ok(existing.clone());
        }
        *slot = Some(removal.clone());
        Ok(removal)
    }

    fn claim_removal_result(&self) -> Result<Option<FleetRemoval>, FleetError> {
        let mut slot = self
            .removal_result
            .lock()
            .map_err(|_| FleetError::HostBusy)?;
        Ok(slot.take())
    }
}

enum DriverRequest {
    Execute {
        admission: FleetAdmission,
        envelope: CommandEnvelope,
        reply: oneshot::Sender<Result<FleetOwned<CommandReceipt>, FleetError>>,
    },
    Query {
        admission: FleetAdmission,
        envelope: QueryEnvelope,
        timeout: Option<Duration>,
        reply: oneshot::Sender<Result<FleetOwned<QueryReply>, FleetError>>,
    },
    TerminalInput {
        admission: FleetAdmission,
        request: TerminalInputRequest,
        reply: oneshot::Sender<Result<FleetOwned<InputAck>, FleetError>>,
    },
    PreviewTasks {
        admission: FleetAdmission,
        reply: oneshot::Sender<Result<FleetOwned<TaskInboxPreview>, FleetError>>,
    },
    Detach {
        admission: FleetAdmission,
        reply: oneshot::Sender<Result<FleetOwned<Uuid>, FleetError>>,
    },
    PrepareUpdate {
        admission: FleetAdmission,
        command_id: CommandId,
        target_version: String,
        client_build: String,
        host_build: String,
        allow_explicit_confirm_with_active: bool,
        reply: oneshot::Sender<Result<FleetOwned<UpdateHandoffToken>, FleetError>>,
    },
    Synchronize {
        reply: oneshot::Sender<Result<FleetOwned<()>, FleetError>>,
    },
    ReconnectFactory {
        factory: HostClientFactory,
        next_generation: u64,
        reply: oneshot::Sender<Result<FleetOwned<u64>, FleetError>>,
    },
    ReconnectLocal {
        next_generation: u64,
        reply: oneshot::Sender<Result<FleetOwned<u64>, FleetError>>,
    },
    #[cfg(test)]
    InspectTracked {
        operation_id: crate::domain::id::OperationId,
        reply: oneshot::Sender<Option<TrackedOperation>>,
    },
}

struct HostDriver {
    shared: Arc<HostDriverShared>,
    tx: mpsc::Sender<DriverRequest>,
    background: Mutex<Option<RemoteBackgroundWork>>,
}

/// Runtime construction failure or a panic must retire lifecycle waiters too.
struct DriverExitGuard(Arc<HostDriverShared>);

impl Drop for DriverExitGuard {
    fn drop(&mut self) {
        let _ = self.0.with_state(|state| {
            state.invalidated = true;
            state.token.connected = false;
        });
        complete_disconnect_waiters(&self.0, Err(FleetError::WorkerGone));
        self.0.stopped.store(true, Ordering::Release);
        self.0.stopped_notify.notify_waiters();
        self.0.events_notify.notify_waiters();
    }
}

impl HostDriver {
    fn start(
        host_id: HostId,
        client: HostClient,
        generation: u64,
    ) -> Result<Arc<Self>, FleetError> {
        let client_id = client.client_id();
        let capabilities = client.granted_capabilities();
        let metadata = client.metadata().clone();
        let connected = client.is_connected();
        let shared = Arc::new(HostDriverShared {
            state: Mutex::new(OwnerState {
                token: OwnerToken {
                    host: host_id.clone(),
                    generation,
                    client_id,
                    connected,
                    fence: HostFence::Live,
                    capabilities,
                },
                metadata,
                cached_model: None,
                outcomes: VecDeque::new(),
                outcome_reserved: 0,
                uncertain: VecDeque::new(),
                subscription_events: VecDeque::new(),
                events_overflow: false,
                subscription_gap: false,
                subscription_id: None,
                pending_replay: Vec::new(),
                in_flight: None,
                invalidated: false,
                disconnect_token: None,
            }),
            events_notify: Notify::new(),
            disconnect_notify: Notify::new(),
            disconnect_waiters: Mutex::new(Vec::new()),
            removal_result: Mutex::new(None),
            stopped: AtomicBool::new(false),
            stopped_notify: Notify::new(),
        });
        let (tx, rx) = mpsc::channel(MAX_DRIVER_QUEUE);
        let shared_for_job = Arc::clone(&shared);
        let background =
            RemoteBackgroundWork::spawn(format!("devmanager-fleet-{host_id:?}"), move |stop| {
                let _exit = DriverExitGuard(Arc::clone(&shared_for_job));
                let runtime = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(runtime) => runtime,
                    Err(_) => return,
                };
                runtime.block_on(driver_loop(
                    client,
                    ClientSubscription::new(),
                    rx,
                    shared_for_job,
                    stop,
                ));
            })
            .map_err(|_| FleetError::WorkerGone)?;
        Ok(Arc::new(Self {
            shared,
            tx,
            background: Mutex::new(Some(background)),
        }))
    }

    fn request_stop(&self) {
        let _ = self.shared.with_state(|state| {
            state.invalidated = true;
            state.token.fence = HostFence::RemoveRequested;
            state.token.connected = false;
        });
        self.shared.events_notify.notify_waiters();
        self.shared.disconnect_notify.notify_waiters();
        if let Ok(guard) = self.background.lock() {
            if let Some(background) = guard.as_ref() {
                background.request_stop();
            }
        }
    }

    fn is_background_finished(&self) -> bool {
        self.background
            .lock()
            .map(|guard| guard.as_ref().is_none_or(RemoteBackgroundWork::is_finished))
            .unwrap_or(false)
    }

    async fn wait_shutdown(self: &Arc<Self>) {
        self.request_stop();
        loop {
            let notified = self.shared.stopped_notify.notified();
            tokio::pin!(notified);
            if notified.as_mut().enable() {
                if self.shared.stopped.load(Ordering::Acquire) {
                    break;
                }
                continue;
            }
            if self.shared.stopped.load(Ordering::Acquire) {
                break;
            }
            notified.await;
            if self.shared.stopped.load(Ordering::Acquire) {
                break;
            }
        }
        loop {
            let joined = {
                let mut guard = self
                    .background
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                if guard.as_ref().is_none_or(RemoteBackgroundWork::is_finished) {
                    // Physical completion precedes this immediate join. Keep
                    // ownership in the registry across every cancellable await.
                    drop(guard.take());
                    true
                } else {
                    false
                }
            };
            if joined {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }
    }

    async fn request<T>(
        &self,
        build: impl FnOnce(oneshot::Sender<Result<T, FleetError>>) -> DriverRequest,
    ) -> Result<T, FleetError> {
        let invalidated = self.shared.with_state(|state| state.invalidated)?;
        if invalidated {
            return Err(FleetError::HostFenced);
        }
        let (tx, rx) = oneshot::channel();
        self.tx.try_send(build(tx)).map_err(|error| match error {
            mpsc::error::TrySendError::Full(_) => FleetError::QueueFull,
            mpsc::error::TrySendError::Closed(_) => FleetError::WorkerGone,
        })?;
        rx.await.map_err(|_| FleetError::WorkerGone)?
    }
}

impl Drop for HostDriver {
    fn drop(&mut self) {
        if let Ok(mut guard) = self.background.lock() {
            if let Some(background) = guard.as_mut() {
                background.request_stop();
            }
            // Dropping RemoteBackgroundWork requests stop and defers to reaper.
            drop(guard.take());
        }
    }
}

/// Public handle snapshot accessors (no exclusive client loan).
pub struct HostHandle {
    shared: Arc<HostDriverShared>,
}

impl HostHandle {
    pub fn host_id(&self) -> Result<HostId, FleetError> {
        Ok(self.shared.snapshot_token()?.host)
    }

    pub fn generation(&self) -> Result<u64, FleetError> {
        Ok(self.shared.snapshot_token()?.generation)
    }

    pub fn client_id(&self) -> Result<ClientId, FleetError> {
        Ok(self.shared.snapshot_token()?.client_id)
    }

    pub fn is_connected(&self) -> bool {
        self.shared
            .snapshot_token()
            .map(|token| token.connected)
            .unwrap_or(false)
    }

    pub fn granted_capabilities(&self) -> CapabilitySet {
        self.shared
            .snapshot_token()
            .map(|token| token.capabilities)
            .unwrap_or_else(|_| CapabilitySet::empty())
    }

    pub fn cached_model(&self) -> Option<Arc<ClientModel>> {
        self.shared
            .with_state(|state| state.cached_model.clone())
            .ok()
            .flatten()
    }
}

struct HostReservation {
    generation: u64,
    invalidated: AtomicBool,
}

enum HostEntry {
    Reserved(HostReservation),
    Live(Arc<HostDriver>),
}

/// Registry of reserved/live hosts. No active/current host pointer exists.
pub struct HostFleet {
    hosts: Mutex<BTreeMap<HostId, HostEntry>>,
    /// Exact lifecycle identity: concurrent reinstall must not overwrite another generation.
    draining: Mutex<BTreeMap<(HostId, u64), Arc<HostDriver>>>,
    removal_ledgers: Mutex<BTreeMap<(HostId, u64), FleetRemoval>>,
    generation_counter: AtomicU64,
}

impl Default for HostFleet {
    fn default() -> Self {
        Self::new()
    }
}

impl HostFleet {
    pub fn new() -> Self {
        Self {
            hosts: Mutex::new(BTreeMap::new()),
            draining: Mutex::new(BTreeMap::new()),
            removal_ledgers: Mutex::new(BTreeMap::new()),
            generation_counter: AtomicU64::new(0),
        }
    }

    fn alloc_generation(&self) -> u64 {
        self.generation_counter.fetch_add(1, Ordering::AcqRel) + 1
    }

    fn occupied_count(
        hosts: &BTreeMap<HostId, HostEntry>,
        draining: &BTreeMap<(HostId, u64), Arc<HostDriver>>,
    ) -> usize {
        hosts.len() + draining.len()
    }

    fn reclaim_finished_draining_locked(
        draining: &mut BTreeMap<(HostId, u64), Arc<HostDriver>>,
        ledgers: &mut BTreeMap<(HostId, u64), FleetRemoval>,
    ) -> Result<(), FleetError> {
        let finished: Vec<(HostId, u64)> = draining
            .iter()
            .filter(|(_, driver)| driver.is_background_finished())
            .map(|(key, _)| key.clone())
            .collect();
        for key in finished {
            if !ledgers.contains_key(&key) && ledgers.len() >= MAX_REMOVAL_LEDGERS {
                // Keep the exact driver and outcomes until an older ledger is acknowledged.
                return Err(FleetError::QueueFull);
            }
            let Some(driver) = draining.remove(&key) else {
                continue;
            };
            // Transfer OS-worker ownership without blocking_recv on an async executor:
            // Drop joins when finished, otherwise defers to the existing reaper.
            if let Ok(mut guard) = driver.background.lock() {
                drop(guard.take());
            }
            let (retained, uncertain) = driver.shared.with_state(|state| {
                state.retire_subscription_generation();
                state.take_ledgers()
            })?;
            let client_id = driver
                .shared
                .snapshot_token()
                .ok()
                .map(|token| token.client_id);
            let removal = driver.shared.publish_removal_once(FleetRemoval {
                host: key.0.clone(),
                generation: key.1,
                client_id,
                retained,
                uncertain,
            })?;
            // Never overwrite an existing unresolved ledger for this generation.
            if !ledgers.contains_key(&key) {
                if ledgers.len() >= MAX_REMOVAL_LEDGERS {
                    return Err(FleetError::QueueFull);
                }
                ledgers.insert(key, removal);
            }
        }
        Ok(())
    }

    /// Publish finished draining workers into generation-keyed removal ledgers.
    pub fn reclaim_finished_draining(&self) -> Result<(), FleetError> {
        let mut draining = self.draining.lock().map_err(|_| FleetError::HostBusy)?;
        let mut ledgers = self
            .removal_ledgers
            .lock()
            .map_err(|_| FleetError::HostBusy)?;
        Self::reclaim_finished_draining_locked(&mut draining, &mut ledgers)
    }

    pub fn contains(&self, host: &HostId) -> bool {
        self.hosts
            .lock()
            .map(|guard| guard.contains_key(host))
            .unwrap_or(false)
    }

    pub fn host_ids(&self) -> Vec<HostId> {
        self.hosts
            .lock()
            .map(|guard| guard.keys().cloned().collect())
            .unwrap_or_default()
    }

    pub fn generation(&self, host: &HostId) -> Result<u64, FleetError> {
        let hosts = self.hosts.lock().map_err(|_| FleetError::HostBusy)?;
        match hosts.get(host) {
            Some(HostEntry::Reserved(reservation)) => Ok(reservation.generation),
            Some(HostEntry::Live(driver)) => Ok(driver.shared.snapshot_token()?.generation),
            None => Err(FleetError::HostNotFound),
        }
    }

    pub fn client_id(&self, host: &HostId) -> Result<ClientId, FleetError> {
        match self.live(host) {
            Ok(driver) => Ok(driver.shared.snapshot_token()?.client_id),
            Err(FleetError::DisconnectedReadOnly) => Err(FleetError::DisconnectedReadOnly),
            Err(error) => Err(error),
        }
    }

    pub fn is_connected(&self, host: &HostId) -> Result<bool, FleetError> {
        let hosts = self.hosts.lock().map_err(|_| FleetError::HostBusy)?;
        match hosts.get(host) {
            Some(HostEntry::Reserved(_)) => Ok(false),
            Some(HostEntry::Live(driver)) => Ok(driver.shared.snapshot_token()?.connected),
            None => Err(FleetError::HostNotFound),
        }
    }

    pub fn granted_capabilities(&self, host: &HostId) -> Result<CapabilitySet, FleetError> {
        let hosts = self.hosts.lock().map_err(|_| FleetError::HostBusy)?;
        match hosts.get(host) {
            Some(HostEntry::Reserved(_)) => Ok(CapabilitySet::empty()),
            Some(HostEntry::Live(driver)) => Ok(driver.shared.snapshot_token()?.capabilities),
            None => Err(FleetError::HostNotFound),
        }
    }

    pub fn handle(&self, host: &HostId) -> Result<HostHandle, FleetError> {
        let driver = self.live(host)?;
        Ok(HostHandle {
            shared: Arc::clone(&driver.shared),
        })
    }

    /// Install a connected client under an explicit [`HostId`].
    pub fn install(&self, host_id: HostId, client: HostClient) -> Result<u64, FleetError> {
        validate_client_metadata(&host_id, &client)?;
        if !client.is_connected() {
            return Err(FleetError::DisconnectedReadOnly);
        }
        let _ = self.reclaim_finished_draining();
        let generation = {
            let mut hosts = self.hosts.lock().map_err(|_| FleetError::HostBusy)?;
            let mut draining = self.draining.lock().map_err(|_| FleetError::HostBusy)?;
            let mut ledgers = self
                .removal_ledgers
                .lock()
                .map_err(|_| FleetError::HostBusy)?;
            Self::reclaim_finished_draining_locked(&mut draining, &mut ledgers)?;
            if Self::occupied_count(&hosts, &draining) >= MAX_FLEET_HOSTS {
                return Err(FleetError::HostCapacityExceeded);
            }
            if hosts.contains_key(&host_id) {
                return Err(FleetError::HostAlreadyInstalled);
            }
            let generation = self.alloc_generation();
            hosts.insert(
                host_id.clone(),
                HostEntry::Reserved(HostReservation {
                    generation,
                    invalidated: AtomicBool::new(false),
                }),
            );
            generation
        };
        let mut guard = InstallReservationGuard {
            fleet: self,
            host_id: host_id.clone(),
            generation,
            disarmed: false,
        };
        let driver = HostDriver::start(host_id.clone(), client, generation)?;
        {
            let mut hosts = self.hosts.lock().map_err(|_| FleetError::HostBusy)?;
            match hosts.get(&host_id) {
                Some(HostEntry::Reserved(reservation))
                    if reservation.generation == generation
                        && !reservation.invalidated.load(Ordering::Acquire) =>
                {
                    hosts.insert(host_id, HostEntry::Live(driver));
                    guard.disarm();
                    Ok(generation)
                }
                _ => {
                    drop(hosts);
                    driver.request_stop();
                    Err(FleetError::StaleReservation)
                }
            }
        }
    }

    /// Reserve generation, await factory, commit only if reservation still valid.
    pub async fn install_with_factory(
        &self,
        host_id: HostId,
        factory: HostClientFactory,
    ) -> Result<u64, FleetError> {
        let _ = self.reclaim_finished_draining();
        let generation = {
            let mut hosts = self.hosts.lock().map_err(|_| FleetError::HostBusy)?;
            let mut draining = self.draining.lock().map_err(|_| FleetError::HostBusy)?;
            let mut ledgers = self
                .removal_ledgers
                .lock()
                .map_err(|_| FleetError::HostBusy)?;
            Self::reclaim_finished_draining_locked(&mut draining, &mut ledgers)?;
            if Self::occupied_count(&hosts, &draining) >= MAX_FLEET_HOSTS {
                return Err(FleetError::HostCapacityExceeded);
            }
            if hosts.contains_key(&host_id) {
                return Err(FleetError::HostAlreadyInstalled);
            }
            let generation = self.alloc_generation();
            hosts.insert(
                host_id.clone(),
                HostEntry::Reserved(HostReservation {
                    generation,
                    invalidated: AtomicBool::new(false),
                }),
            );
            generation
        };
        let mut guard = InstallReservationGuard {
            fleet: self,
            host_id: host_id.clone(),
            generation,
            disarmed: false,
        };

        let client = factory().await.map_err(FleetError::from)?;
        validate_client_metadata(&host_id, &client)?;
        if !client.is_connected() {
            return Err(FleetError::DisconnectedReadOnly);
        }

        let driver = HostDriver::start(host_id.clone(), client, generation)?;
        {
            let mut hosts = self.hosts.lock().map_err(|_| FleetError::HostBusy)?;
            match hosts.get(&host_id) {
                Some(HostEntry::Reserved(reservation))
                    if reservation.generation == generation
                        && !reservation.invalidated.load(Ordering::Acquire) =>
                {
                    hosts.insert(host_id, HostEntry::Live(driver));
                    guard.disarm();
                    Ok(generation)
                }
                Some(HostEntry::Reserved(_)) | None => {
                    drop(hosts);
                    driver.request_stop();
                    Err(FleetError::StaleReservation)
                }
                Some(HostEntry::Live(_)) => {
                    drop(hosts);
                    driver.request_stop();
                    Err(FleetError::StaleReservation)
                }
            }
        }
    }

    /// Remove whatever generation currently owns `host` (legacy entrypoint).
    pub async fn remove(&self, host: &HostId) -> Result<FleetRemoval, FleetError> {
        self.remove_inner(host, None).await
    }

    /// Remove only when the live/reserved generation still equals `expected_generation`.
    ///
    /// For live hosts, the generation check and `RemoveRequested`/invalidation are
    /// published under the same [`OwnerState`] lock before the registry entry is
    /// removed, so a concurrent reconnect cannot rotate the generation between
    /// snapshot and fence. A stale expected generation leaves the replacement
    /// transport, registry entry, and ledgers untouched.
    pub async fn remove_at_generation(
        &self,
        host: &HostId,
        expected_generation: u64,
    ) -> Result<FleetRemoval, FleetError> {
        self.remove_inner(host, Some(expected_generation)).await
    }

    async fn remove_inner(
        &self,
        host: &HostId,
        expected_generation: Option<u64>,
    ) -> Result<FleetRemoval, FleetError> {
        let (key, driver) = {
            // All registry transitions take these locks in this order.
            let mut hosts = self.hosts.lock().map_err(|_| FleetError::HostBusy)?;
            let mut draining = self.draining.lock().map_err(|_| FleetError::HostBusy)?;
            let mut ledgers = self
                .removal_ledgers
                .lock()
                .map_err(|_| FleetError::HostBusy)?;
            Self::reclaim_finished_draining_locked(&mut draining, &mut ledgers)?;
            if !hosts.contains_key(host) {
                return Err(FleetError::HostNotFound);
            }
            // Draining generations already own a future ledger slot.
            if ledgers.len() + draining.len() >= MAX_REMOVAL_LEDGERS {
                return Err(FleetError::QueueFull);
            }
            match hosts.get(host) {
                Some(HostEntry::Reserved(reservation)) => {
                    if expected_generation
                        .is_some_and(|expected| expected != reservation.generation)
                    {
                        return Err(FleetError::StaleGeneration);
                    }
                    let HostEntry::Reserved(reservation) = hosts
                        .remove(host)
                        .expect("checked reserved host under registry lock")
                    else {
                        unreachable!("reserved entry observed above");
                    };
                    reservation.invalidated.store(true, Ordering::Release);
                    let removal = FleetRemoval {
                        host: host.clone(),
                        generation: reservation.generation,
                        client_id: None,
                        retained: Vec::new(),
                        uncertain: Vec::new(),
                    };
                    ledgers.insert((host.clone(), reservation.generation), removal.clone());
                    return Ok(removal);
                }
                Some(HostEntry::Live(driver)) => {
                    let generation = driver.shared.with_state(|state| {
                        if let Some(expected) = expected_generation {
                            if state.token.generation != expected {
                                return Err(FleetError::StaleGeneration);
                            }
                        }
                        // Fence under the owner lock before registry removal so a
                        // reconnect cannot publish a newer generation mid-remove.
                        state.invalidated = true;
                        state.token.fence = HostFence::RemoveRequested;
                        state.token.connected = false;
                        Ok(state.token.generation)
                    })??;
                    let HostEntry::Live(driver) = hosts
                        .remove(host)
                        .expect("checked live host under registry lock")
                    else {
                        unreachable!("live entry observed above");
                    };
                    let key = (host.clone(), generation);
                    // Owner fence already published under OwnerState; request_stop
                    // is idempotent and drives background teardown / waiters.
                    driver.request_stop();
                    draining.insert(key.clone(), Arc::clone(&driver));
                    (key, driver)
                }
                None => return Err(FleetError::HostNotFound),
            }
        };
        // Cancellation cannot lose the registry's draining owner or its ledger.
        driver.wait_shutdown().await;
        // Claim the driver's once-published completion before any concurrent
        // acknowledge can erase the shared ledger slot.
        let claimed = driver.shared.claim_removal_result()?;
        self.reclaim_finished_draining()?;
        let claimed = match claimed {
            Some(removal) => Some(removal),
            None => driver.shared.claim_removal_result()?,
        };
        if let Some(removal) = claimed {
            // The reclaimer already published this ledger. An explicit ack
            // may have removed it meanwhile; never resurrect acknowledged work.
            return Ok(removal);
        }
        self.removal_ledgers
            .lock()
            .map_err(|_| FleetError::HostBusy)?
            .get(&key)
            .cloned()
            .ok_or(FleetError::WorkerGone)
    }

    /// Bounded outcome ledger retained after remove (latest generation for host).
    pub fn removal_ledger(&self, host: &HostId) -> Result<Option<FleetRemoval>, FleetError> {
        let _ = self.reclaim_finished_draining();
        let ledgers = self
            .removal_ledgers
            .lock()
            .map_err(|_| FleetError::HostBusy)?;
        Ok(ledgers
            .iter()
            .filter(|((h, _), _)| h == host)
            .max_by_key(|((_, gen), _)| *gen)
            .map(|(_, removal)| removal.clone()))
    }

    /// Exact generation removal ledger.
    pub fn removal_ledger_at(
        &self,
        host: &HostId,
        generation: u64,
    ) -> Result<Option<FleetRemoval>, FleetError> {
        let _ = self.reclaim_finished_draining();
        let ledgers = self
            .removal_ledgers
            .lock()
            .map_err(|_| FleetError::HostBusy)?;
        Ok(ledgers.get(&(host.clone(), generation)).cloned())
    }

    /// Free a finished removal ledger slot (bounded history backpressure).
    pub fn acknowledge_removal_ledger(
        &self,
        host: &HostId,
        generation: u64,
    ) -> Result<(), FleetError> {
        let mut ledgers = self
            .removal_ledgers
            .lock()
            .map_err(|_| FleetError::HostBusy)?;
        if ledgers.remove(&(host.clone(), generation)).is_some() {
            Ok(())
        } else {
            Err(FleetError::HostNotFound)
        }
    }

    /// Fence disconnect synchronously before the first await; driver completes waiters.
    pub async fn disconnect(&self, host: &HostId) -> Result<FleetOwned<()>, FleetError> {
        let driver = self.live(host)?;
        disconnect_driver(driver, None).await
    }

    /// Fence disconnect only when generation+client still match the captured
    /// driver owner. Validates under the same state lock that publishes
    /// `DisconnectRequested`, using the captured driver (no second live lookup).
    pub async fn disconnect_admitted(
        &self,
        admission: &FleetAdmission,
    ) -> Result<FleetOwned<()>, FleetError> {
        let driver = self.live(&admission.host)?;
        disconnect_driver(driver, Some(admission)).await
    }

    pub fn admit_action(&self, key: HostTaskKey) -> Result<FleetAdmission, FleetError> {
        let driver = self.live(&key.host)?;
        let token = driver.shared.snapshot_token()?;
        if !token.connected || !matches!(token.fence, HostFence::Live) {
            return Err(FleetError::DisconnectedReadOnly);
        }
        let admission = FleetAdmission {
            host: key.host,
            task_id: Some(key.task_id),
            generation: token.generation,
            client_id: token.client_id,
        };
        driver
            .shared
            .with_state(|state| state.admission_matches(&admission))??;
        Ok(admission)
    }

    pub fn admit_read(&self, key: HostTaskKey) -> Result<FleetAdmission, FleetError> {
        let driver = self.live(&key.host)?;
        let (token, has_model) = driver
            .shared
            .with_state(|state| (state.token.clone(), state.cached_model.is_some()))?;
        if !has_model && !token.connected {
            return Err(FleetError::DisconnectedReadOnly);
        }
        Ok(FleetAdmission {
            host: key.host,
            task_id: Some(key.task_id),
            generation: token.generation,
            client_id: token.client_id,
        })
    }

    /// Host-global admission (`task_id = None`). Never synthesizes a TaskId.
    pub fn admit_host(&self, host: &HostId) -> Result<FleetAdmission, FleetError> {
        let driver = self.live(host)?;
        let token = driver.shared.snapshot_token()?;
        if !token.connected || !matches!(token.fence, HostFence::Live) {
            return Err(FleetError::DisconnectedReadOnly);
        }
        let admission = FleetAdmission {
            host: host.clone(),
            task_id: None,
            generation: token.generation,
            client_id: token.client_id,
        };
        driver
            .shared
            .with_state(|state| state.admission_matches(&admission))??;
        Ok(admission)
    }

    pub fn validate_admission(&self, admission: &FleetAdmission) -> Result<(), FleetError> {
        self.live(&admission.host)?
            .shared
            .with_state(|state| state.admission_matches(admission))??;
        Ok(())
    }

    /// Owner-scoped Hello/Connect metadata snapshot (host-global `task_id`).
    pub fn owner_metadata(
        &self,
        host: &HostId,
    ) -> Result<FleetOwned<ConnectionMetadata>, FleetError> {
        let driver = self.live(host)?;
        driver.shared.with_state(|state| {
            Ok(FleetOwned {
                host: state.token.host.clone(),
                generation: state.token.generation,
                client_id: state.token.client_id,
                task_id: None,
                value: state.metadata.clone(),
            })
        })?
    }

    pub fn classify_request_support(
        &self,
        host: &HostId,
        kind: FleetUnsupportedKind,
    ) -> Result<(), FleetError> {
        let _caps = self.granted_capabilities(host)?;
        if matches!(host, HostId::Remote(_)) {
            return Err(FleetError::UnsupportedRequest(kind));
        }
        Ok(())
    }

    pub async fn execute_command(
        &self,
        admission: &FleetAdmission,
        envelope: CommandEnvelope,
    ) -> Result<FleetOwned<CommandReceipt>, FleetError> {
        validate_command_admission(admission, &envelope)?;
        reject_remote_shutdown_or_update(&admission.host, &envelope.command)?;
        let driver = self.live(&admission.host)?;
        driver
            .shared
            .with_state(|state| state.admission_matches(admission))??;
        let admission = admission.clone();
        driver
            .request(|reply| DriverRequest::Execute {
                admission,
                envelope,
                reply,
            })
            .await
    }

    pub async fn query(
        &self,
        admission: &FleetAdmission,
        envelope: QueryEnvelope,
    ) -> Result<FleetOwned<QueryReply>, FleetError> {
        self.query_with_timeout(admission, envelope, None).await
    }

    pub async fn query_with_timeout(
        &self,
        admission: &FleetAdmission,
        envelope: QueryEnvelope,
        timeout: Option<Duration>,
    ) -> Result<FleetOwned<QueryReply>, FleetError> {
        validate_query_admission(admission, &envelope)?;
        let driver = self.live(&admission.host)?;
        driver
            .shared
            .with_state(|state| state.admission_matches(admission))??;
        let admission = admission.clone();
        driver
            .request(|reply| DriverRequest::Query {
                admission,
                envelope,
                timeout,
                reply,
            })
            .await
    }

    pub async fn execute_terminal_input(
        &self,
        admission: &FleetAdmission,
        request: TerminalInputRequest,
    ) -> Result<FleetOwned<InputAck>, FleetError> {
        if request.client_id != admission.client_id {
            return Err(FleetError::AdmissionOwnerMismatch);
        }
        match admission.task_id {
            Some(task_id) if request.context.task_id == task_id => {}
            _ => return Err(FleetError::AdmissionOwnerMismatch),
        }
        self.classify_request_support(&admission.host, FleetUnsupportedKind::RawTerminalInput)?;
        let driver = self.live(&admission.host)?;
        driver
            .shared
            .with_state(|state| state.admission_matches(admission))??;
        let admission = admission.clone();
        driver
            .request(|reply| DriverRequest::TerminalInput {
                admission,
                request,
                reply,
            })
            .await
    }

    /// Bounded task-list preview: open/copy/release under one driver-owned request.
    /// Uses a temporary subscription so the canonical subscription is untouched.
    pub async fn preview_tasks(
        &self,
        admission: &FleetAdmission,
    ) -> Result<FleetOwned<TaskInboxPreview>, FleetError> {
        if admission.task_id.is_some() {
            return Err(FleetError::AdmissionOwnerMismatch);
        }
        let driver = self.live(&admission.host)?;
        driver
            .shared
            .with_state(|state| state.admission_matches(admission))??;
        let admission = admission.clone();
        driver
            .request(|reply| DriverRequest::PreviewTasks { admission, reply })
            .await
    }

    /// Local-only acknowledged detach through the sole HostClient.
    pub async fn detach(&self, admission: &FleetAdmission) -> Result<FleetOwned<Uuid>, FleetError> {
        if admission.task_id.is_some() {
            return Err(FleetError::AdmissionOwnerMismatch);
        }
        self.classify_request_support(&admission.host, FleetUnsupportedKind::ExplicitDetach)?;
        let driver = self.live(&admission.host)?;
        driver
            .shared
            .with_state(|state| state.admission_matches(admission))??;
        let admission = admission.clone();
        driver
            .request(|reply| DriverRequest::Detach { admission, reply })
            .await
    }

    /// Local-only prepare-update handoff through the sole HostClient.
    pub async fn prepare_update(
        &self,
        admission: &FleetAdmission,
        command_id: CommandId,
        target_version: &str,
        client_build: &str,
        host_build: &str,
        allow_explicit_confirm_with_active: bool,
    ) -> Result<FleetOwned<UpdateHandoffToken>, FleetError> {
        if admission.task_id.is_some() {
            return Err(FleetError::AdmissionOwnerMismatch);
        }
        self.classify_request_support(&admission.host, FleetUnsupportedKind::PrepareUpdate)?;
        let driver = self.live(&admission.host)?;
        driver
            .shared
            .with_state(|state| state.admission_matches(admission))??;
        let admission = admission.clone();
        driver
            .request(|reply| DriverRequest::PrepareUpdate {
                admission,
                command_id,
                target_version: target_version.to_string(),
                client_build: client_build.to_string(),
                host_build: host_build.to_string(),
                allow_explicit_confirm_with_active,
                reply,
            })
            .await
    }

    pub async fn synchronize(&self, host: &HostId) -> Result<FleetOwned<()>, FleetError> {
        self.live(host)?
            .request(|reply| DriverRequest::Synchronize { reply })
            .await
    }

    /// Local-only reconnect through the sole owned [`HostClient::reconnect`].
    ///
    /// Retains the rotating reconnect grant and tracked Accepted operations.
    /// Remote hosts are rejected before I/O. Generation allocation is monotonic
    /// at enqueue so older reconnect admissions cannot regress a newer generation.
    pub async fn reconnect_local(&self, host: &HostId) -> Result<FleetOwned<u64>, FleetError> {
        if matches!(host, HostId::Remote(_)) {
            return Err(FleetError::UnsupportedRequest(
                FleetUnsupportedKind::ExplicitDetach,
            ));
        }
        let next_generation = self.alloc_generation();
        self.live(host)?
            .request(move |reply| DriverRequest::ReconnectLocal {
                next_generation,
                reply,
            })
            .await
    }

    /// Cached canonical subscription id (driver-owned; clone-free idle path).
    pub fn subscription_id(
        &self,
        host: &HostId,
    ) -> Result<FleetOwned<Option<SubscriptionId>>, FleetError> {
        let driver = self.live(host)?;
        driver
            .shared
            .with_state(|state| Ok(state.tag(state.subscription_id)))?
    }

    /// Drain snapshot-race replay under one lock (host-global admission only).
    ///
    /// Synchronous so cancel cannot strand ownership between validate and take.
    pub fn take_replay_events(
        &self,
        admission: &FleetAdmission,
    ) -> Result<FleetOwned<Vec<DomainEvent>>, FleetError> {
        if admission.task_id.is_some() {
            return Err(FleetError::AdmissionOwnerMismatch);
        }
        let driver = self.live(&admission.host)?;
        driver.shared.with_state(|state| {
            state.admission_matches(admission)?;
            let events = std::mem::take(&mut state.pending_replay);
            Ok(state.tag(events))
        })?
    }

    /// Wait for the next subscription event published by the host driver.
    pub async fn recv_subscription_update(
        &self,
        host: &HostId,
    ) -> Result<FleetOwned<SubscriptionUpdate>, FleetError> {
        let driver = self.live(host)?;
        loop {
            let notified = driver.shared.events_notify.notified();
            tokio::pin!(notified);
            let _ = notified.as_mut().enable();
            let status = driver.shared.with_state(|state| {
                if state.invalidated {
                    return Err(FleetError::HostFenced);
                }
                if !state.token.connected || state.token.fence != HostFence::Live {
                    return Err(FleetError::DisconnectedReadOnly);
                }
                if state.events_overflow || state.subscription_gap {
                    return Err(FleetError::Subscription(SubscriptionError::NeedsResync));
                }
                Ok(state.subscription_events.pop_front())
            })?;
            match status {
                Ok(Some(event)) => return Ok(event),
                Ok(None) => {
                    notified.await;
                }
                Err(error) => return Err(error),
            }
        }
    }

    pub async fn reconnect_with_factory(
        &self,
        host: &HostId,
        factory: HostClientFactory,
    ) -> Result<FleetOwned<u64>, FleetError> {
        let next_generation = self.alloc_generation();
        self.live(host)?
            .request(move |reply| DriverRequest::ReconnectFactory {
                factory,
                next_generation,
                reply,
            })
            .await
    }

    pub fn presentation_model(
        &self,
        host: &HostId,
    ) -> Result<Option<FleetOwned<Arc<ClientModel>>>, FleetError> {
        let driver = self.live(host)?;
        driver
            .shared
            .with_state(|state| state.cached_model.clone().map(|model| state.tag(model)))
    }

    pub fn merged_task_keys(&self) -> Result<Vec<HostTaskKey>, FleetError> {
        let hosts = self.hosts.lock().map_err(|_| FleetError::HostBusy)?;
        let mut keys = Vec::new();
        for (host_id, entry) in hosts.iter() {
            let HostEntry::Live(driver) = entry else {
                continue;
            };
            let Ok(guard) = driver.shared.state.lock() else {
                continue;
            };
            let Some(model) = guard.cached_model.as_ref() else {
                continue;
            };
            for task_id in model.tasks().keys().copied() {
                keys.push(HostTaskKey {
                    host: host_id.clone(),
                    task_id,
                });
            }
        }
        keys.sort();
        keys.dedup();
        Ok(keys)
    }

    /// Peek unacked retained outcomes (does not free capacity).
    pub fn retained_outcomes(
        &self,
        host: &HostId,
    ) -> Result<Vec<FleetOwned<FleetRetainedCommand>>, FleetError> {
        let driver = self.live(host)?;
        driver.shared.with_state(|state| {
            state
                .outcomes
                .iter()
                .filter(|entry| !entry.acked)
                .map(|entry| entry.owned.clone())
                .collect()
        })
    }

    /// Explicit outcome acknowledgement frees ledger capacity.
    pub fn acknowledge_retained(
        &self,
        host: &HostId,
        command_id: CommandId,
    ) -> Result<(), FleetError> {
        let driver = self.live(host)?;
        let token = driver.shared.snapshot_token()?;
        self.acknowledge_retained_owned(host, token.generation, token.client_id, command_id)
    }

    /// Ack exact owner+generation+client+command (no cross-generation alias).
    pub fn acknowledge_retained_owned(
        &self,
        host: &HostId,
        generation: u64,
        client_id: ClientId,
        command_id: CommandId,
    ) -> Result<(), FleetError> {
        let driver = self.live(host)?;
        driver.shared.with_state(|state| {
            let mut found = false;
            for entry in state.outcomes.iter_mut() {
                if entry.owned.host != *host
                    || entry.owned.generation != generation
                    || entry.owned.client_id != client_id
                {
                    continue;
                }
                let matches = match &entry.owned.value {
                    FleetRetainedCommand::Receipt(receipt) => receipt.command_id() == command_id,
                    FleetRetainedCommand::Uncertain(uncertain) => {
                        uncertain.command_id == command_id
                    }
                };
                if matches {
                    entry.acked = true;
                    found = true;
                    break;
                }
            }
            state.outcomes.retain(|entry| !entry.acked);
            if found {
                state.uncertain.retain(|uncertain| {
                    uncertain.admission.host != *host
                        || uncertain.admission.generation != generation
                        || uncertain.admission.client_id != client_id
                        || uncertain.command_id != command_id
                });
                Ok(())
            } else {
                Err(FleetError::HostNotFound)
            }
        })?
    }

    /// Compatibility peek that does not evict unresolved receipts.
    pub fn take_retained_outcomes(
        &self,
        host: &HostId,
        max: usize,
    ) -> Result<Vec<FleetOwned<FleetRetainedCommand>>, FleetError> {
        let mut items = self.retained_outcomes(host)?;
        items.truncate(max);
        Ok(items)
    }

    pub fn uncertain_commands(
        &self,
        host: &HostId,
    ) -> Result<Vec<FleetUncertainCommand>, FleetError> {
        let driver = self.live(host)?;
        driver
            .shared
            .with_state(|state| state.uncertain.iter().cloned().collect())
    }

    fn live(&self, host: &HostId) -> Result<Arc<HostDriver>, FleetError> {
        let hosts = self.hosts.lock().map_err(|_| FleetError::HostBusy)?;
        match hosts.get(host) {
            Some(HostEntry::Live(driver)) => Ok(Arc::clone(driver)),
            Some(HostEntry::Reserved(_)) => Err(FleetError::DisconnectedReadOnly),
            None => Err(FleetError::HostNotFound),
        }
    }

    #[cfg(test)]
    async fn tracked_operation_for_test(
        &self,
        host: &HostId,
        operation_id: crate::domain::id::OperationId,
    ) -> Result<Option<TrackedOperation>, FleetError> {
        let driver = self.live(host)?;
        let (tx, rx) = oneshot::channel();
        driver
            .tx
            .try_send(DriverRequest::InspectTracked {
                operation_id,
                reply: tx,
            })
            .map_err(|error| match error {
                mpsc::error::TrySendError::Full(_) => FleetError::QueueFull,
                mpsc::error::TrySendError::Closed(_) => FleetError::WorkerGone,
            })?;
        Ok(rx.await.map_err(|_| FleetError::WorkerGone)?)
    }

    #[cfg(test)]
    fn seed_cached_model_for_test(
        &self,
        host: &HostId,
        model: ClientModel,
    ) -> Result<(), FleetError> {
        let driver = self.live(host)?;
        driver.shared.with_state(|state| {
            state.cached_model = Some(Arc::new(model));
        })
    }
}

/// RAII install reservation: Drop removes only the exact generation Reserved slot.
struct InstallReservationGuard<'a> {
    fleet: &'a HostFleet,
    host_id: HostId,
    generation: u64,
    disarmed: bool,
}

impl InstallReservationGuard<'_> {
    fn disarm(&mut self) {
        self.disarmed = true;
    }
}

impl Drop for InstallReservationGuard<'_> {
    fn drop(&mut self) {
        if self.disarmed {
            return;
        }
        let Ok(mut hosts) = self.fleet.hosts.lock() else {
            return;
        };
        match hosts.get(&self.host_id) {
            Some(HostEntry::Reserved(reservation)) if reservation.generation == self.generation => {
                reservation.invalidated.store(true, Ordering::Release);
                let _ = hosts.remove(&self.host_id);
            }
            _ => {}
        }
    }
}

async fn wait_stop(stop: &BackgroundWorkStop) {
    stop.cancelled().await;
}

async fn wait_disconnect(shared: &HostDriverShared) {
    loop {
        let notified = shared.disconnect_notify.notified();
        tokio::pin!(notified);
        if notified.as_mut().enable() {
            if shared.fence_is_disconnect() {
                return;
            }
            continue;
        }
        if shared.fence_is_disconnect() {
            return;
        }
        notified.await;
        if shared.fence_is_disconnect() {
            return;
        }
    }
}

fn disconnect_driver(
    driver: Arc<HostDriver>,
    expected: Option<&FleetAdmission>,
) -> impl std::future::Future<Output = Result<FleetOwned<()>, FleetError>> + '_ {
    async move {
        let (tx, rx) = oneshot::channel();
        let captured = {
            let mut waiters = driver
                .shared
                .disconnect_waiters
                .lock()
                .map_err(|_| FleetError::HostBusy)?;
            waiters.retain(|waiter| !waiter.is_closed());
            if waiters.len() >= MAX_DRIVER_QUEUE {
                return Err(FleetError::QueueFull);
            }
            // Validate expected host/gen/client under the SAME lock that publishes
            // DisconnectRequested so a replacement generation cannot be fenced.
            let captured = driver.shared.with_state(|state| {
                if let Some(admission) = expected {
                    state.admission_generation_matches(admission)?;
                }
                if state.invalidated || matches!(state.token.fence, HostFence::RemoveRequested) {
                    return Err(FleetError::HostFenced);
                }
                if matches!(state.token.fence, HostFence::DisconnectRequested) {
                    if let Some(token) = state.disconnect_token.as_ref() {
                        return Ok(FleetOwned {
                            host: token.host.clone(),
                            generation: token.generation,
                            client_id: token.client_id,
                            task_id: None,
                            value: (),
                        });
                    }
                }
                let tagged = FleetOwned {
                    host: state.token.host.clone(),
                    generation: state.token.generation,
                    client_id: state.token.client_id,
                    task_id: None,
                    value: (),
                };
                state.token.fence = HostFence::DisconnectRequested;
                state.token.connected = false;
                state.disconnect_token = Some(OwnerToken {
                    host: tagged.host.clone(),
                    generation: tagged.generation,
                    client_id: tagged.client_id,
                    connected: false,
                    fence: HostFence::DisconnectRequested,
                    capabilities: state.token.capabilities.clone(),
                });
                Ok(tagged)
            })??;
            waiters.push(tx);
            captured
        };
        driver.shared.disconnect_notify.notify_waiters();
        driver.shared.events_notify.notify_waiters();
        match rx.await {
            Ok(result) => result,
            Err(_) => Ok(captured),
        }
    }
}

fn observe_client_connection(shared: &HostDriverShared, client: &HostClient) {
    let connected = client.is_connected();
    let _ = shared.with_state(|state| {
        state.token.connected = connected;
        if !connected {
            state.subscription_gap = true;
        }
    });
    if !connected {
        shared.events_notify.notify_waiters();
    }
}

fn complete_disconnect_waiters(
    shared: &HostDriverShared,
    result: Result<FleetOwned<()>, FleetError>,
) {
    let waiters = shared
        .disconnect_waiters
        .lock()
        .map(|mut guard| std::mem::take(&mut *guard))
        .unwrap_or_else(|poisoned| std::mem::take(&mut *poisoned.into_inner()));
    match result {
        Ok(owned) => {
            for waiter in waiters {
                let _ = waiter.send(Ok(owned.clone()));
            }
        }
        Err(error) => {
            for waiter in waiters {
                let terminal = match error {
                    FleetError::WorkerGone => FleetError::WorkerGone,
                    FleetError::HostBusy => FleetError::HostBusy,
                    _ => FleetError::HostFenced,
                };
                let _ = waiter.send(Err(terminal));
            }
        }
    }
}

fn apply_disconnect_now(
    client: &mut HostClient,
    subscription: &mut ClientSubscription,
    shared: &HostDriverShared,
) {
    if let Ok(Some(in_flight)) = shared.with_state(|state| state.in_flight.take()) {
        let _ = shared.with_state(|state| {
            state.push_uncertain_exact(FleetUncertainCommand {
                admission: in_flight.admission,
                command_id: in_flight.command_id,
            })
        });
    }
    if let Some(model) = subscription.model().cloned() {
        let _ = shared.with_state(|state| {
            state.cached_model = Some(Arc::new(model));
        });
    }
    // Preserve HostClient object (disconnected) so reconnect can absorb tracking.
    client.disconnect();
    let tagged = shared.with_state(|state| {
        let tagged = state
            .disconnect_token
            .as_ref()
            .map(|token| FleetOwned {
                host: token.host.clone(),
                generation: token.generation,
                client_id: token.client_id,
                task_id: None,
                value: (),
            })
            .unwrap_or_else(|| state.tag(()));
        // Retryable disconnected: keep installed, admissions remain fenced by connected=false.
        if !state.invalidated && !matches!(state.token.fence, HostFence::RemoveRequested) {
            state.token.connected = false;
            state.token.fence = HostFence::Live;
        }
        state.disconnect_token = None;
        tagged
    });
    match tagged {
        Ok(owned) => complete_disconnect_waiters(shared, Ok(owned)),
        Err(error) => complete_disconnect_waiters(shared, Err(error)),
    }
    shared.events_notify.notify_waiters();
}

async fn driver_loop(
    mut client: HostClient,
    mut subscription: ClientSubscription,
    mut request_rx: mpsc::Receiver<DriverRequest>,
    shared: Arc<HostDriverShared>,
    stop: BackgroundWorkStop,
) {
    loop {
        if stop.is_requested() {
            finalize_stop(&mut client, &mut subscription, &mut request_rx, &shared).await;
            break;
        }
        if shared.fence_is_disconnect() {
            apply_disconnect_now(&mut client, &mut subscription, &shared);
        }

        let subscription_ready = {
            let connected = shared
                .with_state(|state| state.token.connected)
                .unwrap_or(false);
            connected
                && matches!(subscription.state(), ClientSubscriptionState::Ready)
                && shared
                    .with_state(|state| !state.events_overflow && !state.subscription_gap)
                    .unwrap_or(false)
        };

        tokio::select! {
            _ = wait_stop(&stop) => {
                finalize_stop(&mut client, &mut subscription, &mut request_rx, &shared).await;
                break;
            }
            _ = wait_disconnect(&shared) => {
                apply_disconnect_now(&mut client, &mut subscription, &shared);
            }
            request = request_rx.recv() => {
                let Some(request) = request else {
                    finalize_stop(&mut client, &mut subscription, &mut request_rx, &shared).await;
                    break;
                };
                if stop.is_requested() {
                    reject_request(request, &shared, false);
                    finalize_stop(&mut client, &mut subscription, &mut request_rx, &shared).await;
                    break;
                }
                if shared.fence_is_disconnect() {
                    apply_disconnect_now(&mut client, &mut subscription, &shared);
                }
                handle_request(
                    request,
                    &mut client,
                    &mut subscription,
                    &shared,
                    &stop,
                ).await;
            }
            update = subscription.recv_and_apply(&client), if subscription_ready => {
                match update {
                    Ok(update) => {
                        let token = match shared.snapshot_token() {
                            Ok(token) => token,
                            Err(_) => continue,
                        };
                        let model_changed = matches!(
                            update,
                            SubscriptionUpdate::DurableEvent(_)
                                | SubscriptionUpdate::ConversationDirty { .. }
                        );
                        if model_changed {
                            if let Some(model) = subscription.model().cloned() {
                                let _ = shared.with_state(|state| {
                                    state.cached_model = Some(Arc::new(model));
                                });
                            }
                        }
                        let push = shared.with_state(|state| {
                            state.push_subscription_event(&token, update)
                        });
                        match push {
                            Ok(Ok(())) => shared.events_notify.notify_waiters(),
                            Ok(Err(_)) | Err(_) => {
                                let _ = shared.with_state(|state| {
                                    state.subscription_gap = true;
                                });
                                shared.events_notify.notify_waiters();
                            }
                        }
                    }
                    Err(SubscriptionError::NeedsResync) | Err(SubscriptionError::NotReady) => {
                        let _ = shared.with_state(|state| {
                            state.subscription_gap = true;
                        });
                        shared.events_notify.notify_waiters();
                    }
                    Err(_) => {
                        let _ = shared.with_state(|state| {
                            state.token.connected = client.is_connected();
                            state.subscription_gap = true;
                        });
                        shared.events_notify.notify_waiters();
                    }
                }
            }
        }
    }
}

async fn handle_request(
    request: DriverRequest,
    client: &mut HostClient,
    subscription: &mut ClientSubscription,
    shared: &Arc<HostDriverShared>,
    stop: &BackgroundWorkStop,
) {
    match request {
        DriverRequest::Execute {
            admission,
            envelope,
            reply,
        } => {
            let command_id = envelope.command_id;
            let prepare = shared.with_state(|state| {
                state.admission_matches(&admission)?;
                if !state.token.connected {
                    return Err(FleetError::DisconnectedReadOnly);
                }
                state.try_reserve_outcome_slot()?;
                state.in_flight = Some(InFlightCommand {
                    admission: admission.clone(),
                    command_id,
                });
                Ok(state.token.clone())
            });
            let token = match prepare {
                Ok(Ok(token)) => token,
                Ok(Err(error)) | Err(error) => {
                    let _ = reply.send(Err(error));
                    return;
                }
            };
            if !client.is_connected() {
                let _ = shared.with_state(|state| {
                    state.in_flight = None;
                    state.release_outcome_reservation();
                });
                let _ = reply.send(Err(FleetError::DisconnectedReadOnly));
                return;
            }
            if stop.is_requested() || shared.fence_is_disconnect() {
                let _ = shared.with_state(|state| {
                    state.in_flight = None;
                    state.release_outcome_reservation();
                });
                let _ = reply.send(Err(FleetError::HostFenced));
                if shared.fence_is_disconnect() {
                    apply_disconnect_now(client, subscription, shared);
                }
                return;
            }
            let outcome = tokio::select! {
                _ = wait_stop(stop) => {
                    client.disconnect();
                    Err(IpcError::Unavailable)
                }
                _ = wait_disconnect(shared) => {
                    client.disconnect();
                    Err(IpcError::Unavailable)
                }
                result = client.execute_command(envelope) => result,
            };
            deliver_command_result(
                shared,
                client,
                subscription,
                &token,
                &admission,
                command_id,
                outcome,
                reply,
                stop,
            );
        }
        DriverRequest::Query {
            admission,
            envelope,
            timeout,
            reply,
        } => {
            match shared.with_state(|state| state.admission_matches(&admission)) {
                Ok(Ok(())) => {}
                Ok(Err(error)) | Err(error) => {
                    let _ = reply.send(Err(error));
                    return;
                }
            }
            let token = match shared.snapshot_token() {
                Ok(token) => token,
                Err(error) => {
                    let _ = reply.send(Err(error));
                    return;
                }
            };
            let outcome = tokio::select! {
                _ = wait_stop(stop) => {
                    client.disconnect();
                    Err(IpcError::Unavailable)
                }
                _ = wait_disconnect(shared) => {
                    client.disconnect();
                    Err(IpcError::Unavailable)
                }
                result = async {
                    if let Query::OperationStatus { operation_id } = envelope.query {
                        let request_id = envelope.request_id;
                        match client.refresh_operation(operation_id).await {
                            Ok(Ok(state)) => Ok(QueryReply {
                                request_id,
                                outcome: crate::domain::query::QueryOutcome::Ok(
                                    crate::domain::query::QueryResult::OperationStatus {
                                        operation_id,
                                        state,
                                    },
                                ),
                            }),
                            Ok(Err(error)) => Ok(QueryReply {
                                request_id,
                                outcome: crate::domain::query::QueryOutcome::Err(error),
                            }),
                            Err(error) => Err(error),
                        }
                    } else {
                        client.query_with_timeout(envelope, timeout).await
                    }
                } => result,
            };
            if shared.fence_is_disconnect() {
                apply_disconnect_now(client, subscription, shared);
            }
            match outcome {
                Ok(value) => {
                    let _ = reply.send(Ok(FleetOwned {
                        host: token.host,
                        generation: token.generation,
                        client_id: token.client_id,
                        task_id: admission.task_id,
                        value,
                    }));
                }
                Err(error) => {
                    observe_client_connection(shared, client);
                    let _ = reply.send(Err(FleetError::from(error)));
                }
            }
        }
        DriverRequest::TerminalInput {
            admission,
            request,
            reply,
        } => {
            match shared.with_state(|state| state.admission_matches(&admission)) {
                Ok(Ok(())) => {}
                Ok(Err(error)) | Err(error) => {
                    let _ = reply.send(Err(error));
                    return;
                }
            }
            if matches!(admission.host, HostId::Remote(_)) {
                let _ = reply.send(Err(FleetError::UnsupportedRequest(
                    FleetUnsupportedKind::RawTerminalInput,
                )));
                return;
            }
            let token = match shared.snapshot_token() {
                Ok(token) => token,
                Err(error) => {
                    let _ = reply.send(Err(error));
                    return;
                }
            };
            let outcome = tokio::select! {
                _ = wait_stop(stop) => {
                    client.disconnect();
                    Err(IpcError::Unavailable)
                }
                _ = wait_disconnect(shared) => {
                    client.disconnect();
                    Err(IpcError::Unavailable)
                }
                result = client.execute_terminal_input(request) => result,
            };
            if shared.fence_is_disconnect() {
                apply_disconnect_now(client, subscription, shared);
            }
            match outcome {
                Ok(value) => {
                    let _ = reply.send(Ok(FleetOwned {
                        host: token.host,
                        generation: token.generation,
                        client_id: token.client_id,
                        task_id: admission.task_id,
                        value,
                    }));
                }
                Err(error) => {
                    observe_client_connection(shared, client);
                    let _ = reply.send(Err(FleetError::from(error)));
                }
            }
        }
        DriverRequest::PreviewTasks { admission, reply } => {
            match shared.with_state(|state| state.admission_matches(&admission)) {
                Ok(Ok(())) => {}
                Ok(Err(error)) | Err(error) => {
                    let _ = reply.send(Err(error));
                    return;
                }
            }
            let token = match shared.snapshot_token() {
                Ok(token) => token,
                Err(error) => {
                    let _ = reply.send(Err(error));
                    return;
                }
            };
            // Temporary subscription: never mutates/replaces the canonical one.
            let mut preview_subscription = ClientSubscription::new();
            let outcome = tokio::select! {
                _ = wait_stop(stop) => {
                    client.disconnect();
                    Err(SubscriptionError::Transport(IpcError::Unavailable))
                }
                _ = wait_disconnect(shared) => {
                    client.disconnect();
                    Err(SubscriptionError::Transport(IpcError::Unavailable))
                }
                result = preview_subscription.preview_tasks(client) => result,
            };
            drop(preview_subscription);
            if shared.fence_is_disconnect() {
                apply_disconnect_now(client, subscription, shared);
            }
            match outcome {
                Ok(value) => {
                    let _ = reply.send(Ok(FleetOwned {
                        host: token.host,
                        generation: token.generation,
                        client_id: token.client_id,
                        task_id: None,
                        value,
                    }));
                }
                Err(error) => {
                    observe_client_connection(shared, client);
                    let _ = reply.send(Err(FleetError::from(error)));
                }
            }
        }
        DriverRequest::Detach { admission, reply } => {
            match shared.with_state(|state| state.admission_matches(&admission)) {
                Ok(Ok(())) => {}
                Ok(Err(error)) | Err(error) => {
                    let _ = reply.send(Err(error));
                    return;
                }
            }
            if matches!(admission.host, HostId::Remote(_)) {
                let _ = reply.send(Err(FleetError::UnsupportedRequest(
                    FleetUnsupportedKind::ExplicitDetach,
                )));
                return;
            }
            let token = match shared.snapshot_token() {
                Ok(token) => token,
                Err(error) => {
                    let _ = reply.send(Err(error));
                    return;
                }
            };
            let outcome = tokio::select! {
                _ = wait_stop(stop) => {
                    client.disconnect();
                    Err(IpcError::Unavailable)
                }
                _ = wait_disconnect(shared) => {
                    client.disconnect();
                    Err(IpcError::Unavailable)
                }
                result = client.detach() => result,
            };
            if shared.fence_is_disconnect() {
                apply_disconnect_now(client, subscription, shared);
            }
            match outcome {
                Ok(value) => {
                    let _ = reply.send(Ok(FleetOwned {
                        host: token.host,
                        generation: token.generation,
                        client_id: token.client_id,
                        task_id: None,
                        value,
                    }));
                }
                Err(error) => {
                    observe_client_connection(shared, client);
                    let _ = reply.send(Err(FleetError::from(error)));
                }
            }
        }
        DriverRequest::PrepareUpdate {
            admission,
            command_id,
            target_version,
            client_build,
            host_build,
            allow_explicit_confirm_with_active,
            reply,
        } => {
            match shared.with_state(|state| state.admission_matches(&admission)) {
                Ok(Ok(())) => {}
                Ok(Err(error)) | Err(error) => {
                    let _ = reply.send(Err(error));
                    return;
                }
            }
            if matches!(admission.host, HostId::Remote(_)) {
                let _ = reply.send(Err(FleetError::UnsupportedRequest(
                    FleetUnsupportedKind::PrepareUpdate,
                )));
                return;
            }
            let token = match shared.snapshot_token() {
                Ok(token) => token,
                Err(error) => {
                    let _ = reply.send(Err(error));
                    return;
                }
            };
            let outcome = tokio::select! {
                _ = wait_stop(stop) => {
                    client.disconnect();
                    Err(IpcError::Unavailable)
                }
                _ = wait_disconnect(shared) => {
                    client.disconnect();
                    Err(IpcError::Unavailable)
                }
                result = client.prepare_update(
                    command_id,
                    &target_version,
                    &client_build,
                    &host_build,
                    allow_explicit_confirm_with_active,
                ) => result,
            };
            if shared.fence_is_disconnect() {
                apply_disconnect_now(client, subscription, shared);
            }
            match outcome {
                Ok(value) => {
                    let _ = reply.send(Ok(FleetOwned {
                        host: token.host,
                        generation: token.generation,
                        client_id: token.client_id,
                        task_id: None,
                        value,
                    }));
                }
                Err(error) => {
                    observe_client_connection(shared, client);
                    let _ = reply.send(Err(FleetError::from(error)));
                }
            }
        }
        DriverRequest::Synchronize { reply } => {
            let connected = shared
                .with_state(|state| state.token.connected)
                .unwrap_or(false);
            if !connected || !client.is_connected() {
                let _ = reply.send(Err(FleetError::DisconnectedReadOnly));
                return;
            }
            let token = match shared.snapshot_token() {
                Ok(token) => token,
                Err(error) => {
                    let _ = reply.send(Err(error));
                    return;
                }
            };
            if subscription.state() == ClientSubscriptionState::Ready {
                let gap = shared
                    .with_state(|state| state.events_overflow || state.subscription_gap)
                    .unwrap_or(true);
                if !gap {
                    // Repeated attach/refresh callers share the canonical live
                    // subscription; they must not restart a Ready subscription.
                    let _ = reply.send(Ok(FleetOwned {
                        host: token.host,
                        generation: token.generation,
                        client_id: token.client_id,
                        task_id: None,
                        value: (),
                    }));
                    return;
                }
                // The fleet's bounded presentation queue can detect a gap even
                // while the underlying subscription is still Ready. Retire its
                // exact handles through the existing resync lifecycle.
                subscription.observe_recv_transport_failure();
            }
            let outcome = tokio::select! {
                _ = wait_stop(stop) => {
                    client.disconnect();
                    Err(SubscriptionError::Transport(IpcError::Unavailable))
                }
                _ = wait_disconnect(shared) => {
                    client.disconnect();
                    Err(SubscriptionError::Transport(IpcError::Unavailable))
                }
                result = subscription.synchronize(client) => result,
            };
            if shared.fence_is_disconnect() {
                apply_disconnect_now(client, subscription, shared);
            }
            match outcome {
                Ok(()) => {
                    let publish = shared.with_state(|state| {
                        let new_id = subscription.subscription_id();
                        let mut fresh = subscription.take_replay_events();
                        let combined = state.pending_replay.len().saturating_add(fresh.len());
                        if combined > MAX_FLEET_REPLAY_EVENTS {
                            // Fail closed: retain unread prior handoff; do not
                            // publish a new subscription_id that would strand it.
                            state.subscription_gap = true;
                            return Err(FleetError::Subscription(
                                SubscriptionError::ReplayOverflow {
                                    limit: MAX_FLEET_REPLAY_EVENTS,
                                },
                            ));
                        }
                        state.clear_live_subscription_queue();
                        state.subscription_id = new_id;
                        state.pending_replay.append(&mut fresh);
                        if let Some(model) = subscription.model().cloned() {
                            state.cached_model = Some(Arc::new(model));
                        }
                        Ok(())
                    });
                    shared.events_notify.notify_waiters();
                    match publish {
                        Ok(Ok(())) => {
                            let _ = reply.send(Ok(FleetOwned {
                                host: token.host,
                                generation: token.generation,
                                client_id: token.client_id,
                                task_id: None,
                                value: (),
                            }));
                        }
                        Ok(Err(error)) | Err(error) => {
                            let _ = reply.send(Err(error));
                        }
                    }
                }
                Err(error) => {
                    let _ = shared.with_state(|state| {
                        state.token.connected = client.is_connected();
                        state.subscription_gap = true;
                        // Failed sync must not publish a new subscription_id.
                    });
                    shared.events_notify.notify_waiters();
                    let _ = reply.send(Err(FleetError::from(error)));
                }
            }
        }
        DriverRequest::ReconnectFactory {
            factory,
            next_generation,
            reply,
        } => {
            let result =
                run_reconnect_factory(client, subscription, shared, factory, next_generation, stop)
                    .await;
            let _ = reply.send(result);
        }
        DriverRequest::ReconnectLocal {
            next_generation,
            reply,
        } => {
            let result =
                run_reconnect_local(client, subscription, shared, next_generation, stop).await;
            let _ = reply.send(result);
        }
        #[cfg(test)]
        DriverRequest::InspectTracked {
            operation_id,
            reply,
        } => {
            let _ = reply.send(client.tracked_operation(operation_id).cloned());
        }
    }
}

/// `wire_started`: if false, queued/unsent work is rejected without uncertain.
fn reject_request(request: DriverRequest, shared: &HostDriverShared, wire_started: bool) {
    match request {
        DriverRequest::Execute {
            admission,
            envelope,
            reply,
        } => {
            if wire_started {
                let _ = shared.with_state(|state| {
                    state.push_uncertain_exact(FleetUncertainCommand {
                        admission,
                        command_id: envelope.command_id,
                    })
                });
            } else if let Ok(true) = shared.with_state(|state| {
                if state.outcome_reserved > 0 {
                    state.release_outcome_reservation();
                    true
                } else {
                    false
                }
            }) {
                // Reservation released for unsent work.
            }
            let _ = reply.send(Err(FleetError::HostFenced));
        }
        DriverRequest::Query { reply, .. } => {
            let _ = reply.send(Err(FleetError::HostFenced));
        }
        DriverRequest::TerminalInput { reply, .. } => {
            let _ = reply.send(Err(FleetError::HostFenced));
        }
        DriverRequest::PreviewTasks { reply, .. } => {
            let _ = reply.send(Err(FleetError::HostFenced));
        }
        DriverRequest::Detach { reply, .. } => {
            let _ = reply.send(Err(FleetError::HostFenced));
        }
        DriverRequest::PrepareUpdate { reply, .. } => {
            let _ = reply.send(Err(FleetError::HostFenced));
        }
        DriverRequest::Synchronize { reply } => {
            let _ = reply.send(Err(FleetError::HostFenced));
        }
        DriverRequest::ReconnectFactory { reply, .. } => {
            let _ = reply.send(Err(FleetError::HostFenced));
        }
        DriverRequest::ReconnectLocal { reply, .. } => {
            let _ = reply.send(Err(FleetError::HostFenced));
        }
        #[cfg(test)]
        DriverRequest::InspectTracked { reply, .. } => {
            let _ = reply.send(None);
        }
    }
}

async fn finalize_stop(
    client: &mut HostClient,
    subscription: &mut ClientSubscription,
    request_rx: &mut mpsc::Receiver<DriverRequest>,
    shared: &HostDriverShared,
) {
    if let Ok(Some(in_flight)) = shared.with_state(|state| state.in_flight.take()) {
        let _ = shared.with_state(|state| {
            state.push_uncertain_exact(FleetUncertainCommand {
                admission: in_flight.admission,
                command_id: in_flight.command_id,
            })
        });
    }
    if let Some(model) = subscription.model().cloned() {
        let _ = shared.with_state(|state| {
            state.cached_model = Some(Arc::new(model));
        });
    }
    client.disconnect();
    let _ = shared.with_state(|state| {
        state.invalidated = true;
        state.token.connected = false;
        state.token.fence = HostFence::RemoveRequested;
        state.retire_subscription_generation();
    });
    complete_disconnect_waiters(shared, Err(FleetError::HostFenced));
    shared.events_notify.notify_waiters();
    while let Ok(request) = request_rx.try_recv() {
        // Queued work never reached the wire.
        reject_request(request, shared, false);
    }
    shared.stopped.store(true, Ordering::Release);
    shared.stopped_notify.notify_waiters();
}

fn deliver_command_result(
    shared: &HostDriverShared,
    client: &mut HostClient,
    subscription: &mut ClientSubscription,
    token: &OwnerToken,
    admission: &FleetAdmission,
    command_id: CommandId,
    outcome: Result<CommandReceipt, IpcError>,
    reply: oneshot::Sender<Result<FleetOwned<CommandReceipt>, FleetError>>,
    stop: &BackgroundWorkStop,
) {
    let stop_requested = stop.is_requested();
    let disconnect_requested = shared.fence_is_disconnect();
    let publish = shared.with_state(|state| {
        state.in_flight = None;
        state.token.connected = client.is_connected();
        match outcome {
            Ok(receipt) => {
                // Successful wire receipt stays a receipt even if stop races after select.
                let retained = state.commit_reserved_receipt(token, admission, receipt.clone())?;
                let tagged = FleetOwned {
                    host: token.host.clone(),
                    generation: token.generation,
                    client_id: token.client_id,
                    task_id: admission.task_id,
                    value: receipt,
                };
                let _ = retained;
                Ok::<_, FleetError>((Ok(tagged), false))
            }
            Err(error) => {
                // Admitted wire failure: retain uncertain BEFORE any caller outcome.
                state.commit_reserved_uncertain(FleetUncertainCommand {
                    admission: admission.clone(),
                    command_id,
                })?;
                Ok((Err(FleetError::from(error)), true))
            }
        }
    });
    match publish {
        Ok(Ok((result, _))) => {
            let _ = reply.send(result);
        }
        Ok(Err(error)) | Err(error) => {
            // Never ignore failed retention of an admitted command.
            let _ = reply.send(Err(error));
        }
    }
    if disconnect_requested {
        apply_disconnect_now(client, subscription, shared);
    }
    let _ = stop_requested;
}

fn reset_retryable_disconnected(state: &mut OwnerState) {
    // A concurrent Disconnect owns its waiter until apply_disconnect_now.
    // Never erase that fence when a factory/validation error wins the select.
    if !state.invalidated
        && !matches!(
            state.token.fence,
            HostFence::RemoveRequested | HostFence::DisconnectRequested
        )
    {
        state.token.connected = false;
        state.token.fence = HostFence::Live;
    }
}

fn begin_reconnect_generation(
    client: &mut HostClient,
    subscription: &mut ClientSubscription,
    shared: &HostDriverShared,
    next_generation: u64,
) -> Result<(), FleetError> {
    shared.with_state(|state| {
        if state.invalidated
            || matches!(
                state.token.fence,
                HostFence::RemoveRequested | HostFence::DisconnectRequested
            )
        {
            return Err(FleetError::HostFenced);
        }
        if next_generation <= state.token.generation {
            return Err(FleetError::StaleGeneration);
        }
        if let Some(model) = subscription.model().cloned() {
            state.cached_model = Some(Arc::new(model));
        }
        state.retire_subscription_generation();
        state.token.generation = next_generation;
        state.token.connected = false;
        state.token.fence = HostFence::Reconnecting;
        Ok(())
    })??;
    shared.events_notify.notify_waiters();
    *subscription = ClientSubscription::new();
    let _ = client;
    Ok(())
}

fn finish_reconnect_publication(
    shared: &HostDriverShared,
    client: &HostClient,
    next_generation: u64,
) -> Result<FleetOwned<u64>, FleetError> {
    let client_id = client.client_id();
    let capabilities = client.granted_capabilities();
    let metadata = client.metadata().clone();
    shared.with_state(|state| {
        if state.invalidated || matches!(state.token.fence, HostFence::RemoveRequested) {
            return Err(FleetError::HostFenced);
        }
        if state.token.generation != next_generation {
            return Err(FleetError::StaleGeneration);
        }
        if matches!(state.token.fence, HostFence::DisconnectRequested) {
            return Err(FleetError::HostFenced);
        }
        state.clear_live_subscription_queue();
        state.subscription_id = None;
        state.pending_replay.clear();
        state.token.client_id = client_id;
        state.token.capabilities = capabilities;
        state.metadata = metadata;
        state.token.connected = true;
        state.token.fence = HostFence::Live;
        Ok(state.tag(next_generation))
    })?
}

async fn run_reconnect_local(
    client: &mut HostClient,
    subscription: &mut ClientSubscription,
    shared: &HostDriverShared,
    next_generation: u64,
    stop: &BackgroundWorkStop,
) -> Result<FleetOwned<u64>, FleetError> {
    begin_reconnect_generation(client, subscription, shared, next_generation)?;
    // HostClient::reconnect clears and rebuilds the exact local connection while
    // retaining tracked Accepted ops and rotating the reconnect grant.
    // The local Hello rotates a one-shot reconnect grant. Once started, its
    // bounded exchange must settle even if the UI cancels: dropping the future
    // after host admission can lose the replacement grant and strand this client.
    // The driver remains owned/joinable and observes stop immediately afterward.
    if stop.is_requested() || shared.fence_is_disconnect() {
        client.disconnect();
        if shared.fence_is_disconnect() {
            apply_disconnect_now(client, subscription, shared);
        }
        let _ = shared.with_state(reset_retryable_disconnected);
        return Err(FleetError::HostFenced);
    }
    if let Err(error) = client.reconnect().await {
        let _ = shared.with_state(reset_retryable_disconnected);
        return Err(FleetError::from(error));
    }
    if stop.is_requested() {
        client.disconnect();
        let _ = shared.with_state(reset_retryable_disconnected);
        return Err(FleetError::HostFenced);
    }
    if shared.fence_is_disconnect() {
        apply_disconnect_now(client, subscription, shared);
        return Err(FleetError::HostFenced);
    }
    let host_id = match shared.snapshot_token() {
        Ok(token) => token.host,
        Err(error) => {
            let _ = shared.with_state(reset_retryable_disconnected);
            return Err(error);
        }
    };
    if let Err(error) = validate_client_metadata(&host_id, client) {
        client.disconnect();
        let _ = shared.with_state(reset_retryable_disconnected);
        return Err(error);
    }
    if !client.is_connected() {
        client.disconnect();
        let _ = shared.with_state(reset_retryable_disconnected);
        return Err(FleetError::DisconnectedReadOnly);
    }
    finish_reconnect_publication(shared, client, next_generation)
}

async fn run_reconnect_factory(
    client: &mut HostClient,
    subscription: &mut ClientSubscription,
    shared: &HostDriverShared,
    factory: HostClientFactory,
    next_generation: u64,
    stop: &BackgroundWorkStop,
) -> Result<FleetOwned<u64>, FleetError> {
    begin_reconnect_generation(client, subscription, shared, next_generation)?;
    client.disconnect();

    let mut replacement = tokio::select! {
        _ = wait_stop(stop) => {
            let _ = shared.with_state(reset_retryable_disconnected);
            return Err(FleetError::HostFenced);
        }
        _ = wait_disconnect(shared) => {
            apply_disconnect_now(client, subscription, shared);
            return Err(FleetError::HostFenced);
        }
        result = factory() => match result {
            Ok(client) => client,
            Err(error) => {
                let _ = shared.with_state(reset_retryable_disconnected);
                return Err(FleetError::from(error));
            }
        },
    };

    if stop.is_requested() {
        replacement.disconnect();
        let _ = shared.with_state(reset_retryable_disconnected);
        return Err(FleetError::HostFenced);
    }
    if shared.fence_is_disconnect() {
        replacement.disconnect();
        apply_disconnect_now(client, subscription, shared);
        return Err(FleetError::HostFenced);
    }

    let host_id = match shared.snapshot_token() {
        Ok(token) => token.host,
        Err(error) => {
            replacement.disconnect();
            let _ = shared.with_state(reset_retryable_disconnected);
            return Err(error);
        }
    };

    if let Err(error) = validate_client_metadata(&host_id, &replacement) {
        replacement.disconnect();
        let _ = shared.with_state(reset_retryable_disconnected);
        return Err(error);
    }
    if !replacement.is_connected() {
        replacement.disconnect();
        let _ = shared.with_state(reset_retryable_disconnected);
        return Err(FleetError::DisconnectedReadOnly);
    }
    if let Err(error) = replacement.absorb_tracked_operations(client) {
        replacement.disconnect();
        let _ = shared.with_state(reset_retryable_disconnected);
        return Err(FleetError::from(error));
    }

    *client = replacement;
    finish_reconnect_publication(shared, client, next_generation)
}

fn subscription_update_task_id(update: &SubscriptionUpdate) -> Option<TaskId> {
    match update {
        SubscriptionUpdate::ConversationDirty { task_id, .. } => Some(*task_id),
        SubscriptionUpdate::DurableEvent(event) => event.task_id,
        SubscriptionUpdate::Stream(_) | SubscriptionUpdate::ResyncRequired { .. } => None,
    }
}

fn validate_query_admission(
    admission: &FleetAdmission,
    envelope: &QueryEnvelope,
) -> Result<(), FleetError> {
    if envelope.client_id != admission.client_id {
        return Err(FleetError::AdmissionOwnerMismatch);
    }
    match (admission.task_id, envelope.task_id) {
        (Some(admitted), Some(wire)) if admitted == wire => Ok(()),
        (Some(_), Some(_)) | (None, Some(_)) => Err(FleetError::AdmissionOwnerMismatch),
        (_, None) => Ok(()),
    }
}

fn validate_command_admission(
    admission: &FleetAdmission,
    envelope: &CommandEnvelope,
) -> Result<(), FleetError> {
    if envelope.client_id != admission.client_id {
        return Err(FleetError::AdmissionOwnerMismatch);
    }
    match (admission.task_id, envelope.task_id) {
        (Some(admitted), Some(wire)) if admitted == wire => Ok(()),
        (Some(_), Some(_)) | (None, Some(_)) => Err(FleetError::AdmissionOwnerMismatch),
        (_, None) => Ok(()),
    }
}

fn reject_remote_shutdown_or_update(host: &HostId, command: &Command) -> Result<(), FleetError> {
    if !matches!(host, HostId::Remote(_)) {
        return Ok(());
    }
    match command {
        Command::ConfirmHostQuit(_) => Err(FleetError::UnsupportedRequest(
            FleetUnsupportedKind::ExplicitDetach,
        )),
        Command::PrepareUpdate(_)
        | Command::ConfirmUpdateDrain(_)
        | Command::ArmUpdateInstall(_)
        | Command::AbortUpdateHandoff => Err(FleetError::UnsupportedRequest(
            FleetUnsupportedKind::PrepareUpdate,
        )),
        _ => Ok(()),
    }
}

fn validate_client_metadata(host_id: &HostId, client: &HostClient) -> Result<(), FleetError> {
    match host_id {
        HostId::LocalProfile(profile) => {
            let hello = client
                .metadata()
                .as_local()
                .ok_or(FleetError::HostMetadataMismatch)?;
            let expected = profile_fingerprint_for_named_profile(profile)
                .map_err(|_| FleetError::HostMetadataMismatch)?;
            if hello.profile_fingerprint != expected {
                return Err(FleetError::HostMetadataMismatch);
            }
            Ok(())
        }
        HostId::Remote(expected) => {
            let session = client
                .metadata()
                .as_connect()
                .ok_or(FleetError::HostMetadataMismatch)?;
            if session.host_public_id() != *expected {
                return Err(FleetError::HostMetadataMismatch);
            }
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::connection::{ClientConnection, DelayedCommandWireHandle};
    use crate::client::host_client::{HostClient, HostClientConfig, TrackedOperation};
    use crate::client::model::ClientModelBuilder;
    use crate::domain::command::CommandReceipt;
    use crate::domain::id::{EnvironmentId, OperationId, ProjectId, RequestId, SnapshotId};
    use crate::domain::operation::OperationState;
    use crate::domain::query::{Query, QueryEnvelope, QueryOutcome, QueryReply, QueryResult};
    use crate::domain::snapshot::{SnapshotItem, SnapshotPage, SnapshotSection, TaskSnapshotItem};
    use crate::domain::task::{
        ReviewReadiness, TaskActivity, TaskAssignment, TaskAttention, TaskConnectivity, TaskFacts,
        TaskLifecycle, WorkspaceRef,
    };
    use crate::protocol::{
        Capability, CapabilitySet, FrameLimits, ProfileFingerprint, ServerHello, PROTOCOL_MAJOR,
        PROTOCOL_MINOR,
    };
    use std::collections::BTreeMap;
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::sync::oneshot;

    fn fixed_uuid_v7(tail: u8) -> [u8; 16] {
        [
            0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, tail,
        ]
    }

    fn test_hello(profile: &str, connection_tail: u8) -> ServerHello {
        let normalized = match AppProfile::named(profile).expect("profile") {
            AppProfile::Named(name) => name,
            other => panic!("expected named, got {other:?}"),
        };
        ServerHello {
            protocol_major: PROTOCOL_MAJOR,
            protocol_minor: PROTOCOL_MINOR,
            server_build: "devmanager-host/fleet-test".into(),
            host_boot_id: uuid::Uuid::from_bytes(fixed_uuid_v7(0xb0)),
            connection_id: uuid::Uuid::from_bytes(fixed_uuid_v7(connection_tail)),
            profile_fingerprint: ProfileFingerprint::hash_normalized(&normalized),
            granted: CapabilitySet::from_capabilities([
                Capability::PagedSnapshots,
                Capability::EventReplay,
                Capability::ProviderInput,
                Capability::OperationSettlement,
            ]),
            limits: FrameLimits::v1_default(),
            reconnect_grant: None,
        }
    }

    fn local_client_with_tracked(
        profile: &str,
        client_tail: u8,
        connection_tail: u8,
        tracked: BTreeMap<OperationId, TrackedOperation>,
    ) -> HostClient {
        let normalized = match AppProfile::named(profile).expect("profile") {
            AppProfile::Named(name) => name,
            other => panic!("expected named, got {other:?}"),
        };
        let client_id = ClientId::from_bytes(fixed_uuid_v7(client_tail)).expect("client");
        let hello = test_hello(profile, connection_tail);
        let stub = ClientConnection::inert_stub_for_test(client_id, hello.clone());
        HostClient::from_parts_for_test(
            HostClientConfig {
                named_profile: normalized,
                client_build: "devmanager/fleet-test".into(),
                client_id,
                requested: CapabilitySet::from_capabilities([
                    Capability::PagedSnapshots,
                    Capability::EventReplay,
                    Capability::ProviderInput,
                    Capability::OperationSettlement,
                ]),
                limits: FrameLimits::v1_default(),
            },
            hello,
            Some(stub),
            tracked,
        )
    }

    fn local_client(profile: &str, client_tail: u8, connection_tail: u8) -> HostClient {
        local_client_with_tracked(profile, client_tail, connection_tail, BTreeMap::new())
    }

    fn local_client_with_wire(
        profile: &str,
        client_tail: u8,
        connection_tail: u8,
        tracked: BTreeMap<OperationId, TrackedOperation>,
    ) -> (HostClient, DelayedCommandWireHandle) {
        let normalized = match AppProfile::named(profile).expect("profile") {
            AppProfile::Named(name) => name,
            other => panic!("expected named, got {other:?}"),
        };
        let client_id = ClientId::from_bytes(fixed_uuid_v7(client_tail)).expect("client");
        let hello = test_hello(profile, connection_tail);
        let (wire, handle) =
            ClientConnection::delayed_command_wire_for_test(client_id, hello.clone());
        let client = HostClient::from_parts_for_test(
            HostClientConfig {
                named_profile: normalized,
                client_build: "devmanager/fleet-test".into(),
                client_id,
                requested: CapabilitySet::from_capabilities([
                    Capability::PagedSnapshots,
                    Capability::EventReplay,
                    Capability::ProviderInput,
                    Capability::OperationSettlement,
                ]),
                limits: FrameLimits::v1_default(),
            },
            hello,
            Some(wire),
            tracked,
        );
        (client, handle)
    }

    fn task_model(task_id: TaskId, title: &str) -> ClientModel {
        let snap = SnapshotId::from_bytes(fixed_uuid_v7(0xa0)).expect("snapshot");
        let mut builder = ClientModelBuilder::new();
        for section in [
            SnapshotSection::Tasks,
            SnapshotSection::AgentSessions,
            SnapshotSection::Artifacts,
            SnapshotSection::Resources,
            SnapshotSection::Operations,
        ] {
            let items = if section == SnapshotSection::Tasks {
                vec![SnapshotItem::Task(TaskSnapshotItem {
                    task: TaskFacts {
                        id: task_id,
                        environment_id: EnvironmentId::from_bytes(fixed_uuid_v7(0xa1))
                            .expect("env"),
                        title: title.into(),
                        description: None,
                        project_id: ProjectId::from_bytes(fixed_uuid_v7(0xa2)).expect("project"),
                        workspace: WorkspaceRef::Main,
                        assignment: TaskAssignment::LocalOwner,
                        lifecycle: TaskLifecycle::Open,
                        action_epoch: 0,
                        revision: 1,
                        created_at_ms: 1,
                    },
                    connectivity: TaskConnectivity::Connected,
                    attention: TaskAttention::None,
                    activity: TaskActivity::Idle,
                    review_readiness: ReviewReadiness::NotReady,
                    primary_agent_id: None,
                })]
            } else {
                Vec::new()
            };
            builder
                .ingest_page(SnapshotPage {
                    snapshot_id: snap,
                    through_sequence: 1,
                    section,
                    after_item: None,
                    items,
                    encoded_bytes: 1,
                    next_cursor: None,
                })
                .expect("section");
        }
        builder.finish().expect("model")
    }

    fn accepted_receipt(command_id: CommandId, operation_id: OperationId) -> CommandReceipt {
        CommandReceipt::Accepted {
            command_id,
            operation_id,
            task_revision: Some(1),
            event_ids: Vec::new(),
            prompt_mutation: None,
        }
    }

    #[tokio::test]
    async fn acknowledging_uncertain_outcomes_releases_all_retention_capacity() {
        let host = HostId::local_profile("fleet_ack_capacity").unwrap();
        let fleet = HostFleet::new();
        fleet
            .install(host.clone(), local_client("fleet_ack_capacity", 0xa1, 0xa2))
            .unwrap();
        let admission = fleet
            .admit_action(HostTaskKey::new(host.clone(), TaskId::new()))
            .unwrap();
        let driver = fleet.live(&host).unwrap();
        for _ in 0..(MAX_UNCERTAIN + 2) {
            let command_id = CommandId::new();
            driver
                .shared
                .with_state(|state| {
                    state.try_reserve_outcome_slot().unwrap();
                    state
                        .commit_reserved_uncertain(FleetUncertainCommand {
                            admission: admission.clone(),
                            command_id,
                        })
                        .unwrap();
                })
                .unwrap();
            fleet
                .acknowledge_retained_owned(
                    &host,
                    admission.generation,
                    admission.client_id,
                    command_id,
                )
                .unwrap();
            assert!(fleet.uncertain_commands(&host).unwrap().is_empty());
        }
        fleet.remove(&host).await.unwrap();
    }

    #[tokio::test]
    async fn full_removal_history_keeps_live_host_and_its_slot_unchanged() {
        let host = HostId::local_profile("fleet_remove_capacity").unwrap();
        let fleet = HostFleet::new();
        let generation = fleet
            .install(
                host.clone(),
                local_client("fleet_remove_capacity", 0xa3, 0xa4),
            )
            .unwrap();
        {
            let mut ledgers = fleet.removal_ledgers.lock().unwrap();
            for index in 0..MAX_REMOVAL_LEDGERS {
                let old_generation = 10_000 + index as u64;
                ledgers.insert(
                    (host.clone(), old_generation),
                    FleetRemoval {
                        host: host.clone(),
                        generation: old_generation,
                        client_id: None,
                        retained: Vec::new(),
                        uncertain: Vec::new(),
                    },
                );
            }
        }
        assert!(matches!(
            fleet.remove(&host).await,
            Err(FleetError::QueueFull)
        ));
        assert_eq!(fleet.generation(&host).unwrap(), generation);
        assert!(fleet.is_connected(&host).unwrap());
        assert!(fleet.draining.lock().unwrap().is_empty());
        fleet.acknowledge_removal_ledger(&host, 10_000).unwrap();
        fleet.remove(&host).await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn remove_at_generation_leaves_replacement_after_reconnect() {
        let host = HostId::local_profile("fleet_remove_at_gen").expect("host");
        let fleet = HostFleet::new();
        let (client, _wire0) =
            local_client_with_wire("fleet_remove_at_gen", 0x61, 0x62, BTreeMap::new());
        let original = fleet.install(host.clone(), client).expect("install");
        assert_eq!(original, 1);

        let (replacement, _wire1) =
            local_client_with_wire("fleet_remove_at_gen", 0x61, 0x63, BTreeMap::new());
        let owned = fleet
            .reconnect_with_factory(
                &host,
                Box::new(move || Box::pin(async move { Ok(replacement) })),
            )
            .await
            .expect("reconnect");
        assert_eq!(owned.value, 2);
        assert_eq!(fleet.generation(&host).expect("gen"), 2);

        let stale = fleet.remove_at_generation(&host, original).await;
        assert!(
            matches!(stale, Err(FleetError::StaleGeneration)),
            "stale remove must not touch replacement: {stale:?}"
        );
        assert_eq!(fleet.generation(&host).expect("still installed"), 2);
        assert!(fleet.is_connected(&host).expect("connected"));
        assert!(fleet.draining.lock().expect("draining").is_empty());

        let removal = fleet
            .remove_at_generation(&host, 2)
            .await
            .expect("exact remove");
        assert_eq!(removal.generation, 2);
        assert!(!fleet.contains(&host));
    }

    #[tokio::test]
    async fn remove_at_generation_rejects_stale_reserved_install() {
        let host = HostId::local_profile("fleet_remove_reserved").expect("host");
        let fleet = HostFleet::new();
        let generation = {
            let mut hosts = fleet.hosts.lock().expect("hosts");
            let generation = fleet.alloc_generation();
            hosts.insert(
                host.clone(),
                HostEntry::Reserved(HostReservation {
                    generation,
                    invalidated: AtomicBool::new(false),
                }),
            );
            generation
        };
        let stale = fleet
            .remove_at_generation(&host, generation.wrapping_add(1))
            .await;
        assert!(matches!(stale, Err(FleetError::StaleGeneration)));
        assert!(fleet.contains(&host));
        let removal = fleet
            .remove_at_generation(&host, generation)
            .await
            .expect("exact reserved remove");
        assert_eq!(removal.generation, generation);
        assert!(removal.client_id.is_none());
        assert!(!fleet.contains(&host));
    }

    #[test]
    fn same_task_id_on_two_hosts_is_distinct() {
        let task = TaskId::from_bytes(fixed_uuid_v7(0x21)).expect("task");
        let host_a = HostId::local_profile("fleet_drv_a").expect("a");
        let host_b = HostId::local_profile("fleet_drv_b").expect("b");
        let fleet = HostFleet::new();
        fleet
            .install(host_a.clone(), local_client("fleet_drv_a", 0x31, 0x32))
            .expect("a");
        fleet
            .install(host_b.clone(), local_client("fleet_drv_b", 0x33, 0x34))
            .expect("b");
        fleet
            .seed_cached_model_for_test(&host_a, task_model(task, "A"))
            .expect("cache a");
        fleet
            .seed_cached_model_for_test(&host_b, task_model(task, "B"))
            .expect("cache b");
        let merged = fleet.merged_task_keys().expect("merged");
        assert_eq!(merged.len(), 2);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pending_wire_caller_cancel_retains_exact_command_id_once() {
        let task = TaskId::from_bytes(fixed_uuid_v7(0x41)).expect("task");
        let host = HostId::local_profile("fleet_wire_cancel").expect("host");
        let fleet = Arc::new(HostFleet::new());
        let (client, wire) =
            local_client_with_wire("fleet_wire_cancel", 0x42, 0x43, BTreeMap::new());
        fleet.install(host.clone(), client).expect("install");
        let admission = fleet
            .admit_action(HostTaskKey::new(host.clone(), task))
            .expect("admit");
        let command_id = CommandId::from_bytes(fixed_uuid_v7(0x44)).expect("cmd");
        let operation_id = OperationId::from_bytes(fixed_uuid_v7(0x45)).expect("op");
        let fleet_task = Arc::clone(&fleet);
        let admission_task = admission.clone();
        let join = tokio::spawn(async move {
            fleet_task
                .execute_command(
                    &admission_task,
                    CommandEnvelope {
                        command_id,
                        client_id: admission_task.client_id,
                        task_id: Some(task),
                        issued_at_ms: 1,
                        expected_task_revision: None,
                        command: crate::domain::command::Command::SettleTask,
                    },
                )
                .await
        });
        let admitted = wire.wait_command_admitted().await;
        assert_eq!(admitted, command_id);
        join.abort();
        let _ = join.await;
        wire.dispatch_accepted(accepted_receipt(command_id, operation_id))
            .await
            .expect("dispatch");
        let mut saw = 0;
        for _ in 0..100 {
            tokio::task::yield_now().await;
            let retained = fleet.retained_outcomes(&host).expect("outcomes");
            saw = retained
                .iter()
                .filter(|owned| match &owned.value {
                    FleetRetainedCommand::Receipt(receipt) => receipt.command_id() == command_id,
                    FleetRetainedCommand::Uncertain(uncertain) => {
                        uncertain.command_id == command_id
                    }
                })
                .count();
            if saw >= 1 {
                break;
            }
        }
        assert_eq!(saw, 1, "receipt retained exactly once");
        assert!(!wire.is_poisoned());
        assert!(!wire.has_command_waiter(command_id));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancel_after_receipt_send_still_retains_once() {
        let task = TaskId::from_bytes(fixed_uuid_v7(0x46)).expect("task");
        let host = HostId::local_profile("fleet_wire_after").expect("host");
        let fleet = Arc::new(HostFleet::new());
        let (client, wire) =
            local_client_with_wire("fleet_wire_after", 0x47, 0x48, BTreeMap::new());
        fleet.install(host.clone(), client).expect("install");
        let admission = fleet
            .admit_action(HostTaskKey::new(host.clone(), task))
            .expect("admit");
        let command_id = CommandId::from_bytes(fixed_uuid_v7(0x49)).expect("cmd");
        let operation_id = OperationId::from_bytes(fixed_uuid_v7(0x4a)).expect("op");
        let fleet_task = Arc::clone(&fleet);
        let admission_task = admission.clone();
        let join = tokio::spawn(async move {
            fleet_task
                .execute_command(
                    &admission_task,
                    CommandEnvelope {
                        command_id,
                        client_id: admission_task.client_id,
                        task_id: Some(task),
                        issued_at_ms: 1,
                        expected_task_revision: None,
                        command: crate::domain::command::Command::SettleTask,
                    },
                )
                .await
        });
        let _ = wire.wait_command_admitted().await;
        wire.dispatch_accepted(accepted_receipt(command_id, operation_id))
            .await
            .expect("dispatch");
        let _ = tokio::time::timeout(Duration::from_secs(2), join)
            .await
            .expect("join")
            .expect("task");
        let retained = fleet.retained_outcomes(&host).expect("outcomes");
        let count = retained
            .iter()
            .filter(|owned| match &owned.value {
                FleetRetainedCommand::Receipt(receipt) => receipt.command_id() == command_id,
                _ => false,
            })
            .count();
        assert_eq!(count, 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn reconnect_preserves_pending_and_settles_via_query() {
        let task = TaskId::from_bytes(fixed_uuid_v7(0x50)).expect("task");
        let host = HostId::local_profile("fleet_factory_retry").expect("host");
        let op = OperationId::from_bytes(fixed_uuid_v7(0x51)).expect("op");
        let cmd = CommandId::from_bytes(fixed_uuid_v7(0x52)).expect("cmd");
        let fleet = HostFleet::new();
        let (client, _wire0) = local_client_with_wire(
            "fleet_factory_retry",
            0x53,
            0x54,
            BTreeMap::from([(op, TrackedOperation::Pending { command_id: cmd })]),
        );
        fleet.install(host.clone(), client).expect("install");
        let fail = fleet
            .reconnect_with_factory(
                &host,
                Box::new(|| Box::pin(async { Err(IpcError::Unavailable) })),
            )
            .await;
        assert!(matches!(fail, Err(FleetError::Ipc(IpcError::Unavailable))));
        assert_eq!(fleet.generation(&host).expect("gen"), 2);

        let (replacement, wire) =
            local_client_with_wire("fleet_factory_retry", 0x53, 0x55, BTreeMap::new());
        let ok = fleet
            .reconnect_with_factory(
                &host,
                Box::new(move || Box::pin(async move { Ok(replacement) })),
            )
            .await
            .expect("retry");
        assert_eq!(ok.value, 3);
        assert!(fleet.is_connected(&host).expect("connected"));
        let tracked = fleet
            .tracked_operation_for_test(&host, op)
            .await
            .expect("inspect");
        assert_eq!(tracked, Some(TrackedOperation::Pending { command_id: cmd }));

        let admission = fleet
            .admit_action(HostTaskKey::new(host.clone(), task))
            .expect("admit");
        let fleet_query = Arc::new(fleet);
        let host_q = host.clone();
        let admission_q = admission.clone();
        let query_task = {
            let fleet_query = Arc::clone(&fleet_query);
            tokio::spawn(async move {
                fleet_query
                    .query(
                        &admission_q,
                        QueryEnvelope {
                            request_id: RequestId::from_bytes(fixed_uuid_v7(0x56)).expect("req"),
                            client_id: admission_q.client_id,
                            task_id: Some(task),
                            query: Query::OperationStatus { operation_id: op },
                        },
                    )
                    .await
            })
        };
        let wire_request_id = wire.wait_query_admitted().await;
        wire.dispatch_query_reply(QueryReply {
            request_id: wire_request_id,
            outcome: QueryOutcome::Ok(QueryResult::OperationStatus {
                operation_id: op,
                state: OperationState::Settled {
                    settled_at_ms: 10,
                    result_event_ids: Vec::new(),
                },
            }),
        })
        .await
        .expect("query reply");
        let _ = tokio::time::timeout(Duration::from_secs(2), query_task)
            .await
            .expect("query join")
            .expect("query task")
            .expect("query ok");
        let settled = fleet_query
            .tracked_operation_for_test(&host_q, op)
            .await
            .expect("inspect settled");
        assert!(matches!(
            settled,
            Some(TrackedOperation::Resolved { command_id, .. }) if command_id == cmd
        ));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn remove_during_factory_invalidates_reservation() {
        let host = HostId::local_profile("fleet_reserve_race").expect("host");
        let fleet = Arc::new(HostFleet::new());
        let (start_tx, start_rx) = oneshot::channel::<()>();
        let (release_tx, release_rx) = oneshot::channel::<()>();
        let fleet_install = Arc::clone(&fleet);
        let host_install = host.clone();
        let install = tokio::spawn(async move {
            fleet_install
                .install_with_factory(
                    host_install,
                    Box::new(move || {
                        Box::pin(async move {
                            let _ = start_tx.send(());
                            let _ = release_rx.await;
                            Ok(local_client("fleet_reserve_race", 0x61, 0x62))
                        })
                    }),
                )
                .await
        });
        let _ = start_rx.await;
        let removed = fleet.remove(&host).await.expect("remove reservation");
        let _ = release_tx.send(());
        let result = install.await.expect("join");
        assert!(matches!(result, Err(FleetError::StaleReservation)));
        assert!(!fleet.contains(&host));
        assert!(removed.generation >= 1);
        assert!(removed.client_id.is_none());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn stale_install_cannot_remove_newer_install() {
        let host = HostId::local_profile("fleet_stale_inst").expect("host");
        let fleet = Arc::new(HostFleet::new());
        let (start_tx, start_rx) = oneshot::channel::<()>();
        let (release_tx, release_rx) = oneshot::channel::<()>();
        let fleet_a = Arc::clone(&fleet);
        let host_a = host.clone();
        let first = tokio::spawn(async move {
            fleet_a
                .install_with_factory(
                    host_a,
                    Box::new(move || {
                        Box::pin(async move {
                            let _ = start_tx.send(());
                            let _ = release_rx.await;
                            Ok(local_client("fleet_stale_inst", 0x63, 0x64))
                        })
                    }),
                )
                .await
        });
        let _ = start_rx.await;
        let _ = fleet.remove(&host).await.expect("clear reservation");
        let gen = fleet
            .install(host.clone(), local_client("fleet_stale_inst", 0x65, 0x66))
            .expect("newer install");
        let _ = release_tx.send(());
        let first_result = first.await.expect("join");
        assert!(matches!(first_result, Err(FleetError::StaleReservation)));
        assert!(fleet.contains(&host));
        assert_eq!(fleet.generation(&host).expect("gen"), gen);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn remove_during_pending_send_under_timeout() {
        let task = TaskId::from_bytes(fixed_uuid_v7(0x71)).expect("task");
        let host = HostId::local_profile("fleet_remove_send").expect("host");
        let other = HostId::local_profile("fleet_remove_other").expect("other");
        let fleet = Arc::new(HostFleet::new());
        let (client, wire) =
            local_client_with_wire("fleet_remove_send", 0x72, 0x73, BTreeMap::new());
        fleet.install(host.clone(), client).expect("install");
        fleet
            .install(
                other.clone(),
                local_client("fleet_remove_other", 0x75, 0x76),
            )
            .expect("other");
        let admission = fleet
            .admit_action(HostTaskKey::new(host.clone(), task))
            .expect("admit");
        let fleet_task = Arc::clone(&fleet);
        let admission_task = admission.clone();
        let send = tokio::spawn(async move {
            fleet_task
                .execute_command(
                    &admission_task,
                    CommandEnvelope {
                        command_id: CommandId::from_bytes(fixed_uuid_v7(0x74)).expect("cmd"),
                        client_id: admission_task.client_id,
                        task_id: Some(task),
                        issued_at_ms: 1,
                        expected_task_revision: None,
                        command: crate::domain::command::Command::SettleTask,
                    },
                )
                .await
        });
        let _ = wire.wait_command_admitted().await;
        let remove = tokio::time::timeout(Duration::from_secs(3), fleet.remove(&host))
            .await
            .expect("remove timeout")
            .expect("remove");
        assert!(remove.client_id.is_some());
        let _ = send.await;
        assert!(!fleet.contains(&host));
        assert!(fleet.contains(&other));
        assert!(fleet.removal_ledger(&host).expect("ledger").is_some());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn stale_admission_blocked_after_reconnect() {
        let task = TaskId::from_bytes(fixed_uuid_v7(0x81)).expect("task");
        let host = HostId::local_profile("fleet_stale_adm").expect("host");
        let fleet = HostFleet::new();
        fleet
            .install(host.clone(), local_client("fleet_stale_adm", 0x82, 0x83))
            .expect("install");
        let stale = fleet
            .admit_action(HostTaskKey::new(host.clone(), task))
            .expect("admit");
        let _ = fleet
            .reconnect_with_factory(
                &host,
                Box::new(|| Box::pin(async { Ok(local_client("fleet_stale_adm", 0x82, 0x84)) })),
            )
            .await
            .expect("reconnect");
        assert!(matches!(
            fleet.validate_admission(&stale),
            Err(FleetError::StaleGeneration)
        ));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn concurrent_reconnect_generations_stay_monotonic() {
        let host = HostId::local_profile("fleet_mono_gen").expect("host");
        let fleet = Arc::new(HostFleet::new());
        fleet
            .install(host.clone(), local_client("fleet_mono_gen", 0x85, 0x86))
            .expect("install");
        let a = {
            let fleet = Arc::clone(&fleet);
            let host = host.clone();
            tokio::spawn(async move {
                fleet
                    .reconnect_with_factory(
                        &host,
                        Box::new(|| {
                            Box::pin(async {
                                tokio::task::yield_now().await;
                                Ok(local_client("fleet_mono_gen", 0x85, 0x87))
                            })
                        }),
                    )
                    .await
            })
        };
        let b = {
            let fleet = Arc::clone(&fleet);
            let host = host.clone();
            tokio::spawn(async move {
                fleet
                    .reconnect_with_factory(
                        &host,
                        Box::new(|| {
                            Box::pin(async {
                                tokio::task::yield_now().await;
                                Ok(local_client("fleet_mono_gen", 0x85, 0x88))
                            })
                        }),
                    )
                    .await
            })
        };
        let ra = a.await.expect("a");
        let rb = b.await.expect("b");
        let gens = [ra.ok().map(|v| v.value), rb.ok().map(|v| v.value)]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>();
        assert!(!gens.is_empty());
        let current = fleet.generation(&host).expect("gen");
        assert!(gens.iter().all(|g| *g <= current));
        assert_eq!(current, *gens.iter().max().expect("max"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn full_queue_shutdown_drains_without_join_block() {
        let host = HostId::local_profile("fleet_full_q").expect("host");
        let fleet = Arc::new(HostFleet::new());
        let (client, wire) = local_client_with_wire("fleet_full_q", 0x97, 0x98, BTreeMap::new());
        fleet.install(host.clone(), client).expect("install");
        let task = TaskId::from_bytes(fixed_uuid_v7(0x99)).expect("task");
        let admission = fleet
            .admit_action(HostTaskKey::new(host.clone(), task))
            .expect("admit");
        let mut joins = Vec::new();
        for tail in 0x9a_u8..=0xba {
            let fleet_task = Arc::clone(&fleet);
            let admission_task = admission.clone();
            joins.push(tokio::spawn(async move {
                fleet_task
                    .execute_command(
                        &admission_task,
                        CommandEnvelope {
                            command_id: CommandId::from_bytes(fixed_uuid_v7(tail)).expect("cmd"),
                            client_id: admission_task.client_id,
                            task_id: Some(task),
                            issued_at_ms: 1,
                            expected_task_revision: None,
                            command: crate::domain::command::Command::SettleTask,
                        },
                    )
                    .await
            }));
        }
        let _ = wire.wait_command_admitted().await;
        let removed = tokio::time::timeout(Duration::from_secs(3), fleet.remove(&host))
            .await
            .expect("remove must not block on full queue join")
            .expect("remove");
        for join in joins {
            let _ = join.await;
        }
        assert!(removed.client_id.is_some());
        assert!(!fleet.contains(&host));
    }

    #[test]
    fn no_active_host_and_remote_metadata_rejected() {
        let fleet = HostFleet::new();
        assert!(fleet.host_ids().is_empty());
        let remote = HostId::remote(fixed_uuid_v7(0x91)).expect("remote");
        assert!(matches!(
            fleet.install(remote, local_client("fleet_bad_remote", 0x92, 0x93)),
            Err(FleetError::HostMetadataMismatch)
        ));
        let host = HostId::local_profile("fleet_caps").expect("host");
        fleet
            .install(host.clone(), local_client("fleet_caps", 0x94, 0x95))
            .expect("install");
        assert!(fleet
            .granted_capabilities(&host)
            .expect("caps")
            .contains(Capability::PagedSnapshots));
        let remote = HostId::remote(fixed_uuid_v7(0x96)).expect("remote2");
        // Remote host identity is unsupported for raw terminal even before install
        // metadata exists; classify requires a live entry, so expect not found here.
        assert!(matches!(
            fleet.classify_request_support(&remote, FleetUnsupportedKind::RawTerminalInput),
            Err(FleetError::HostNotFound)
        ));
        assert!(matches!(
            fleet.classify_request_support(&host, FleetUnsupportedKind::RawTerminalInput),
            Ok(())
        ));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn disconnect_is_read_only_with_cached_model() {
        let task = TaskId::from_bytes(fixed_uuid_v7(0xa1)).expect("task");
        let host = HostId::local_profile("fleet_disc_cache").expect("host");
        let fleet = HostFleet::new();
        fleet
            .install(host.clone(), local_client("fleet_disc_cache", 0xa2, 0xa3))
            .expect("install");
        fleet
            .seed_cached_model_for_test(&host, task_model(task, "Cached"))
            .expect("cache");
        let _ = fleet.disconnect(&host).await.expect("disconnect");
        assert!(!fleet.is_connected(&host).expect("state"));
        assert!(fleet
            .admit_read(HostTaskKey::new(host.clone(), task))
            .is_ok());
        assert!(matches!(
            fleet.admit_action(HostTaskKey::new(host, task)),
            Err(FleetError::DisconnectedReadOnly)
        ));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn failed_factory_cannot_erase_a_concurrent_disconnect_waiter() {
        let host = HostId::local_profile("fleet_disc_error").expect("host");
        let fleet = Arc::new(HostFleet::new());
        fleet
            .install(host.clone(), local_client("fleet_disc_error", 0xf1, 0xf2))
            .expect("install");
        let (pending_tx, pending_rx) = oneshot::channel();
        let disconnect_fleet = Arc::clone(&fleet);
        let disconnect_host = host.clone();
        let result = fleet
            .reconnect_with_factory(
                &host,
                Box::new(move || {
                    Box::pin(async move {
                        let mut disconnect =
                            Box::pin(
                                async move { disconnect_fleet.disconnect(&disconnect_host).await },
                            );
                        // Admit the real Disconnect within the factory poll,
                        // then return its error in that same poll. The factory
                        // error branch wins while the disconnect owns a waiter.
                        assert!(futures_util::poll!(disconnect.as_mut()).is_pending());
                        pending_tx.send(disconnect).ok().expect("keep waiter alive");
                        Err(IpcError::Unavailable)
                    })
                }),
            )
            .await;
        assert!(matches!(
            result,
            Err(FleetError::Ipc(IpcError::Unavailable))
        ));
        let disconnect = pending_rx.await.expect("pending disconnect");
        let result = tokio::time::timeout(Duration::from_secs(2), disconnect)
            .await
            .expect("disconnect waiter must complete")
            .expect("disconnect");
        assert_eq!(result.host, host);
        assert!(!fleet.is_connected(&host).expect("state"));
        fleet.remove(&host).await.expect("remove");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn disconnect_interrupts_pending_factory() {
        let host = HostId::local_profile("fleet_disc_factory").expect("host");
        let fleet = Arc::new(HostFleet::new());
        fleet
            .install(host.clone(), local_client("fleet_disc_factory", 0xb1, 0xb2))
            .expect("install");
        let (start_tx, start_rx) = oneshot::channel::<()>();
        let (release_tx, release_rx) = oneshot::channel::<()>();
        let fleet_rec = Arc::clone(&fleet);
        let host_rec = host.clone();
        let reconnect = tokio::spawn(async move {
            fleet_rec
                .reconnect_with_factory(
                    &host_rec,
                    Box::new(move || {
                        Box::pin(async move {
                            let _ = start_tx.send(());
                            let _ = release_rx.await;
                            Ok(local_client("fleet_disc_factory", 0xb1, 0xb3))
                        })
                    }),
                )
                .await
        });
        let _ = start_rx.await;
        let disconnected = tokio::time::timeout(Duration::from_secs(2), fleet.disconnect(&host))
            .await
            .expect("disconnect timeout")
            .expect("disconnect");
        let _ = release_tx.send(());
        let rec = reconnect.await.expect("join");
        assert!(matches!(rec, Err(FleetError::HostFenced)));
        assert!(!fleet.is_connected(&host).expect("state"));
        assert_eq!(disconnected.host, host);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn ledger_rejects_before_outbound_write_when_full() {
        let task = TaskId::from_bytes(fixed_uuid_v7(0xb4)).expect("task");
        let host = HostId::local_profile("fleet_ledger_full").expect("host");
        let fleet = Arc::new(HostFleet::new());
        let (client, wire) =
            local_client_with_wire("fleet_ledger_full", 0xb5, 0xb6, BTreeMap::new());
        fleet.install(host.clone(), client).expect("install");
        let admission = fleet
            .admit_action(HostTaskKey::new(host.clone(), task))
            .expect("admit");
        // Fill ledger with retained receipts via delayed wire settle + no ack.
        for i in 0..MAX_OUTCOME_BUFFER {
            let command_id =
                CommandId::from_bytes(fixed_uuid_v7(0xc0_u8.wrapping_add(i as u8))).expect("cmd");
            let operation_id =
                OperationId::from_bytes(fixed_uuid_v7(0xd0_u8.wrapping_add(i as u8))).expect("op");
            let fleet_task = Arc::clone(&fleet);
            let admission_task = admission.clone();
            let join = tokio::spawn(async move {
                fleet_task
                    .execute_command(
                        &admission_task,
                        CommandEnvelope {
                            command_id,
                            client_id: admission_task.client_id,
                            task_id: Some(task),
                            issued_at_ms: 1,
                            expected_task_revision: None,
                            command: crate::domain::command::Command::SettleTask,
                        },
                    )
                    .await
            });
            let admitted = wire.wait_command_admitted().await;
            assert_eq!(admitted, command_id);
            wire.dispatch_accepted(accepted_receipt(command_id, operation_id))
                .await
                .expect("dispatch");
            let _ = join.await;
        }
        let blocked = fleet
            .execute_command(
                &admission,
                CommandEnvelope {
                    command_id: CommandId::from_bytes(fixed_uuid_v7(0xef)).expect("cmd"),
                    client_id: admission.client_id,
                    task_id: Some(task),
                    issued_at_ms: 1,
                    expected_task_revision: None,
                    command: crate::domain::command::Command::SettleTask,
                },
            )
            .await;
        assert!(matches!(blocked, Err(FleetError::QueueFull)));
        // No outbound write for the rejected command.
        assert!(!wire.has_command_waiter(CommandId::from_bytes(fixed_uuid_v7(0xef)).expect("cmd")));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn canceled_remove_still_publishes_generation_ledger() {
        let host = HostId::local_profile("fleet_rm_cancel").expect("host");
        let fleet = Arc::new(HostFleet::new());
        let (client, wire) = local_client_with_wire("fleet_rm_cancel", 0xe1, 0xe2, BTreeMap::new());
        let gen = fleet.install(host.clone(), client).expect("install");
        let task = TaskId::from_bytes(fixed_uuid_v7(0xe3)).expect("task");
        let admission = fleet
            .admit_action(HostTaskKey::new(host.clone(), task))
            .expect("admit");
        let fleet_send = Arc::clone(&fleet);
        let admission_send = admission.clone();
        let send = tokio::spawn(async move {
            fleet_send
                .execute_command(
                    &admission_send,
                    CommandEnvelope {
                        command_id: CommandId::from_bytes(fixed_uuid_v7(0xe4)).expect("cmd"),
                        client_id: admission_send.client_id,
                        task_id: Some(task),
                        issued_at_ms: 1,
                        expected_task_revision: None,
                        command: crate::domain::command::Command::SettleTask,
                    },
                )
                .await
        });
        let _ = wire.wait_command_admitted().await;
        let fleet_rm = Arc::clone(&fleet);
        let host_rm = host.clone();
        let remove = tokio::spawn(async move { fleet_rm.remove(&host_rm).await });
        // Cancel after removal has actually been admitted. A single yield can
        // abort the spawned future before it ever runs on a different worker.
        tokio::time::timeout(Duration::from_secs(2), async {
            while fleet.contains(&host) {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .expect("remove reached its generation fence");
        remove.abort();
        let _ = remove.await;
        let _ = send.await;
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                fleet.reclaim_finished_draining().expect("reclaim");
                if fleet
                    .removal_ledger_at(&host, gen)
                    .expect("ledger")
                    .is_some()
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .expect("canceled removal published its generation ledger");
        assert!(fleet
            .removal_ledger_at(&host, gen)
            .expect("ledger")
            .is_some());
        // Reinstall under a new generation must not erase the prior ledger.
        let gen2 = fleet
            .install(host.clone(), local_client("fleet_rm_cancel", 0xe5, 0xe6))
            .expect("reinstall");
        assert_ne!(gen, gen2);
        assert!(fleet
            .removal_ledger_at(&host, gen)
            .expect("old ledger")
            .is_some());
        let _ = fleet.remove(&host).await.expect("remove2");
        assert!(fleet
            .removal_ledger_at(&host, gen2)
            .expect("new ledger")
            .is_some());
        assert!(fleet
            .removal_ledger_at(&host, gen)
            .expect("old still")
            .is_some());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn reconnect_metadata_failure_then_successful_retry() {
        let host = HostId::local_profile("fleet_meta_retry").expect("host");
        let fleet = HostFleet::new();
        fleet
            .install(host.clone(), local_client("fleet_meta_retry", 0xf1, 0xf2))
            .expect("install");
        let bad = fleet
            .reconnect_with_factory(
                &host,
                Box::new(|| {
                    Box::pin(async {
                        // Wrong profile fingerprint relative to HostId.
                        Ok(local_client("fleet_meta_other", 0xf1, 0xf3))
                    })
                }),
            )
            .await;
        assert!(matches!(bad, Err(FleetError::HostMetadataMismatch)));
        assert_eq!(fleet.generation(&host).expect("gen"), 2);
        assert!(!fleet.is_connected(&host).expect("disc"));
        let ok = fleet
            .reconnect_with_factory(
                &host,
                Box::new(|| Box::pin(async { Ok(local_client("fleet_meta_retry", 0xf1, 0xf4)) })),
            )
            .await
            .expect("retry");
        assert_eq!(ok.value, 3);
        assert!(fleet.is_connected(&host).expect("up"));
        let meta = fleet.owner_metadata(&host).expect("meta");
        assert_eq!(meta.generation, 3);
        assert_eq!(meta.task_id, None);
        assert_eq!(meta.client_id, ok.client_id);
    }

    #[test]
    fn admit_host_has_no_synthetic_task() {
        let host = HostId::local_profile("fleet_admit_host").unwrap();
        let fleet = HostFleet::new();
        fleet
            .install(host.clone(), local_client("fleet_admit_host", 0x11, 0x12))
            .unwrap();
        let admission = fleet.admit_host(&host).unwrap();
        assert!(admission.task_id.is_none());
        assert_eq!(admission.host, host);
    }

    #[tokio::test]
    async fn wrong_task_or_stale_generation_fails_before_wire() {
        let task = TaskId::from_bytes(fixed_uuid_v7(0x71)).expect("task");
        let other = TaskId::from_bytes(fixed_uuid_v7(0x72)).expect("other");
        let host = HostId::local_profile("fleet_scope_gate").expect("host");
        let fleet = HostFleet::new();
        fleet
            .install(host.clone(), local_client("fleet_scope_gate", 0x73, 0x74))
            .expect("install");
        let admission = fleet
            .admit_action(HostTaskKey::new(host.clone(), task))
            .expect("admit");
        let err = fleet
            .query(
                &admission,
                QueryEnvelope {
                    request_id: RequestId::new(),
                    client_id: admission.client_id,
                    task_id: Some(other),
                    query: Query::InspectHostQuit,
                },
            )
            .await
            .expect_err("wrong task");
        assert!(matches!(err, FleetError::AdmissionOwnerMismatch));

        let global = fleet.admit_host(&host).expect("global");
        let err = fleet
            .query(
                &global,
                QueryEnvelope {
                    request_id: RequestId::new(),
                    client_id: global.client_id,
                    task_id: Some(task),
                    query: Query::InspectHostQuit,
                },
            )
            .await
            .expect_err("task on host-global");
        assert!(matches!(err, FleetError::AdmissionOwnerMismatch));

        let mut stale = admission.clone();
        stale.generation = admission.generation.wrapping_add(99);
        let err = fleet
            .query(
                &stale,
                QueryEnvelope {
                    request_id: RequestId::new(),
                    client_id: stale.client_id,
                    task_id: Some(task),
                    query: Query::InspectHostQuit,
                },
            )
            .await
            .expect_err("stale");
        assert!(matches!(err, FleetError::StaleGeneration));
        fleet.remove(&host).await.unwrap();
    }

    #[test]
    fn remote_local_only_ops_rejected_before_io() {
        let host = HostId::remote([1; 16]).unwrap();
        // classify without a live entry: HostNotFound is acceptable; remote
        // HostId still rejects when classify_request_support is reachable.
        let fleet = HostFleet::new();
        assert!(matches!(
            fleet.classify_request_support(&host, FleetUnsupportedKind::ExplicitDetach),
            Err(FleetError::HostNotFound) | Err(FleetError::UnsupportedRequest(_))
        ));
        assert!(matches!(
            reject_remote_shutdown_or_update(
                &host,
                &crate::domain::command::Command::ConfirmHostQuit(
                    crate::domain::command::ConfirmHostQuitIntent {
                        inspection_id: 1,
                        allow_uninspected_worktrees: false,
                    }
                )
            ),
            Err(FleetError::UnsupportedRequest(_))
        ));
        assert!(matches!(
            reject_remote_shutdown_or_update(
                &host,
                &crate::domain::command::Command::PrepareUpdate(
                    crate::domain::command::PrepareUpdateIntent {
                        target_version: "1".into(),
                        client_build: "c".into(),
                        host_build: "h".into(),
                        allow_explicit_confirm_with_active: false,
                    }
                )
            ),
            Err(FleetError::UnsupportedRequest(_))
        ));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn disconnect_admitted_cannot_fence_in_flight_replacement_generation() {
        let host = HostId::local_profile("fleet_disc_admit").unwrap();
        let fleet = Arc::new(HostFleet::new());
        fleet
            .install(host.clone(), local_client("fleet_disc_admit", 0x81, 0x82))
            .unwrap();
        let admission = fleet.admit_host(&host).unwrap();
        let gen = admission.generation;
        let (entered_tx, entered_rx) = oneshot::channel::<()>();
        let (release_tx, release_rx) = oneshot::channel::<()>();
        let fleet_rec = Arc::clone(&fleet);
        let host_rec = host.clone();
        let reconnect = tokio::spawn(async move {
            fleet_rec
                .reconnect_with_factory(
                    &host_rec,
                    Box::new(move || {
                        Box::pin(async move {
                            let _ = entered_tx.send(());
                            let _ = release_rx.await;
                            Ok(local_client("fleet_disc_admit", 0x81, 0x83))
                        })
                    }),
                )
                .await
        });
        let _ = entered_rx.await;
        let err = fleet
            .disconnect_admitted(&admission)
            .await
            .expect_err("stale while reconnecting");
        assert!(matches!(err, FleetError::StaleGeneration));
        assert_eq!(fleet.generation(&host).unwrap(), gen + 1);
        let _ = release_tx.send(());
        let _ = reconnect.await.expect("join");
        fleet.remove(&host).await.unwrap();
    }

    #[tokio::test]
    async fn taskful_command_result_keeps_admission_task() {
        let task = TaskId::from_bytes(fixed_uuid_v7(0x91)).expect("task");
        let host = HostId::local_profile("fleet_task_scope").expect("host");
        let fleet = Arc::new(HostFleet::new());
        let (client, wire) =
            local_client_with_wire("fleet_task_scope", 0x92, 0x93, BTreeMap::new());
        fleet.install(host.clone(), client).expect("install");
        let admission = fleet
            .admit_action(HostTaskKey::new(host.clone(), task))
            .expect("admit");
        let command_id = CommandId::from_bytes(fixed_uuid_v7(0x94)).expect("cmd");
        let operation_id = OperationId::from_bytes(fixed_uuid_v7(0x95)).expect("op");
        let fleet_task = Arc::clone(&fleet);
        let admission_task = admission.clone();
        let join = tokio::spawn(async move {
            fleet_task
                .execute_command(
                    &admission_task,
                    CommandEnvelope {
                        command_id,
                        client_id: admission_task.client_id,
                        task_id: Some(task),
                        issued_at_ms: 1,
                        expected_task_revision: None,
                        command: crate::domain::command::Command::SettleTask,
                    },
                )
                .await
        });
        let admitted = wire.wait_command_admitted().await;
        assert_eq!(admitted, command_id);
        wire.dispatch_accepted(accepted_receipt(command_id, operation_id))
            .await
            .expect("dispatch");
        let owned = join.await.expect("join").expect("receipt");
        assert_eq!(owned.task_id, Some(task));
        assert_eq!(owned.host, host);
        fleet.remove(&host).await.unwrap();
    }

    #[tokio::test]
    async fn preview_tasks_requires_host_global_admission() {
        let task = TaskId::from_bytes(fixed_uuid_v7(0x96)).expect("task");
        let host = HostId::local_profile("fleet_preview_scope").expect("host");
        let fleet = HostFleet::new();
        fleet
            .install(
                host.clone(),
                local_client("fleet_preview_scope", 0x97, 0x98),
            )
            .expect("install");
        let taskful = fleet
            .admit_action(HostTaskKey::new(host.clone(), task))
            .expect("admit");
        let err = fleet
            .preview_tasks(&taskful)
            .await
            .expect_err("taskful preview");
        assert!(matches!(err, FleetError::AdmissionOwnerMismatch));
        fleet.remove(&host).await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn remove_return_survives_concurrent_acknowledge() {
        let host = HostId::local_profile("fleet_rm_ack_race").unwrap();
        let fleet = Arc::new(HostFleet::new());
        let gen = fleet
            .install(host.clone(), local_client("fleet_rm_ack_race", 0xd1, 0xd2))
            .unwrap();
        let remove_fleet = Arc::clone(&fleet);
        let remove_host = host.clone();
        let remove = tokio::spawn(async move { remove_fleet.remove(&remove_host).await });
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let _ = fleet.reclaim_finished_draining();
                if fleet.removal_ledger_at(&host, gen).ok().flatten().is_some() {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("ledger published");
        fleet
            .acknowledge_removal_ledger(&host, gen)
            .expect("ack erased shared ledger");
        let removal = remove
            .await
            .expect("join")
            .expect("remover retained once-published copy");
        assert_eq!(removal.generation, gen);
        assert_eq!(removal.host, host);
        assert!(fleet.removal_ledger_at(&host, gen).unwrap().is_none());
    }

    #[tokio::test]
    async fn reconnect_local_rejects_remote_and_preserves_disconnect_fence() {
        let remote = HostId::remote([3; 16]).unwrap();
        let fleet = HostFleet::new();
        assert!(matches!(
            fleet.reconnect_local(&remote).await,
            Err(FleetError::UnsupportedRequest(_))
        ));

        let host = HostId::local_profile("fleet_reconnect_local").unwrap();
        fleet
            .install(
                host.clone(),
                local_client("fleet_reconnect_local", 0xe1, 0xe2),
            )
            .unwrap();
        let disconnected = fleet.disconnect(&host).await.expect("disconnect");
        assert_eq!(disconnected.task_id, None);
        // Disconnect settles to retryable Live+disconnected; admit_host fails.
        // A concurrent DisconnectRequested during reconnect is covered by factory
        // tests; here remote reject and post-disconnect reconnect failure matter.
        let err = fleet.reconnect_local(&host).await.expect_err("disc");
        assert!(matches!(
            err,
            FleetError::DisconnectedReadOnly
                | FleetError::HostFenced
                | FleetError::Ipc(_)
                | FleetError::WorkerGone
                | FleetError::HostBusy
        ));
        // Still installed for later reconnect_with_factory recovery.
        assert!(fleet.contains(&host));
        let _ = fleet.remove(&host).await;
    }

    #[test]
    fn subscription_id_and_replay_are_host_global() {
        let host = HostId::local_profile("fleet_sub_replay").unwrap();
        let fleet = HostFleet::new();
        fleet
            .install(host.clone(), local_client("fleet_sub_replay", 0xf5, 0xf6))
            .unwrap();
        let owned = fleet.subscription_id(&host).unwrap();
        assert_eq!(owned.task_id, None);
        assert!(owned.value.is_none());
        let admission = fleet.admit_host(&host).unwrap();
        let replay = fleet.take_replay_events(&admission).unwrap();
        assert!(replay.value.is_empty());
        let task = TaskId::from_bytes(fixed_uuid_v7(0xf7)).expect("task");
        let taskful = fleet
            .admit_action(HostTaskKey::new(host.clone(), task))
            .unwrap();
        assert!(matches!(
            fleet.take_replay_events(&taskful),
            Err(FleetError::AdmissionOwnerMismatch)
        ));
    }
}
