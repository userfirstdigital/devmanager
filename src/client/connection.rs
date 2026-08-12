//! Cloneable duplex multiplexed client connection after Hello.
//!
//! One shared I/O supervisor owns the named-pipe writer and continuously
//! running reader futures. Concurrent command/query calls register correlation
//! waiters before enqueueing writes. Unsolicited durable messages land in a
//! bounded inbox for later subscription consumers.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::{mpsc, oneshot};

use crate::domain::command::{CommandEnvelope, CommandReceipt};
use crate::domain::event::DomainEvent;
use crate::domain::id::{CommandId, RequestId, SubscriptionId};
#[cfg(test)]
use crate::domain::query::{Query, QueryError, QueryOutcome, QueryResult};
use crate::domain::query::{QueryEnvelope, QueryReply};
use crate::domain::{ClientId, MAX_SNAPSHOT_PAGE_ENCODED_BYTES, MAX_SNAPSHOT_PAGE_ITEMS};
use crate::host::{
    codecs_for_limits, handshake_codecs, handshake_timeout, read_physical_frame,
    read_physical_frame_idle_then_deadline, request_completion_timeout, supervise_duplex_halves,
    write_physical_frame, write_physical_frame_with_deadline, IpcError,
};
use crate::protocol::{
    ClientHello, ClientRequest, DetachAck, DetachRequest, FrameLimits, MessagePackCodec,
    PhysicalFrameCodec, ServerHello, ServerMessage, StreamFrame, MAX_PHYSICAL_FRAME_BYTES,
    MAX_REASSEMBLED_MESSAGE_BYTES,
};

const WRITE_QUEUE_CAPACITY: usize = 32;
const UNSOLICITED_QUEUE_CAPACITY: usize = 64;
const UNSOLICITED_STREAM_CAPACITY: usize = 16;
const MAX_RETIRED_SUBSCRIPTION_IDS: usize = 64;
const MAX_RETIRED_DRAIN_WORK: usize = UNSOLICITED_QUEUE_CAPACITY;
const MAX_RETAINED_DURABLE_MESSAGES: usize = UNSOLICITED_QUEUE_CAPACITY * 2;

/// Scripted detach I/O behavior for unit tests.
#[cfg(test)]
#[derive(Debug, Clone, Copy)]
pub(crate) enum ScriptedDetachBehavior {
    /// Echo a Detached ack with the request's connection_id.
    MatchingAck,
    /// Echo a Detached ack with a different connection_id (correlation fail-closed).
    WrongConnectionAck,
    /// Reject enqueue immediately (closed write queue / transport unavailable).
    ClosedWriteQueue,
    /// Return a correlated application error for ReleaseEventReplay.
    ReleaseQueryError,
    /// Return a release acknowledgement for a different subscription id.
    ReleaseWrongSubscriptionAck,
}

/// Unsolicited server→client messages (not correlated replies).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UnsolicitedServerMessage {
    DurableEvent {
        subscription_id: SubscriptionId,
        event: DomainEvent,
    },
    ResyncRequired {
        subscription_id: SubscriptionId,
        last_delivered_sequence: u64,
        newest_sequence: u64,
    },
    Stream(StreamFrame),
}

#[derive(Clone, Copy)]
enum UnsolicitedLane {
    Durable,
    Retained,
    Stream,
}

fn durable_message_subscription_id(message: &UnsolicitedServerMessage) -> Option<SubscriptionId> {
    match message {
        UnsolicitedServerMessage::DurableEvent {
            subscription_id, ..
        }
        | UnsolicitedServerMessage::ResyncRequired {
            subscription_id, ..
        } => Some(*subscription_id),
        UnsolicitedServerMessage::Stream(_) => None,
    }
}

fn durable_message_is_retired(
    retired_subscription_ids: &HashSet<SubscriptionId>,
    message: &UnsolicitedServerMessage,
) -> bool {
    durable_message_subscription_id(message)
        .is_some_and(|subscription_id| retired_subscription_ids.contains(&subscription_id))
}

impl UnsolicitedLane {
    fn next(self) -> Self {
        match self {
            Self::Durable => Self::Retained,
            Self::Retained => Self::Stream,
            Self::Stream => Self::Durable,
        }
    }
}

/// Bounded fair unsolicited inbox with separate retained, durable, and
/// coalescing current-stream lanes.
struct DurableInboxState {
    durable_tx: mpsc::Sender<UnsolicitedServerMessage>,
    durable_rx: mpsc::Receiver<UnsolicitedServerMessage>,
    /// Non-retired durable messages removed while fencing an old generation.
    /// They are drained before newly queued messages to preserve wire order.
    retained_durable: VecDeque<UnsolicitedServerMessage>,
    /// Subscription ids fenced at release. This set is deliberately bounded;
    /// exhausting it forces a typed reconnect instead of allowing an old tail
    /// to poison a replacement generation.
    retired_subscription_ids: HashSet<SubscriptionId>,
}

struct UnsolicitedInbox {
    /// Admission, retirement, and dequeue share one short synchronous fence.
    /// This makes the retired check plus nonblocking send atomic with the
    /// retirement mark plus queue drain: an old frame is either linearized
    /// before retirement and drained, or observes the retired id.
    durable: Mutex<DurableInboxState>,
    next_lane: Mutex<UnsolicitedLane>,
    stream_capacity: usize,
    streams: Mutex<std::collections::HashMap<crate::protocol::StreamKey, StreamFrame>>,
    notify: tokio::sync::Notify,
    closed: std::sync::atomic::AtomicBool,
    #[cfg(test)]
    close_gap_hook: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    #[cfg(test)]
    admission_barrier: Mutex<
        Option<(
            std::sync::Arc<std::sync::Barrier>,
            std::sync::Arc<std::sync::Barrier>,
        )>,
    >,
}

impl UnsolicitedInbox {
    fn new(durable_capacity: usize, stream_capacity: usize) -> Self {
        let (durable_tx, durable_rx) = mpsc::channel(durable_capacity.max(1));
        Self {
            durable: Mutex::new(DurableInboxState {
                durable_tx,
                durable_rx,
                retained_durable: VecDeque::new(),
                retired_subscription_ids: HashSet::new(),
            }),
            // Preserve the durable-first admission behavior, then rotate
            // retained and current-stream work before another durable turn.
            next_lane: Mutex::new(UnsolicitedLane::Durable),
            stream_capacity: stream_capacity.max(1),
            streams: Mutex::new(std::collections::HashMap::new()),
            notify: tokio::sync::Notify::new(),
            closed: std::sync::atomic::AtomicBool::new(false),
            #[cfg(test)]
            close_gap_hook: Mutex::new(None),
            #[cfg(test)]
            admission_barrier: Mutex::new(None),
        }
    }

    #[cfg(test)]
    fn new_for_test(durable_capacity: usize, stream_capacity: usize) -> Self {
        Self::new(durable_capacity, stream_capacity)
    }

    #[cfg(test)]
    fn install_close_gap_hook(&self, hook: Box<dyn FnOnce() + Send>) {
        *self.close_gap_hook.lock().expect("close gap hook") = Some(hook);
    }

    #[cfg(test)]
    fn install_admission_barrier(
        &self,
        entered: std::sync::Arc<std::sync::Barrier>,
        release: std::sync::Arc<std::sync::Barrier>,
    ) {
        *self.admission_barrier.lock().expect("admission barrier") = Some((entered, release));
    }

    fn close(&self) {
        self.closed.store(true, std::sync::atomic::Ordering::SeqCst);
        // Wake existing waiters and leave a permit for a waiter that races in
        // after the closed store but before registration.
        self.notify.notify_waiters();
        self.notify.notify_one();
    }

