//! Single host-owned CommandBus executor boundary.
//!
//! Transport connection tasks never mutate the bus or projections directly.
//! They submit decoded requests through [`HostRequestHandle`]; one
//! [`HostRequestExecutor`] task exclusively owns [`CommandBus`] and services
//! them in arrival order. The executor also owns the bounded SnapshotSession
//! and EventReplaySession registries for paged snapshot and event-replay queries.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio::time::{interval, MissedTickBehavior};

use crate::domain::id::{SnapshotId, SubscriptionId};
use crate::domain::query::{
    Query, QueryEnvelope, QueryError, QueryOutcome, QueryReply, QueryResult,
};
use crate::domain::snapshot::{PageLimits, SnapshotSection};
use crate::domain::ClientId;
use crate::kernel::{
    CommandBus, EventReplaySession, ReplayError, SnapshotError, SnapshotSession, StoreError,
};
use crate::protocol::{Capability, ClientRequest, NegotiatedParameters, ServerResponse};

use super::ipc::IpcError;

/// Fixed capacity for the host request queue.
///
/// When the queue is full, [`HostRequestHandle::execute`] awaits send capacity
/// (bounded backpressure). Requests are never silently dropped.
pub const HOST_REQUEST_QUEUE_CAPACITY: usize = 32;

const MAX_SNAPSHOT_SESSIONS: usize = 32;
const SNAPSHOT_IDLE_TTL: Duration = Duration::from_secs(30);
const SNAPSHOT_REAPER_PERIOD: Duration = Duration::from_secs(1);

const MAX_EVENT_REPLAY_SESSIONS: usize = 32;
const EVENT_REPLAY_IDLE_TTL: Duration = Duration::from_secs(30);
const EVENT_REPLAY_REAPER_PERIOD: Duration = Duration::from_secs(1);

struct HostRequestJob {
    negotiated: NegotiatedParameters,
    request: ClientRequest,
    reply: oneshot::Sender<Result<ServerResponse, IpcError>>,
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

struct EventReplayRegistryEntry {
    owner: ClientId,
    session: EventReplaySession,
    limits: PageLimits,
    last_touch: Instant,
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
        self.entries
            .retain(|_, entry| now.duration_since(entry.last_touch) < EVENT_REPLAY_IDLE_TTL);
    }