    fn push_durable(&self, message: UnsolicitedServerMessage) -> Result<(), IpcError> {
        if self.closed.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(IpcError::Unavailable);
        }
        match message {
            UnsolicitedServerMessage::DurableEvent { .. }
            | UnsolicitedServerMessage::ResyncRequired { .. } => {}
            UnsolicitedServerMessage::Stream(_) => return Err(IpcError::Unavailable),
        }
        let subscription_id = match &message {
            UnsolicitedServerMessage::DurableEvent {
                subscription_id, ..
            }
            | UnsolicitedServerMessage::ResyncRequired {
                subscription_id, ..
            } => *subscription_id,
            UnsolicitedServerMessage::Stream(_) => return Err(IpcError::Unavailable),
        };
        let durable = self.durable.lock().expect("durable inbox");
        #[cfg(test)]
        if let Some((entered, release)) = self
            .admission_barrier
            .lock()
            .expect("admission barrier")
            .take()
        {
            // The barrier is deliberately inside the durable fence. Tests can
            // then start retirement and prove it cannot interleave between the
            // retired check and the nonblocking send.
            entered.wait();
            release.wait();
        }
        if durable_message_is_retired(&durable.retired_subscription_ids, &message) {
            // A late exact old-generation tail is fenced at admission. Do not
            // notify a consumer when there is no message to receive.
            return Ok(());
        }
        durable
            .durable_tx
            .try_send(message)
            .map_err(|_| IpcError::Unavailable)?;
        drop(durable);
        self.notify.notify_one();
        Ok(())
    }

    fn retire_subscription_id(&self, subscription_id: SubscriptionId) -> Result<(), IpcError> {
        let mut durable = self.durable.lock().expect("durable inbox");
        if !durable.retired_subscription_ids.contains(&subscription_id) {
            if durable.retired_subscription_ids.len() >= MAX_RETIRED_SUBSCRIPTION_IDS {
                return Err(IpcError::RetiredSubscriptionFlood {
                    limit: MAX_RETIRED_SUBSCRIPTION_IDS,
                });
            }
            durable.retired_subscription_ids.insert(subscription_id);
        }

        // A frame for another live generation may have been retained while an
        // earlier generation was fenced. Re-check that bounded retained lane
        // whenever its own generation retires; otherwise a later replacement
        // could receive a frame that was already obsolete at the time of its
        // release.
        durable
            .retained_durable
            .retain(|message| durable_message_subscription_id(message) != Some(subscription_id));

        if durable
            .retained_durable
            .len()
            .saturating_add(durable.durable_rx.len())
            > MAX_RETAINED_DURABLE_MESSAGES
        {
            return Err(IpcError::RetiredSubscriptionFlood {
                limit: MAX_RETAINED_DURABLE_MESSAGES,
            });
        }

        let mut work = 0usize;
        while work < MAX_RETIRED_DRAIN_WORK {
            let message = match durable.durable_rx.try_recv() {
                Ok(message) => message,
                Err(mpsc::error::TryRecvError::Empty | mpsc::error::TryRecvError::Disconnected) => {
                    break;
                }
            };
            work = work.saturating_add(1);
            if durable_message_subscription_id(&message) != Some(subscription_id) {
                durable.retained_durable.push_back(message);
            }
        }

        if !durable.durable_rx.is_empty() {
            return Err(IpcError::RetiredSubscriptionFlood {
                limit: MAX_RETIRED_DRAIN_WORK,
            });
        }
        Ok(())
    }

    fn push_stream(&self, frame: StreamFrame) -> Result<(), IpcError> {
        if self.closed.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(IpcError::Unavailable);
        }
        let key = frame.stream;
        let mut streams = self.streams.lock().expect("stream inbox");
        if streams.contains_key(&key) || streams.len() < self.stream_capacity {
            streams.insert(key, frame);
            drop(streams);
            self.notify.notify_one();
        }
        // Saturated for a new key: drop without failing I/O.
        Ok(())
    }

    fn try_dequeue_from_state(
        durable: &mut DurableInboxState,
        streams: &Mutex<std::collections::HashMap<crate::protocol::StreamKey, StreamFrame>>,
        next_lane: &mut UnsolicitedLane,
    ) -> Option<UnsolicitedServerMessage> {
        let first_lane = *next_lane;
        for offset in 0..3 {
            let lane = match offset {
                0 => first_lane,
                1 => first_lane.next(),
                _ => first_lane.next().next(),
            };
            loop {
                let message = match lane {
                    UnsolicitedLane::Retained => durable.retained_durable.pop_front(),
                    // Retirement moves still-valid older durable frames into the
                    // retained lane. Do not let a newer wire frame leapfrog that
                    // retained truth, even when round-robin currently starts on
                    // the durable lane.
                    UnsolicitedLane::Durable if durable.retained_durable.is_empty() => {
                        durable.durable_rx.try_recv().ok()
                    }
                    UnsolicitedLane::Durable => None,
                    UnsolicitedLane::Stream => {
                        let mut streams = streams.lock().expect("stream inbox");
                        let key = streams.keys().next().copied();
                        key.and_then(|key| streams.remove(&key))
                            .map(UnsolicitedServerMessage::Stream)
                    }
                };
                let Some(message) = message else {
                    break;
                };
                if durable_message_is_retired(&durable.retired_subscription_ids, &message) {
                    // A drain flood can leave already-queued retired ids in
                    // the real host lanes. Drop them here so a replacement
                    // generation cannot observe that tail.
                    continue;
                }
                *next_lane = lane.next();
                return Some(message);
            }
        }
        None
    }

    async fn recv(&self) -> Result<UnsolicitedServerMessage, IpcError> {
        loop {
            if let Some(message) = self.try_dequeue() {
                return Ok(message);
            }

            // Register before rechecking closed/queues so close cannot be lost.
            let notified = self.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();

            if let Some(message) = self.try_dequeue() {
                return Ok(message);
            }

            #[cfg(test)]
            if let Some(hook) = self.close_gap_hook.lock().expect("close gap hook").take() {
                hook();
            }

            if self.closed.load(std::sync::atomic::Ordering::SeqCst) {
                if let Some(message) = self.try_dequeue() {
                    return Ok(message);
                }
                return Err(IpcError::Unavailable);
            }

            notified.await;
        }
    }

    fn try_dequeue(&self) -> Option<UnsolicitedServerMessage> {
        let mut next_lane = self.next_lane.lock().expect("unsolicited lane");
        let mut durable = self.durable.lock().expect("durable inbox");
        Self::try_dequeue_from_state(&mut durable, &self.streams, &mut next_lane)
    }
}

enum PendingKind {
    Command(oneshot::Sender<Result<CommandReceipt, IpcError>>),
    Query(oneshot::Sender<Result<QueryReply, IpcError>>),
    Detach(oneshot::Sender<Result<DetachAck, IpcError>>),
}

#[derive(Clone, Copy)]
enum PendingKey {
    Command(CommandId),
    Query(RequestId),
    Detach(RequestId),
}

struct PendingReply<T> {
    registration_id: u64,
    sender: oneshot::Sender<Result<T, IpcError>>,
    deadline_task: Option<tokio::task::JoinHandle<()>>,
}

impl<T> PendingReply<T> {
    fn complete(mut self, result: Result<T, IpcError>) {
        if let Some(task) = self.deadline_task.take() {
            task.abort();
        }
        let _ = self.sender.send(result);
    }

    fn complete_from_deadline(mut self) {
        // This is the task whose handle is stored here, so dropping the handle
        // lets the currently running deadline finish normally.
        self.deadline_task.take();
        let _ = self.sender.send(Err(IpcError::Timeout));
    }

    fn cancel(mut self) {
        if let Some(task) = self.deadline_task.take() {
            task.abort();
        }
    }
}

enum ErasedPendingReply {
    Command(PendingReply<CommandReceipt>),
    Query(PendingReply<QueryReply>),
    Detach(PendingReply<DetachAck>),
}

impl ErasedPendingReply {
    fn complete_from_deadline(self) {
        match self {
            Self::Command(pending) => pending.complete_from_deadline(),
            Self::Query(pending) => pending.complete_from_deadline(),
            Self::Detach(pending) => pending.complete_from_deadline(),
        }
    }

    fn cancel(self) {
        match self {
            Self::Command(pending) => pending.cancel(),
            Self::Query(pending) => pending.cancel(),
            Self::Detach(pending) => pending.cancel(),
        }
    }
}

struct PendingRegistration {
    state: Arc<Mutex<SharedState>>,
    key: Option<(PendingKey, u64)>,
}

impl PendingRegistration {
    fn new(state: Arc<Mutex<SharedState>>, key: PendingKey, registration_id: u64) -> Self {
        Self {
            state,
            key: Some((key, registration_id)),
        }
    }

    fn arm_response_deadline(
        &mut self,
        io_abort: tokio::task::AbortHandle,
        completion: Duration,
    ) -> Result<(), IpcError> {
        let Some((key, registration_id)) = self.key else {
            return Err(IpcError::Unavailable);
        };
        let weak_state = Arc::downgrade(&self.state);
        let mut deadline_task = Some(tokio::spawn(async move {
            tokio::time::sleep(completion).await;
            let Some(state) = weak_state.upgrade() else {
                return;
            };
            let Some(pending) = remove_pending_exact(&state, key, registration_id) else {
                return;
            };
            pending.complete_from_deadline();
            poison_mutex(&state);
            io_abort.abort();
        }));

        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match key {
            PendingKey::Command(id) => {
                if let Some(pending) = state
                    .command_waiters
                    .get_mut(&id)
                    .filter(|pending| pending.registration_id == registration_id)
                {
                    pending.deadline_task = deadline_task.take();
                }
            }
            PendingKey::Query(id) => {
                if let Some(pending) = state
                    .query_waiters
                    .get_mut(&id)
                    .filter(|pending| pending.registration_id == registration_id)
                {
                    pending.deadline_task = deadline_task.take();
                }
            }
            PendingKey::Detach(id) => {
                if let Some(pending) = state
                    .detach_waiters
                    .get_mut(&id)
                    .filter(|pending| pending.registration_id == registration_id)
                {
                    pending.deadline_task = deadline_task.take();
                }
            }
        }
        drop(state);

        // A response or connection failure may have removed this exact
        // generation before its deadline task was installed.
        if let Some(task) = deadline_task {
            task.abort();
        }
        self.key = None;
        Ok(())
    }
}

impl Drop for PendingRegistration {
    fn drop(&mut self) {
        let Some((key, registration_id)) = self.key.take() else {
            return;
        };
        if let Some(pending) = remove_pending_exact(&self.state, key, registration_id) {
            pending.cancel();
        }
    }
}

struct WriteJob {
    request: ClientRequest,
}

#[derive(Default)]
struct SharedState {
    next_registration_id: u64,
    command_waiters: HashMap<CommandId, PendingReply<CommandReceipt>>,
    query_waiters: HashMap<RequestId, PendingReply<QueryReply>>,
    detach_waiters: HashMap<RequestId, PendingReply<DetachAck>>,
    poisoned: bool,
    closed: bool,
}

struct SharedConnection {
    client_id: ClientId,
    server_hello: ServerHello,
    state: Arc<Mutex<SharedState>>,
    write_tx: mpsc::Sender<WriteJob>,
    unsolicited: Arc<UnsolicitedInbox>,
    io_task: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

/// Client-side authenticated duplex pipe after Hello.
#[derive(Clone)]
pub struct ClientConnection {
    shared: Arc<SharedConnection>,
}

impl ClientConnection {
    pub fn client_id(&self) -> ClientId {
        self.shared.client_id
    }

    pub fn server_hello(&self) -> ServerHello {
        self.shared.server_hello.clone()
    }

    /// Inert connected stub for HostClient unit tests (never performs I/O).
    #[cfg(test)]
    pub(crate) fn inert_stub_for_test(client_id: ClientId, server_hello: ServerHello) -> Self {
        Self {
            shared: Arc::new(SharedConnection {
                client_id,
                server_hello,
                state: Arc::new(Mutex::new(SharedState::default())),
                write_tx: {
                    let (tx, _rx) = mpsc::channel(1);
                    tx
                },
                unsolicited: Arc::new(UnsolicitedInbox::new_for_test(1, 1)),
                io_task: Mutex::new(None),
            }),
        }
    }

    /// Deterministic scripted duplex for detach unit tests (no named pipe).
    #[cfg(test)]
    pub(crate) fn scripted_for_test(
        client_id: ClientId,
        server_hello: ServerHello,
        behavior: ScriptedDetachBehavior,
    ) -> Self {
        let (write_tx, write_rx) = mpsc::channel::<WriteJob>(WRITE_QUEUE_CAPACITY);
        let state = Arc::new(Mutex::new(SharedState::default()));
        let unsolicited = Arc::new(UnsolicitedInbox::new_for_test(1, 1));
        let io_task = match behavior {
            ScriptedDetachBehavior::ClosedWriteQueue => {
                drop(write_rx);
                None
            }
            ScriptedDetachBehavior::MatchingAck
            | ScriptedDetachBehavior::WrongConnectionAck
            | ScriptedDetachBehavior::ReleaseQueryError
            | ScriptedDetachBehavior::ReleaseWrongSubscriptionAck => {
                let reader_state = Arc::clone(&state);
                let terminal_state = Arc::clone(&state);
                let reader_unsolicited = Arc::clone(&unsolicited);
                let wrong_connection =
                    matches!(behavior, ScriptedDetachBehavior::WrongConnectionAck);
                let release_query_error =
                    matches!(behavior, ScriptedDetachBehavior::ReleaseQueryError);
                let release_wrong_subscription = matches!(
                    behavior,
                    ScriptedDetachBehavior::ReleaseWrongSubscriptionAck
                );
                Some(tokio::spawn(async move {
                    let mut write_rx = write_rx;
                    while let Some(job) = write_rx.recv().await {
                        match job.request {
                            ClientRequest::Detach(request) => {
                                let connection_id = if wrong_connection {
                                    uuid::Uuid::from_bytes([
                                        0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00,
                                        0x00, 0x00, 0x00, 0x00, 0x00, 0xff,
                                    ])
                                } else {
                                    request.connection_id
                                };
                                if dispatch_server_message(
                                    &reader_state,
                                    &reader_unsolicited,
                                    ServerMessage::Detached(DetachAck {
                                        request_id: request.request_id,
                                        connection_id,
                                    }),
                                )
                                .await
                                .is_err()
                                {
                                    break;
                                }
                            }
                            ClientRequest::Query(request)
                                if release_query_error
                                    && matches!(
                                        request.query,
                                        Query::ReleaseEventReplay { .. }
                                    ) =>
                            {
                                if dispatch_server_message(
                                    &reader_state,
                                    &reader_unsolicited,
                                    ServerMessage::QueryReply(QueryReply {
                                        request_id: request.request_id,
                                        outcome: QueryOutcome::Err(QueryError::Unauthorized),
                                    }),
                                )
                                .await
                                .is_err()
                                {
                                    break;
                                }
                            }
                            ClientRequest::Query(request)
                                if release_wrong_subscription
                                    && matches!(
                                        request.query,
                                        Query::ReleaseEventReplay { .. }
                                    ) =>
                            {
                                if dispatch_server_message(
                                    &reader_state,
                                    &reader_unsolicited,
                                    ServerMessage::QueryReply(QueryReply {
                                        request_id: request.request_id,
                                        outcome: QueryOutcome::Ok(
                                            QueryResult::EventReplayReleased {
                                                subscription_id: SubscriptionId::new(),
                                            },
                                        ),
                                    }),
                                )
                                .await
                                .is_err()
                                {
                                    break;
                                }
                            }
                            _ => {}
                        }
                    }
                    reader_unsolicited.close();
                    poison_mutex(&terminal_state);
                }))
            }
        };

        Self {
            shared: Arc::new(SharedConnection {
                client_id,
                server_hello,
                state,
                write_tx,
                unsolicited,
                io_task: Mutex::new(io_task),
            }),
        }
    }

    #[cfg(test)]
    pub(crate) fn shares_state_with(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.shared, &other.shared)
    }

    #[cfg(test)]
    pub(crate) fn push_durable_for_test(
        &self,
        message: UnsolicitedServerMessage,
    ) -> Result<(), IpcError> {
        self.shared.unsolicited.push_durable(message)
    }

    #[cfg(test)]
    fn detach_waiter_registration_id(&self, request_id: RequestId) -> Option<u64> {
        self.shared
            .state
            .lock()
            .expect("client connection state")
            .detach_waiters
            .get(&request_id)
            .map(|pending| pending.registration_id)
    }

    #[cfg(test)]
    fn detach_waiter_count(&self) -> usize {
        self.shared
            .state
            .lock()
            .expect("client connection state")
            .detach_waiters
            .len()
    }

    pub fn is_poisoned(&self) -> bool {
        self.shared
            .state
            .lock()
            .expect("client connection state")
            .poisoned
    }

    pub async fn execute_command(
        &self,
        envelope: CommandEnvelope,
    ) -> Result<CommandReceipt, IpcError> {
        let command_id = envelope.command_id;
        let (reply_tx, reply_rx) = oneshot::channel();
        let mut registration = self.register_waiter(
            PendingKey::Command(command_id),
            PendingKind::Command(reply_tx),
        )?;
        if let Err(error) = self.enqueue_write(ClientRequest::Command(envelope)).await {
            self.fail_closed();
            return Err(error);
        }
        if let Err(error) = registration
            .arm_response_deadline(self.io_abort_handle()?, request_completion_timeout())
        {
            self.fail_closed();
            return Err(error);
        }
        reply_rx.await.map_err(|_| IpcError::Unavailable)?
    }

    pub async fn query(&self, envelope: QueryEnvelope) -> Result<QueryReply, IpcError> {
        let request_id = envelope.request_id;
        let (reply_tx, reply_rx) = oneshot::channel();
        let mut registration =
            self.register_waiter(PendingKey::Query(request_id), PendingKind::Query(reply_tx))?;
        if let Err(error) = self.enqueue_write(ClientRequest::Query(envelope)).await {
            self.fail_closed();
            return Err(error);
        }
        if let Err(error) = registration
            .arm_response_deadline(self.io_abort_handle()?, request_completion_timeout())
        {
            self.fail_closed();
            return Err(error);
        }
        reply_rx.await.map_err(|_| IpcError::Unavailable)?
    }