    fn evict_lru_if_at_capacity(&mut self) {
        while self.entries.len() >= MAX_EVENT_REPLAY_SESSIONS {
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
        session: EventReplaySession,
        limits: PageLimits,
        now: Instant,
    ) {
        self.evict_lru_if_at_capacity();
        let subscription_id = session.subscription_id();
        self.entries.insert(
            subscription_id,
            EventReplayRegistryEntry {
                owner,
                session,
                limits,
                last_touch: now,
            },
        );
    }

    fn touch(&mut self, subscription_id: SubscriptionId, now: Instant) {
        if let Some(entry) = self.entries.get_mut(&subscription_id) {
            entry.last_touch = now;
        }
    }

    fn remove(&mut self, subscription_id: SubscriptionId) -> Option<EventReplayRegistryEntry> {
        self.entries.remove(&subscription_id)
    }

    fn get(
        &self,
        subscription_id: SubscriptionId,
        requester: ClientId,
        limits: PageLimits,
        now: Instant,
    ) -> Result<&EventReplaySession, QueryError> {
        let Some(entry) = self.entries.get(&subscription_id) else {
            return Err(QueryError::NotFound);
        };
        if now.duration_since(entry.last_touch) >= EVENT_REPLAY_IDLE_TTL {
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

/// Cloneable submit handle for the single host CommandBus executor.
#[derive(Clone, Debug)]
pub struct HostRequestHandle {
    tx: mpsc::Sender<HostRequestJob>,
}

impl HostRequestHandle {
    /// Enqueue one authenticated request and await its correlated reply.
    ///
    /// Blocks (with bounded queue backpressure) when the executor queue is full.
    /// Returns [`IpcError::Unavailable`] if the executor has stopped.
    pub async fn execute(
        &self,
        negotiated: NegotiatedParameters,
        request: ClientRequest,
    ) -> Result<ServerResponse, IpcError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx
            .send(HostRequestJob {
                negotiated,
                request,
                reply: reply_tx,
            })
            .await
            .map_err(|_| IpcError::Unavailable)?;
        reply_rx.await.map_err(|_| IpcError::Unavailable)?
    }
}

/// Exclusive owner of [`CommandBus`]. Runs on one task and drains a bounded queue.
pub struct HostRequestExecutor {
    bus: CommandBus,
    rx: mpsc::Receiver<HostRequestJob>,
    registry: SnapshotRegistry,
    replay_registry: EventReplayRegistry,
}

impl HostRequestExecutor {
    /// Spawn the single CommandBus executor task.
    ///
    /// The returned handle may be cloned for every connection task. Dropping
    /// every handle closes the queue; the executor then finishes after draining
    /// any already-queued jobs.
    pub fn start(bus: CommandBus) -> (HostRequestHandle, JoinHandle<()>) {
        let (tx, rx) = mpsc::channel(HOST_REQUEST_QUEUE_CAPACITY);
        let handle = HostRequestHandle { tx };
        let mut executor = Self {
            bus,
            rx,
            registry: SnapshotRegistry::new(),
            replay_registry: EventReplayRegistry::new(),
        };
        let join = tokio::spawn(async move {
            executor.run().await;
        });
        (handle, join)
    }

    async fn run(&mut self) {
        let mut reaper = interval(SNAPSHOT_REAPER_PERIOD.min(EVENT_REPLAY_REAPER_PERIOD));
        reaper.set_missed_tick_behavior(MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                job = self.rx.recv() => {
                    let Some(job) = job else {
                        break;
                    };
                    let result = self.dispatch(job.negotiated, job.request);
                    // If the connection task went away, drop the reply; do not panic.
                    let _ = job.reply.send(result);
                }
                _ = reaper.tick() => {
                    let now = Instant::now();
                    self.registry.reap_idle(now);
                    self.replay_registry.reap_idle(now);
                }
            }
        }
    }

    fn dispatch(
        &mut self,
        negotiated: NegotiatedParameters,
        request: ClientRequest,
    ) -> Result<ServerResponse, IpcError> {
        match request {
            ClientRequest::Command(envelope) => {
                if envelope.client_id != negotiated.client_id {
                    return Err(IpcError::Unauthorized);
                }
                let receipt = self.bus.execute(envelope).map_err(map_store_error)?;
                Ok(ServerResponse::CommandReceipt(receipt))
            }
            ClientRequest::Query(envelope) => {
                if envelope.client_id != negotiated.client_id {
                    return Err(IpcError::Unauthorized);
                }
                let reply = self.dispatch_query(negotiated, envelope)?;
                Ok(ServerResponse::QueryReply(reply))
            }
        }
    }

    fn dispatch_query(
        &mut self,
        negotiated: NegotiatedParameters,
        envelope: QueryEnvelope,
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
                let outcome = self.serve_open_event_replay(negotiated, after_sequence)?;
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
                let outcome =
                    self.serve_continue_event_replay(negotiated, subscription_id, resume_cursor)?;
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
            (Some(snapshot_id), Some(resume_cursor)) => {
                self.resume_snapshot_page(negotiated, section, snapshot_id, resume_cursor)
            }
            _ => Ok(QueryOutcome::Err(QueryError::InvalidRequest)),
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
        if page.next_cursor.is_some() {
            self.registry
                .insert(negotiated.client_id, session, limits, Instant::now());
        }
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
        let finished = page.next_cursor.is_none();
        if finished {
            self.registry.remove(snapshot_id);
        } else {
            self.registry.touch(snapshot_id, Instant::now());
        }
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
        if page.next_cursor.is_some() {
            self.replay_registry
                .insert(negotiated.client_id, session, limits, Instant::now());
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
    ) -> Result<QueryOutcome, IpcError> {
        let now = Instant::now();
        self.replay_registry.reap_idle(now);
        if let Some(entry) = self.replay_registry.entries.get(&subscription_id) {
            if now.duration_since(entry.last_touch) >= EVENT_REPLAY_IDLE_TTL {
                self.replay_registry.remove(subscription_id);
                return Ok(QueryOutcome::Err(QueryError::NotFound));
            }
        }
        let limits = page_limits_from_negotiated(negotiated)?;
        let session =
            match self
                .replay_registry
                .get(subscription_id, negotiated.client_id, limits, now)
            {
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
        let finished = page.next_cursor.is_none();
        if finished {
            self.replay_registry.remove(subscription_id);
        } else {
            self.replay_registry.touch(subscription_id, Instant::now());
        }
        Ok(QueryOutcome::Ok(QueryResult::EventReplayPage {
            subscription_id,
            page,
        }))
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
                self.replay_registry.remove(subscription_id);
                QueryOutcome::Ok(QueryResult::EventReplayReleased { subscription_id })
            }
        }
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

/// Authenticated client_id check plus CommandBus execute/query dispatch.
///
/// Used by the exclusive [`super::ipc::HostConnection::serve_request`]
/// compatibility path. Registry-backed snapshot and event-replay queries are
/// unsupported here; the single executor owns those registries.
pub(crate) fn dispatch_authenticated_request(
    authenticated_client_id: ClientId,
    bus: &mut CommandBus,
    request: ClientRequest,
) -> Result<ServerResponse, IpcError> {
    match request {
        ClientRequest::Command(envelope) => {
            if envelope.client_id != authenticated_client_id {
                return Err(IpcError::Unauthorized);
            }
            let receipt = bus.execute(envelope).map_err(map_store_error)?;
            Ok(ServerResponse::CommandReceipt(receipt))
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
                | Query::ReleaseEventReplay { .. } => {
                    return Ok(ServerResponse::QueryReply(QueryReply {
                        request_id: envelope.request_id,
                        outcome: QueryOutcome::Err(QueryError::UnsupportedCapability),
                    }));
                }
                Query::OperationStatus { .. } | Query::TaskSnapshot => {}
            }
            let reply = bus.query(envelope).map_err(map_store_error)?;
            Ok(ServerResponse::QueryReply(reply))
        }
    }
}