    /// Request host-acknowledged detach for this connection's wire connection_id.
    pub async fn detach(&self, request: DetachRequest) -> Result<DetachAck, IpcError> {
        let request_id = request.request_id;
        let expected_connection_id = request.connection_id;
        let (reply_tx, reply_rx) = oneshot::channel();
        let mut registration = self.register_waiter(
            PendingKey::Detach(request_id),
            PendingKind::Detach(reply_tx),
        )?;
        if let Err(error) = self.enqueue_write(ClientRequest::Detach(request)).await {
            self.fail_closed();
            return Err(error);
        }
        if let Err(error) = registration
            .arm_response_deadline(self.io_abort_handle()?, request_completion_timeout())
        {
            self.fail_closed();
            return Err(error);
        }
        let ack = reply_rx.await.map_err(|_| IpcError::Unavailable)??;
        if ack.request_id != request_id || ack.connection_id != expected_connection_id {
            self.fail_closed();
            return Err(IpcError::CorrelationMismatch);
        }
        Ok(ack)
    }

    /// Receive the next unsolicited message, preferring durable/resync over streams.
    pub async fn recv_unsolicited(&self) -> Result<UnsolicitedServerMessage, IpcError> {
        self.shared.unsolicited.recv().await
    }

    /// Fence a released durable subscription and drain only its exact queued
    /// tail. Other durable frames remain ordered for the next generation.
    pub(crate) async fn retire_subscription_id(
        &self,
        subscription_id: SubscriptionId,
    ) -> Result<(), IpcError> {
        self.shared
            .unsolicited
            .retire_subscription_id(subscription_id)
    }

    fn register_waiter(
        &self,
        key: PendingKey,
        waiter: PendingKind,
    ) -> Result<PendingRegistration, IpcError> {
        let mut state = self.shared.state.lock().expect("client connection state");
        if state.poisoned || state.closed {
            return Err(if state.poisoned {
                IpcError::ConnectionPoisoned
            } else {
                IpcError::Unavailable
            });
        }
        let registration_id = state.next_registration_id;
        state.next_registration_id = state
            .next_registration_id
            .checked_add(1)
            .ok_or(IpcError::Busy)?;
        match (key, waiter) {
            (PendingKey::Command(id), PendingKind::Command(tx)) => {
                if state.command_waiters.contains_key(&id) {
                    return Err(IpcError::DuplicateInFlight);
                }
                state.command_waiters.insert(
                    id,
                    PendingReply {
                        registration_id,
                        sender: tx,
                        deadline_task: None,
                    },
                );
            }
            (PendingKey::Query(id), PendingKind::Query(tx)) => {
                if state.query_waiters.contains_key(&id) {
                    return Err(IpcError::DuplicateInFlight);
                }
                state.query_waiters.insert(
                    id,
                    PendingReply {
                        registration_id,
                        sender: tx,
                        deadline_task: None,
                    },
                );
            }
            (PendingKey::Detach(id), PendingKind::Detach(tx)) => {
                if state.detach_waiters.contains_key(&id) {
                    return Err(IpcError::DuplicateInFlight);
                }
                state.detach_waiters.insert(
                    id,
                    PendingReply {
                        registration_id,
                        sender: tx,
                        deadline_task: None,
                    },
                );
            }
            _ => unreachable!("waiter kind must match key"),
        }
        drop(state);
        Ok(PendingRegistration::new(
            Arc::clone(&self.shared.state),
            key,
            registration_id,
        ))
    }

    async fn enqueue_write(&self, request: ClientRequest) -> Result<(), IpcError> {
        self.shared
            .write_tx
            .send(WriteJob { request })
            .await
            .map_err(|_| IpcError::Unavailable)
    }

    fn io_abort_handle(&self) -> Result<tokio::task::AbortHandle, IpcError> {
        self.shared
            .io_task
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .as_ref()
            .map(tokio::task::JoinHandle::abort_handle)
            .ok_or(IpcError::Unavailable)
    }

    fn fail_closed(&self) {
        poison_mutex(&self.shared.state);
        let task = self
            .shared
            .io_task
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(task) = task.as_ref() {
            task.abort();
        }
    }
}

impl Drop for SharedConnection {
    fn drop(&mut self) {
        poison_mutex(&self.state);
        let task = self
            .io_task
            .get_mut()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(task) = task.take() {
            task.abort();
        }
    }
}

/// Connect to a host pipe endpoint, complete Hello, and retain the duplex pipe.
pub async fn connect(endpoint: &str, hello: &ClientHello) -> Result<ClientConnection, IpcError> {
    #[cfg(windows)]
    {
        windows_connect(endpoint, hello).await
    }
    #[cfg(not(windows))]
    {
        let _ = endpoint;
        let _ = hello;
        Err(IpcError::Unsupported)
    }
}

/// Connect, complete Hello, and drop the retained pipe (compatibility wrapper).
pub async fn perform_client_hello(
    endpoint: &str,
    hello: &ClientHello,
) -> Result<ServerHello, IpcError> {
    let connection = connect(endpoint, hello).await?;
    Ok(connection.server_hello())
}

#[cfg(windows)]
async fn windows_connect(
    endpoint: &str,
    hello: &ClientHello,
) -> Result<ClientConnection, IpcError> {
    use tokio::io::AsyncWriteExt;
    use tokio::net::windows::named_pipe::ClientOptions;

    let (hello_physical, hello_message) = handshake_codecs()?;
    let encoded = hello_message.encode(hello).map_err(IpcError::MessagePack)?;

    let mut pipe = ClientOptions::new().open(endpoint).map_err(IpcError::Io)?;

    let server_hello = tokio::time::timeout(handshake_timeout(), async {
        write_physical_frame(&mut pipe, &hello_physical, &encoded).await?;
        pipe.flush().await.map_err(IpcError::Io)?;
        let payload = read_physical_frame(&mut pipe, &hello_physical).await?;
        hello_message
            .decode::<ServerHello>(&payload)
            .map_err(IpcError::MessagePack)
    })
    .await
    .map_err(|_| IpcError::Timeout)??;

    validate_server_hello(hello, &server_hello)?;
    let (physical, message) = codecs_for_limits(server_hello.limits)?;
    Ok(spawn_duplex_supervisor(
        hello.client_id,
        server_hello,
        physical,
        message,
        pipe,
    ))
}

#[cfg(windows)]
fn spawn_duplex_supervisor(
    client_id: ClientId,
    server_hello: ServerHello,
    physical: PhysicalFrameCodec,
    message: MessagePackCodec,
    pipe: tokio::net::windows::named_pipe::NamedPipeClient,
) -> ClientConnection {
    let (mut reader, mut writer) = tokio::io::split(pipe);
    let (write_tx, mut write_rx) = mpsc::channel::<WriteJob>(WRITE_QUEUE_CAPACITY);
    let unsolicited = Arc::new(UnsolicitedInbox::new(
        UNSOLICITED_QUEUE_CAPACITY,
        UNSOLICITED_STREAM_CAPACITY,
    ));

    let state = Arc::new(Mutex::new(SharedState::default()));
    let reader_state = Arc::clone(&state);
    let terminal_state = Arc::clone(&state);
    let reader_unsolicited = Arc::clone(&unsolicited);
    let terminal_unsolicited = Arc::clone(&unsolicited);
    let io_task = tokio::spawn(async move {
        let writer = async move {
            while let Some(job) = write_rx.recv().await {
                let encoded = message
                    .encode(&job.request)
                    .map_err(IpcError::MessagePack)?;
                write_physical_frame_with_deadline(
                    &mut writer,
                    &physical,
                    &encoded,
                    request_completion_timeout(),
                )
                .await?;
            }
            Err::<(), IpcError>(IpcError::Unavailable)
        };

        let reader = async move {
            loop {
                let payload = read_physical_frame_idle_then_deadline(
                    &mut reader,
                    &physical,
                    request_completion_timeout(),
                )
                .await?;
                let server_message = message
                    .decode::<ServerMessage>(&payload)
                    .map_err(IpcError::MessagePack)?;
                dispatch_server_message(&reader_state, &reader_unsolicited, server_message).await?;
            }
        };

        let _ = supervise_duplex_halves(reader, writer).await;
        terminal_unsolicited.close();
        poison_mutex(&terminal_state);
    });

    ClientConnection {
        shared: Arc::new(SharedConnection {
            client_id,
            server_hello,
            state,
            write_tx,
            unsolicited,
            io_task: Mutex::new(Some(io_task)),
        }),
    }
}

async fn dispatch_server_message(
    state: &Arc<Mutex<SharedState>>,
    unsolicited: &UnsolicitedInbox,
    message: ServerMessage,
) -> Result<(), IpcError> {
    match message {
        ServerMessage::CommandReceipt(receipt) => {
            let command_id = receipt.command_id();
            let waiter = {
                let mut guard = state.lock().expect("client connection state");
                guard.command_waiters.remove(&command_id)
            };
            match waiter {
                Some(pending) => {
                    pending.complete(Ok(receipt));
                    Ok(())
                }
                None => Err(IpcError::CorrelationMismatch),
            }
        }
        ServerMessage::QueryReply(reply) => {
            let request_id = reply.request_id;
            let waiter = {
                let mut guard = state.lock().expect("client connection state");
                guard.query_waiters.remove(&request_id)
            };
            match waiter {
                Some(pending) => {
                    pending.complete(Ok(reply));
                    Ok(())
                }
                None => Err(IpcError::CorrelationMismatch),
            }
        }
        ServerMessage::DurableEvent {
            subscription_id,
            event,
        } => unsolicited.push_durable(UnsolicitedServerMessage::DurableEvent {
            subscription_id,
            event,
        }),
        ServerMessage::ResyncRequired {
            subscription_id,
            last_delivered_sequence,
            newest_sequence,
        } => unsolicited.push_durable(UnsolicitedServerMessage::ResyncRequired {
            subscription_id,
            last_delivered_sequence,
            newest_sequence,
        }),
        ServerMessage::Stream(frame) => unsolicited.push_stream(frame),
        ServerMessage::Detached(ack) => {
            let request_id = ack.request_id;
            let waiter = {
                let mut guard = state.lock().expect("client connection state");
                guard.detach_waiters.remove(&request_id)
            };
            match waiter {
                Some(pending) => {
                    pending.complete(Ok(ack));
                    Ok(())
                }
                None => Err(IpcError::CorrelationMismatch),
            }
        }
    }
}

fn remove_pending_exact(
    state: &Arc<Mutex<SharedState>>,
    key: PendingKey,
    registration_id: u64,
) -> Option<ErasedPendingReply> {
    let mut guard = state
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    match key {
        PendingKey::Command(id)
            if guard
                .command_waiters
                .get(&id)
                .is_some_and(|pending| pending.registration_id == registration_id) =>
        {
            guard
                .command_waiters
                .remove(&id)
                .map(ErasedPendingReply::Command)
        }
        PendingKey::Query(id)
            if guard
                .query_waiters
                .get(&id)
                .is_some_and(|pending| pending.registration_id == registration_id) =>
        {
            guard
                .query_waiters
                .remove(&id)
                .map(ErasedPendingReply::Query)
        }
        PendingKey::Detach(id)
            if guard
                .detach_waiters
                .get(&id)
                .is_some_and(|pending| pending.registration_id == registration_id) =>
        {
            guard
                .detach_waiters
                .remove(&id)
                .map(ErasedPendingReply::Detach)
        }
        _ => None,
    }
}

fn poison_mutex(state: &Arc<Mutex<SharedState>>) {
    let mut guard = state
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if guard.closed && guard.poisoned {
        return;
    }
    guard.poisoned = true;
    guard.closed = true;
    let commands = std::mem::take(&mut guard.command_waiters);
    let queries = std::mem::take(&mut guard.query_waiters);
    let detaches = std::mem::take(&mut guard.detach_waiters);
    drop(guard);
    for (_, pending) in commands {
        pending.complete(Err(IpcError::Unavailable));
    }
    for (_, pending) in queries {
        pending.complete(Err(IpcError::Unavailable));
    }
    for (_, pending) in detaches {
        pending.complete(Err(IpcError::Unavailable));
    }
}

fn validate_server_hello(sent: &ClientHello, received: &ServerHello) -> Result<(), IpcError> {
    if received.profile_fingerprint != sent.profile_fingerprint {
        return Err(IpcError::ProfileMismatch);
    }
    if received.protocol_major != sent.protocol_major {
        return Err(IpcError::HelloInconsistent);
    }
    if received.protocol_minor > sent.protocol_minor {
        return Err(IpcError::HelloInconsistent);
    }
    if received.granted.bits() & !sent.requested.bits() != 0 {
        return Err(IpcError::HelloInconsistent);
    }
    if !limits_within_offer_and_caps(received.limits, sent.limits) {
        return Err(IpcError::HelloInconsistent);
    }
    Ok(())
}

fn limits_within_offer_and_caps(got: FrameLimits, offer: FrameLimits) -> bool {
    got.validate_offer().is_ok()
        && got.max_physical_frame_bytes
            <= offer.max_physical_frame_bytes.min(MAX_PHYSICAL_FRAME_BYTES)
        && got.max_reassembled_message_bytes
            <= offer
                .max_reassembled_message_bytes
                .min(MAX_REASSEMBLED_MESSAGE_BYTES)
        && got.max_page_items <= offer.max_page_items.min(MAX_SNAPSHOT_PAGE_ITEMS)
        && got.max_page_encoded_bytes
            <= offer
                .max_page_encoded_bytes
                .min(MAX_SNAPSHOT_PAGE_ENCODED_BYTES)
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use tokio::sync::oneshot;

    use super::{
        PendingKey, PendingRegistration, PendingReply, SharedState, UnsolicitedServerMessage,
        MAX_RETIRED_DRAIN_WORK, MAX_RETIRED_SUBSCRIPTION_IDS,
    };
    use crate::domain::command::CommandReceipt;
    use crate::domain::id::CommandId;
    use crate::host::IpcError;

    fn command_id(tail: u8) -> CommandId {
        let mut bytes = [0_u8; 16];
        bytes[0] = 0x01;
        bytes[6] = 0x70;
        bytes[8] = 0x80;
        bytes[15] = tail;
        CommandId::from_bytes(bytes).expect("command id")
    }

    #[test]
    fn detach_cancelled_pre_enqueue_registration_removes_the_unsent_waiter() {
        use super::{ClientConnection, PendingKind, ScriptedDetachBehavior};
        use crate::domain::id::RequestId;
        use crate::domain::ClientId;
        use crate::protocol::{Capability, CapabilitySet, DetachAck, FrameLimits, ServerHello};
        use crate::protocol::{ProfileFingerprint, PROTOCOL_MAJOR, PROTOCOL_MINOR};

        let client_id = ClientId::from_bytes([
            0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0xd0,
        ])
        .expect("client");
        let connection_id = uuid::Uuid::from_bytes([
            0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0xd1,
        ]);
        let hello = ServerHello {
            protocol_major: PROTOCOL_MAJOR,
            protocol_minor: PROTOCOL_MINOR,
            server_build: "test".into(),
            host_boot_id: connection_id,
            connection_id,
            profile_fingerprint: ProfileFingerprint::hash_normalized("detach-unit"),
            granted: CapabilitySet::from_capabilities([Capability::ExplicitDetach]),
            limits: FrameLimits::v1_default(),
            reconnect_grant: None,
        };
        let conn = ClientConnection::scripted_for_test(
            client_id,
            hello,
            ScriptedDetachBehavior::ClosedWriteQueue,
        );
        let id = RequestId::from_bytes([
            0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0xd2,
        ])
        .expect("request id");
        let (reply_tx, _reply_rx) = oneshot::channel::<Result<DetachAck, IpcError>>();
        let registration = conn
            .register_waiter(PendingKey::Detach(id), PendingKind::Detach(reply_tx))
            .expect("register");
        assert_eq!(conn.detach_waiter_count(), 1);
        drop(registration);
        assert_eq!(
            conn.detach_waiter_count(),
            0,
            "cancelling before enqueue must release the unsent detach correlation id"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn detach_cancelled_post_enqueue_retains_deadline_then_poisons_at_expiry() {
        use super::{ClientConnection, PendingKind, ScriptedDetachBehavior};
        use crate::domain::id::RequestId;
        use crate::domain::ClientId;
        use crate::protocol::{Capability, CapabilitySet, DetachAck, FrameLimits, ServerHello};
        use crate::protocol::{ProfileFingerprint, PROTOCOL_MAJOR, PROTOCOL_MINOR};

        let client_id = ClientId::from_bytes([
            0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0xd3,
        ])
        .expect("client");
        let connection_id = uuid::Uuid::from_bytes([
            0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0xd4,
        ]);
        let hello = ServerHello {
            protocol_major: PROTOCOL_MAJOR,
            protocol_minor: PROTOCOL_MINOR,
            server_build: "test".into(),
            host_boot_id: connection_id,
            connection_id,
            profile_fingerprint: ProfileFingerprint::hash_normalized("detach-unit"),
            granted: CapabilitySet::from_capabilities([Capability::ExplicitDetach]),
            limits: FrameLimits::v1_default(),
            reconnect_grant: None,
        };
        // MatchingAck keeps a live write consumer; we only exercise the deadline path.
        let conn = ClientConnection::scripted_for_test(
            client_id,
            hello,
            ScriptedDetachBehavior::MatchingAck,
        );
        let id = RequestId::from_bytes([
            0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0xd5,
        ])
        .expect("request id");
        let (reply_tx, reply_rx) = oneshot::channel::<Result<DetachAck, IpcError>>();
        let mut registration = conn
            .register_waiter(PendingKey::Detach(id), PendingKind::Detach(reply_tx))
            .expect("register");
        let io_abort = conn.io_abort_handle().expect("scripted I/O");
        registration
            .arm_response_deadline(io_abort, Duration::from_millis(20))
            .expect("arm detach deadline");
        drop(registration);
        drop(reply_rx);

        assert_eq!(
            conn.detach_waiter_count(),
            1,
            "post-enqueue cancel must leave the connection-owned detach deadline armed"
        );

        tokio::time::sleep(Duration::from_millis(50)).await;

        assert_eq!(conn.detach_waiter_count(), 0);
        assert!(
            conn.is_poisoned(),
            "deadline must poison and close the connection"
        );
        let guard = conn.shared.state.lock().expect("state");
        assert!(guard.closed);
        drop(guard);
        let task = conn
            .shared
            .io_task
            .lock()
            .expect("io task")
            .take()
            .expect("io task");
        assert!(
            task.await
                .expect_err("deadline must abort I/O")
                .is_cancelled(),
            "timed-out connection I/O must be cancelled"
        );
    }

    #[test]
    fn detach_duplicate_request_id_is_rejected_without_replacing_waiter() {
        use super::{ClientConnection, PendingKind, ScriptedDetachBehavior};
        use crate::domain::id::RequestId;
        use crate::domain::ClientId;
        use crate::protocol::{Capability, CapabilitySet, DetachAck, FrameLimits, ServerHello};
        use crate::protocol::{ProfileFingerprint, PROTOCOL_MAJOR, PROTOCOL_MINOR};

        let client_id = ClientId::from_bytes([
            0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0xd6,
        ])
        .expect("client");
        let connection_id = uuid::Uuid::from_bytes([
            0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0xd7,
        ]);
        let hello = ServerHello {
            protocol_major: PROTOCOL_MAJOR,
            protocol_minor: PROTOCOL_MINOR,
            server_build: "test".into(),
            host_boot_id: connection_id,
            connection_id,
            profile_fingerprint: ProfileFingerprint::hash_normalized("detach-unit"),
            granted: CapabilitySet::from_capabilities([Capability::ExplicitDetach]),
            limits: FrameLimits::v1_default(),
            reconnect_grant: None,
        };
        let conn = ClientConnection::scripted_for_test(
            client_id,
            hello,
            ScriptedDetachBehavior::ClosedWriteQueue,
        );
        let id = RequestId::from_bytes([
            0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0xd8,
        ])
        .expect("request id");
        let (first_tx, _first_rx) = oneshot::channel::<Result<DetachAck, IpcError>>();
        let first = conn
            .register_waiter(PendingKey::Detach(id), PendingKind::Detach(first_tx))
            .expect("first register");
        let before = conn
            .detach_waiter_registration_id(id)
            .expect("first generation");

        let (second_tx, _second_rx) = oneshot::channel::<Result<DetachAck, IpcError>>();
        assert!(matches!(
            conn.register_waiter(PendingKey::Detach(id), PendingKind::Detach(second_tx)),
            Err(IpcError::DuplicateInFlight)
        ));
        assert_eq!(
            conn.detach_waiter_registration_id(id),
            Some(before),
            "duplicate detach RequestId must not replace the original waiter"
        );
        drop(first);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn detach_wrong_connection_ack_fail_closes_without_reusable_pending() {
        use super::{ClientConnection, ScriptedDetachBehavior};
        use crate::domain::id::RequestId;
        use crate::domain::ClientId;
        use crate::protocol::{Capability, CapabilitySet, DetachRequest, FrameLimits, ServerHello};
        use crate::protocol::{ProfileFingerprint, PROTOCOL_MAJOR, PROTOCOL_MINOR};

        let client_id = ClientId::from_bytes([
            0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0xd9,
        ])
        .expect("client");
        let connection_id = uuid::Uuid::from_bytes([
            0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0xda,
        ]);
        let hello = ServerHello {
            protocol_major: PROTOCOL_MAJOR,
            protocol_minor: PROTOCOL_MINOR,
            server_build: "test".into(),
            host_boot_id: connection_id,
            connection_id,
            profile_fingerprint: ProfileFingerprint::hash_normalized("detach-unit"),
            granted: CapabilitySet::from_capabilities([Capability::ExplicitDetach]),
            limits: FrameLimits::v1_default(),
            reconnect_grant: None,
        };
        let conn = ClientConnection::scripted_for_test(
            client_id,
            hello,
            ScriptedDetachBehavior::WrongConnectionAck,
        );
        let request_id = RequestId::from_bytes([
            0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0xdb,
        ])
        .expect("request id");
        assert!(matches!(
            conn.detach(DetachRequest {
                request_id,
                client_id,
                connection_id,
            })
            .await,
            Err(IpcError::CorrelationMismatch)
        ));
        assert_eq!(
            conn.detach_waiter_count(),
            0,
            "fail-closed detach must leave no reusable pending entry"
        );
        assert!(conn.is_poisoned());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn detach_transport_failure_fail_closes_without_reusable_pending() {
        use super::{ClientConnection, ScriptedDetachBehavior};
        use crate::domain::id::RequestId;
        use crate::domain::ClientId;
        use crate::protocol::{Capability, CapabilitySet, DetachRequest, FrameLimits, ServerHello};
        use crate::protocol::{ProfileFingerprint, PROTOCOL_MAJOR, PROTOCOL_MINOR};

        let client_id = ClientId::from_bytes([
            0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0xdc,
        ])
        .expect("client");
        let connection_id = uuid::Uuid::from_bytes([
            0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0xdd,
        ]);
        let hello = ServerHello {
            protocol_major: PROTOCOL_MAJOR,
            protocol_minor: PROTOCOL_MINOR,
            server_build: "test".into(),
            host_boot_id: connection_id,
            connection_id,
            profile_fingerprint: ProfileFingerprint::hash_normalized("detach-unit"),
            granted: CapabilitySet::from_capabilities([Capability::ExplicitDetach]),
            limits: FrameLimits::v1_default(),
            reconnect_grant: None,
        };
        let conn = ClientConnection::scripted_for_test(
            client_id,
            hello,
            ScriptedDetachBehavior::ClosedWriteQueue,
        );
        let request_id = RequestId::from_bytes([
            0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0xde,
        ])
        .expect("request id");
        assert!(matches!(
            conn.detach(DetachRequest {
                request_id,
                client_id,
                connection_id,
            })
            .await,
            Err(IpcError::Unavailable)
        ));
        assert_eq!(
            conn.detach_waiter_count(),
            0,
            "transport failure must leave no reusable detach waiter"
        );
        assert!(conn.is_poisoned());
    }

    #[test]
    fn cancelled_pre_enqueue_registration_removes_the_unsent_waiter() {
        let state = Arc::new(Mutex::new(SharedState::default()));
        let id = command_id(0xc1);
        let (reply_tx, _reply_rx) = oneshot::channel::<Result<CommandReceipt, IpcError>>();
        state.lock().expect("state").command_waiters.insert(
            id,
            PendingReply {
                registration_id: 7,
                sender: reply_tx,
                deadline_task: None,
            },
        );

        let registration = PendingRegistration::new(Arc::clone(&state), PendingKey::Command(id), 7);
        drop(registration);

        assert!(
            !state
                .lock()
                .expect("state")
                .command_waiters
                .contains_key(&id),
            "cancelling before enqueue must release the unsent correlation id"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn cancelled_post_enqueue_call_retains_connection_owned_deadline() {
        let state = Arc::new(Mutex::new(SharedState::default()));
        let id = command_id(0xc2);
        let (reply_tx, reply_rx) = oneshot::channel::<Result<CommandReceipt, IpcError>>();
        state.lock().expect("state").command_waiters.insert(
            id,
            PendingReply {
                registration_id: 8,
                sender: reply_tx,
                deadline_task: None,
            },
        );

        let mut registration =
            PendingRegistration::new(Arc::clone(&state), PendingKey::Command(id), 8);
        let io_task = tokio::spawn(std::future::pending::<()>());
        registration
            .arm_response_deadline(io_task.abort_handle(), Duration::from_millis(20))
            .expect("arm response deadline");

        drop(reply_rx);
        tokio::time::sleep(Duration::from_millis(50)).await;

        let guard = state.lock().expect("state");
        assert!(
            !guard.command_waiters.contains_key(&id),
            "the connection-owned deadline must release the correlation id after caller cancellation"
        );
        assert!(guard.poisoned && guard.closed);
        drop(guard);
        assert!(
            io_task
                .await
                .expect_err("deadline must abort I/O")
                .is_cancelled(),
            "timed-out connection I/O must be cancelled"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn stream_saturated_ephemeral_lane_preserves_durable_dispatch_and_priority() {
        // Catches: new-key-at-capacity must drop without failing I/O; durable remains
        // admissible and preferred; retained stream keeps latest coalesced payload.
        use super::UnsolicitedInbox;
        use crate::domain::event::{DomainEvent, Event};
        use crate::domain::id::{EventId, ResourceId, SubscriptionId};
        use crate::protocol::{StreamFrame, StreamKey, StreamPayloadKind};

        let inbox = UnsolicitedInbox::new_for_test(2, 1);
        let key_a = StreamKey::from(
            ResourceId::from_bytes([
                0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00,
                0x00, 0x71,
            ])
            .expect("resource a"),
        );
        let key_b = StreamKey::from(
            ResourceId::from_bytes([
                0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00,
                0x00, 0x75,
            ])
            .expect("resource b"),
        );
        let sub = SubscriptionId::from_bytes([
            0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x72,
        ])
        .expect("sub");
        let frame = |stream: StreamKey, marker: u8| StreamFrame {
            subscription_id: sub,
            stream,
            generation: 1,
            sequence: u64::from(marker),
            payload_kind: StreamPayloadKind::new(1).expect("kind"),
            schema_version: 1,
            payload: vec![marker],
        };

        inbox
            .push_stream(frame(key_a, 1))
            .expect("first stream admits");
        inbox
            .push_stream(frame(key_a, 2))
            .expect("same-key coalesce must not fail closed");
        inbox
            .push_stream(frame(key_b, 9))
            .expect("distinct key at capacity must drop without failing I/O");

        let durable = UnsolicitedServerMessage::DurableEvent {
            subscription_id: SubscriptionId::from_bytes([
                0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00,
                0x00, 0x73,
            ])
            .expect("sub"),
            event: DomainEvent {
                id: EventId::from_bytes([
                    0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00, 0x00,
                    0x00, 0x00, 0x74,
                ])
                .expect("event"),
                task_id: None,
                sequence: 9,
                task_revision: None,
                occurred_at_ms: 1,
                payload: Event::TaskReopened,
            },
        };
        inbox
            .push_durable(durable.clone())
            .expect("durable must still admit after ephemeral capacity drop");

        let first = inbox.recv().await.expect("recv durable first");
        assert_eq!(first, durable);
        let second = inbox.recv().await.expect("recv retained stream A");
        match second {
            UnsolicitedServerMessage::Stream(got) => {
                assert_eq!(got.stream, key_a);
                assert_eq!(got.payload, vec![2], "retained A must keep latest coalesce");
            }
            other => panic!("expected stream A after durable, got {other:?}"),
        }
        let third = tokio::time::timeout(Duration::from_millis(50), inbox.recv()).await;
        assert!(
            third.is_err(),
            "dropped key B must never appear in the stream lane"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn stream_concurrent_recv_drains_two_queued_durables_without_hang() {
        // Catches: concurrent recv must serialize on the durable receiver so a
        // try_lock miss cannot consume the Notify permit and hang while durables
        // remain queued.
        use super::UnsolicitedInbox;
        use crate::domain::event::{DomainEvent, Event};
        use crate::domain::id::{EventId, SubscriptionId};
        use std::sync::Arc;
        use std::time::Duration;

        let inbox = Arc::new(UnsolicitedInbox::new_for_test(4, 1));
        let durable = |tail: u8| UnsolicitedServerMessage::DurableEvent {
            subscription_id: SubscriptionId::from_bytes([
                0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00,
                0x00, 0x80,
            ])
            .expect("sub"),
            event: DomainEvent {
                id: EventId::from_bytes([
                    0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00, 0x00,
                    0x00, 0x00, tail,
                ])
                .expect("event"),
                task_id: None,
                sequence: u64::from(tail),
                task_revision: None,
                occurred_at_ms: 1,
                payload: Event::TaskReopened,
            },
        };
        let first = durable(0x81);
        let second = durable(0x82);
        inbox
            .push_durable(first.clone())
            .expect("queue first durable");
        inbox
            .push_durable(second.clone())
            .expect("queue second durable");

        let left = Arc::clone(&inbox);
        let right = Arc::clone(&inbox);
        let left_task = tokio::spawn(async move {
            tokio::time::timeout(Duration::from_millis(500), left.recv()).await
        });
        let right_task = tokio::spawn(async move {
            tokio::time::timeout(Duration::from_millis(500), right.recv()).await
        });

        let left_result = left_task
            .await
            .expect("left join")
            .expect("left recv timed out / hung despite queued durables")
            .expect("left recv");
        let right_result = right_task
            .await
            .expect("right join")
            .expect("right recv timed out / hung despite queued durables")
            .expect("right recv");

        let mut got = [left_result, right_result];
        got.sort_by_key(|message| match message {
            UnsolicitedServerMessage::DurableEvent { event, .. } => event.sequence,
            _ => u64::MAX,
        });
        assert_eq!(got[0], first);
        assert_eq!(got[1], second);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn continuous_durable_producer_cannot_starve_current_stream_lane() {
        use super::UnsolicitedInbox;
        use crate::domain::event::{DomainEvent, Event};
        use crate::domain::id::{EventId, ResourceId, SubscriptionId};
        use crate::protocol::{StreamFrame, StreamKey, StreamPayloadKind};
        use std::sync::{Arc, Barrier};

        let inbox = Arc::new(UnsolicitedInbox::new_for_test(128, 1));
        let subscription = SubscriptionId::from_bytes([
            0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0xa1,
        ])
        .expect("subscription");
        let stream = StreamKey::from(
            ResourceId::from_bytes([
                0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00,
                0x00, 0xa2,
            ])
            .expect("stream resource"),
        );
        let step = Arc::new(Barrier::new(2));
        let producer = {
            let inbox = Arc::clone(&inbox);
            let step = Arc::clone(&step);
            std::thread::spawn(move || {
                for marker in 0..64u8 {
                    inbox
                        .push_durable(UnsolicitedServerMessage::DurableEvent {
                            subscription_id: subscription,
                            event: DomainEvent {
                                id: EventId::from_bytes([
                                    0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00,
                                    0x00, 0x00, 0x00, 0x00, 0x00, marker,
                                ])
                                .expect("event"),
                                task_id: None,
                                sequence: u64::from(marker),
                                task_revision: None,
                                occurred_at_ms: i64::from(marker),
                                payload: Event::TaskReopened,
                            },
                        })
                        .expect("durable admission");
                    inbox
                        .push_stream(StreamFrame {
                            subscription_id: subscription,
                            stream,
                            generation: 1,
                            sequence: u64::from(marker),
                            payload_kind: StreamPayloadKind::new(1).expect("payload kind"),
                            schema_version: 1,
                            payload: vec![marker],
                        })
                        .expect("stream admission");
                    step.wait();
                    step.wait();
                }
            })
        };

        let mut stream_count = 0usize;
        for _ in 0..64 {
            step.wait();
            let message = tokio::time::timeout(Duration::from_millis(250), inbox.recv())
                .await
                .expect("continuous producer must not leave recv waiting")
                .expect("recv");
            if matches!(message, UnsolicitedServerMessage::Stream(_)) {
                stream_count += 1;
            }
            step.wait();
        }
        producer.join().expect("producer join");
        assert!(
            stream_count >= 32,
            "current stream lane must receive a fair share while durables stay continuous; got {stream_count}"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn retained_durable_truth_is_delivered_before_newer_durable_frames() {
        use super::UnsolicitedInbox;
        use crate::domain::event::{DomainEvent, Event};
        use crate::domain::id::{EventId, SubscriptionId};

        let inbox = UnsolicitedInbox::new_for_test(4, 1);
        let old_id = SubscriptionId::from_bytes([
            0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x11,
        ])
        .expect("old subscription");
        let new_id = SubscriptionId::from_bytes([
            0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x12,
        ])
        .expect("new subscription");
        let durable = |subscription_id, sequence| UnsolicitedServerMessage::DurableEvent {
            subscription_id,
            event: DomainEvent {
                id: EventId::from_bytes([
                    0x01,
                    0x8f,
                    0x60,
                    0xb0,
                    0x9c,
                    0x1a,
                    0x70,
                    0x01,
                    0x80,
                    0x00,
                    0x00,
                    0x00,
                    0x00,
                    0x00,
                    0x00,
                    sequence as u8,
                ])
                .expect("event"),
                task_id: None,
                sequence,
                task_revision: None,
                occurred_at_ms: sequence as i64,
                payload: Event::TaskReopened,
            },
        };
        let retained = durable(new_id, 1);
        let newer = durable(new_id, 2);

        inbox.push_durable(retained.clone()).expect("old frame");
        inbox
            .retire_subscription_id(old_id)
            .expect("retire old generation");
        inbox.push_durable(newer.clone()).expect("new frame");

        assert_eq!(inbox.recv().await.expect("retained frame"), retained);
        assert_eq!(inbox.recv().await.expect("new frame"), newer);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn retiring_a_later_generation_also_purges_frames_retained_for_it() {
        use super::UnsolicitedInbox;
        use crate::domain::event::{DomainEvent, Event};
        use crate::domain::id::{EventId, SubscriptionId};

        let inbox = UnsolicitedInbox::new_for_test(4, 1);
        let first_id = SubscriptionId::new();
        let second_id = SubscriptionId::new();
        let retained_for_second = UnsolicitedServerMessage::DurableEvent {
            subscription_id: second_id,
            event: DomainEvent {
                id: EventId::new(),
                task_id: None,
                sequence: 1,
                task_revision: None,
                occurred_at_ms: 1,
                payload: Event::TaskReopened,
            },
        };

        inbox
            .push_durable(retained_for_second)
            .expect("second-generation frame");
        inbox
            .retire_subscription_id(first_id)
            .expect("first-generation retirement");
        inbox
            .retire_subscription_id(second_id)
            .expect("second-generation retirement");

        assert!(
            tokio::time::timeout(Duration::from_millis(50), inbox.recv())
                .await
                .is_err(),
            "a frame retained while retiring one generation must be purged when its own generation retires"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn leftover_retired_ids_after_drain_flood_cannot_poison_recv() {
        use super::UnsolicitedInbox;
        use crate::domain::event::{DomainEvent, Event};
        use crate::domain::id::{EventId, SubscriptionId};

        let inbox = UnsolicitedInbox::new_for_test(128, 1);
        let retired_id = SubscriptionId::from_bytes([
            0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0xe6,
        ])
        .expect("retired subscription");
        let live_id = SubscriptionId::from_bytes([
            0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0xe7,
        ])
        .expect("live subscription");
        let durable =
            |subscription_id: SubscriptionId, tail: u8| UnsolicitedServerMessage::DurableEvent {
                subscription_id,
                event: DomainEvent {
                    id: EventId::from_bytes([
                        0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00,
                        0x00, 0x00, 0x00, tail,
                    ])
                    .expect("event"),
                    task_id: None,
                    sequence: u64::from(tail),
                    task_revision: None,
                    occurred_at_ms: i64::from(tail),
                    payload: Event::TaskReopened,
                },
            };

        for tail in 0..=MAX_RETIRED_DRAIN_WORK as u8 {
            inbox
                .push_durable(durable(retired_id, tail))
                .expect("queue retired-generation frames past one drain quantum");
        }
        let flood = inbox
            .retire_subscription_id(retired_id)
            .expect_err("already-queued retired ids past the drain quantum force typed resync");
        assert!(matches!(
            flood,
            IpcError::RetiredSubscriptionFlood { limit } if limit == MAX_RETIRED_DRAIN_WORK
        ));

        inbox
            .push_durable(durable(live_id, 0xf0))
            .expect("replacement generation still admits live frames");
        assert_eq!(
            inbox.recv().await.expect("live replacement frame"),
            durable(live_id, 0xf0)
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(50), inbox.recv())
                .await
                .is_err(),
            "leftover retired ids must not poison the replacement generation"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn stream_close_during_recv_wait_returns_unavailable_without_hang() {
        // Catches: close between empty-queue check and waiter registration must not
        // lose the notification and hang recv forever.
        use super::UnsolicitedInbox;
        use std::sync::Arc;
        use std::time::Duration;

        let inbox = Arc::new(UnsolicitedInbox::new_for_test(2, 1));
        let closer = Arc::clone(&inbox);
        inbox.install_close_gap_hook(Box::new(move || closer.close()));

        let recv = tokio::time::timeout(Duration::from_millis(200), inbox.recv()).await;
        match recv {
            Ok(Err(IpcError::Unavailable)) => {}
            Ok(Ok(other)) => panic!("expected Unavailable after close, got {other:?}"),
            Ok(Err(other)) => panic!("expected Unavailable, got {other:?}"),
            Err(_) => panic!("recv hung after close in the former check/wait gap"),
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn retirement_admission_barrier_never_leaks_an_old_generation_frame() {
        use super::UnsolicitedInbox;
        use crate::domain::event::{DomainEvent, Event};
        use crate::domain::id::{EventId, SubscriptionId};
        use std::sync::{Arc, Barrier};

        let inbox = Arc::new(UnsolicitedInbox::new_for_test(4, 1));
        let subscription_id = SubscriptionId::from_bytes([
            0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x01,
        ])
        .expect("subscription id");
        let event = DomainEvent {
            id: EventId::from_bytes([
                0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00,
                0x00, 0x02,
            ])
            .expect("event id"),
            task_id: None,
            sequence: 1,
            task_revision: None,
            occurred_at_ms: 1,
            payload: Event::TaskReopened,
        };
        let frame = UnsolicitedServerMessage::DurableEvent {
            subscription_id,
            event,
        };
        let entered = Arc::new(Barrier::new(2));
        let release = Arc::new(Barrier::new(2));
        inbox.install_admission_barrier(Arc::clone(&entered), Arc::clone(&release));

        let producer = {
            let inbox = Arc::clone(&inbox);
            let frame = frame.clone();
            tokio::task::spawn_blocking(move || inbox.push_durable(frame))
        };
        entered.wait();
        let retire = {
            let inbox = Arc::clone(&inbox);
            tokio::task::spawn_blocking(move || inbox.retire_subscription_id(subscription_id))
        };
        // The producer is paused while holding the same fence as retirement.
        // Releasing the barrier chooses a deterministic linearization point:
        // either the old frame is queued and drained, or it sees retired.
        release.wait();
        producer
            .await
            .expect("producer join")
            .expect("admission must remain bounded");
        retire
            .await
            .expect("retirement join")
            .expect("retirement must remain bounded");

        let next = tokio::time::timeout(Duration::from_millis(50), inbox.recv()).await;
        assert!(next.is_err(), "retired old frame must not reach a consumer");
        assert!(
            inbox.push_durable(frame).is_ok(),
            "late old frame is fenced"
        );
        let next = tokio::time::timeout(Duration::from_millis(50), inbox.recv()).await;
        assert!(next.is_err(), "late retired frame must be dropped");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn retired_id_and_queued_frame_floods_are_typed_and_streams_survive() {
        use super::UnsolicitedInbox;
        use crate::domain::event::{DomainEvent, Event};
        use crate::domain::id::{EventId, ResourceId, SubscriptionId};
        use crate::protocol::{StreamFrame, StreamKey, StreamPayloadKind};

        let subscription = |tail: u8| {
            SubscriptionId::from_bytes([
                0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00,
                0x00, tail,
            ])
            .expect("subscription id")
        };
        let durable =
            |subscription_id: SubscriptionId, tail: u8| UnsolicitedServerMessage::DurableEvent {
                subscription_id,
                event: DomainEvent {
                    id: EventId::from_bytes([
                        0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00,
                        0x00, 0x00, 0x00, tail,
                    ])
                    .expect("event id"),
                    task_id: None,
                    sequence: u64::from(tail),
                    task_revision: None,
                    occurred_at_ms: i64::from(tail),
                    payload: Event::TaskReopened,
                },
            };

        let ids = UnsolicitedInbox::new_for_test(128, 1);
        for tail in 0..MAX_RETIRED_SUBSCRIPTION_IDS as u8 {
            ids.retire_subscription_id(subscription(tail))
                .expect("the bounded retired-id set admits its limit");
        }
        let retired_overflow = ids
            .retire_subscription_id(subscription(MAX_RETIRED_SUBSCRIPTION_IDS as u8))
            .expect_err("the 65th retired id must force typed resync");
        assert!(matches!(
            retired_overflow,
            IpcError::RetiredSubscriptionFlood { limit } if limit == MAX_RETIRED_SUBSCRIPTION_IDS
        ));
        ids.push_durable(durable(subscription(0), 0xf1))
            .expect("old ids remain harmless after the typed flood");
        let dropped = tokio::time::timeout(Duration::from_millis(25), ids.recv()).await;
        assert!(dropped.is_err(), "retired ids must not reach the consumer");

        let flood = UnsolicitedInbox::new_for_test(128, 1);
        let queued_id = subscription(0xe1);
        for tail in 0..(MAX_RETIRED_DRAIN_WORK as u8 + 1) {
            flood
                .push_durable(durable(queued_id, tail))
                .expect("test queue admits more than one drain quantum");
        }
        let queue_overflow = flood
            .retire_subscription_id(subscription(0xe2))
            .expect_err("more than one drain quantum must force typed resync");
        assert!(matches!(
            queue_overflow,
            IpcError::RetiredSubscriptionFlood { limit } if limit == MAX_RETIRED_DRAIN_WORK
        ));

        let stream_key = StreamKey::from(
            ResourceId::from_bytes([
                0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00,
                0x00, 0xe3,
            ])
            .expect("stream resource"),
        );
        flood
            .push_stream(StreamFrame {
                subscription_id: queued_id,
                stream: stream_key,
                generation: 2,
                sequence: 1,
                payload_kind: StreamPayloadKind::new(1).expect("payload kind"),
                schema_version: 1,
                payload: vec![0xe3],
            })
            .expect("current stream must remain admissible after durable flood");
        let mut stream_survived = false;
        for _ in 0..=MAX_RETIRED_DRAIN_WORK + 1 {
            if matches!(
                flood.recv().await.expect("flooded frame"),
                UnsolicitedServerMessage::Stream(frame) if frame.stream == stream_key
            ) {
                stream_survived = true;
                break;
            }
        }
        assert!(
            stream_survived,
            "fair dispatch must preserve a current stream during a retired durable flood"
        );
    }
}
