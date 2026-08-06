//! Cloneable duplex multiplexed client connection after Hello.
//!
//! One shared I/O supervisor owns the named-pipe writer and continuously
//! running reader futures. Concurrent command/query calls register correlation
//! waiters before enqueueing writes. Unsolicited durable messages land in a
//! bounded inbox for later subscription consumers.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::{mpsc, oneshot};

use crate::domain::command::{CommandEnvelope, CommandReceipt};
use crate::domain::event::DomainEvent;
use crate::domain::id::{CommandId, RequestId, SubscriptionId};
use crate::domain::query::{QueryEnvelope, QueryReply};
use crate::domain::{ClientId, MAX_SNAPSHOT_PAGE_ENCODED_BYTES, MAX_SNAPSHOT_PAGE_ITEMS};
use crate::host::{
    codecs_for_limits, handshake_codecs, handshake_timeout, read_physical_frame,
    read_physical_frame_idle_then_deadline, request_completion_timeout, supervise_duplex_halves,
    write_physical_frame, write_physical_frame_with_deadline, IpcError,
};
use crate::protocol::{
    ClientHello, ClientRequest, FrameLimits, MessagePackCodec, PhysicalFrameCodec, ServerHello,
    ServerMessage, StreamFrame, MAX_PHYSICAL_FRAME_BYTES, MAX_REASSEMBLED_MESSAGE_BYTES,
};

const WRITE_QUEUE_CAPACITY: usize = 32;
const UNSOLICITED_QUEUE_CAPACITY: usize = 64;
const UNSOLICITED_STREAM_CAPACITY: usize = 16;

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

/// Durable-priority unsolicited inbox with a separate coalescing stream lane.
struct UnsolicitedInbox {
    durable_tx: mpsc::Sender<UnsolicitedServerMessage>,
    durable_rx: tokio::sync::Mutex<mpsc::Receiver<UnsolicitedServerMessage>>,
    stream_capacity: usize,
    streams: Mutex<std::collections::HashMap<crate::protocol::StreamKey, StreamFrame>>,
    notify: tokio::sync::Notify,
    closed: std::sync::atomic::AtomicBool,
    #[cfg(test)]
    close_gap_hook: Mutex<Option<Box<dyn FnOnce() + Send>>>,
}

impl UnsolicitedInbox {
    fn new(durable_capacity: usize, stream_capacity: usize) -> Self {
        let (durable_tx, durable_rx) = mpsc::channel(durable_capacity.max(1));
        Self {
            durable_tx,
            durable_rx: tokio::sync::Mutex::new(durable_rx),
            stream_capacity: stream_capacity.max(1),
            streams: Mutex::new(std::collections::HashMap::new()),
            notify: tokio::sync::Notify::new(),
            closed: std::sync::atomic::AtomicBool::new(false),
            #[cfg(test)]
            close_gap_hook: Mutex::new(None),
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
        self.durable_tx
            .try_send(message)
            .map_err(|_| IpcError::Unavailable)?;
        self.notify.notify_one();
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

    fn try_dequeue(
        durable: &mut mpsc::Receiver<UnsolicitedServerMessage>,
        streams: &Mutex<std::collections::HashMap<crate::protocol::StreamKey, StreamFrame>>,
    ) -> Option<UnsolicitedServerMessage> {
        if let Ok(message) = durable.try_recv() {
            return Some(message);
        }
        let mut streams = streams.lock().expect("stream inbox");
        let key = streams.keys().next().copied();
        key.and_then(|key| streams.remove(&key))
            .map(UnsolicitedServerMessage::Stream)
    }

    async fn recv(&self) -> Result<UnsolicitedServerMessage, IpcError> {
        // Serialize durable-receiver ownership across the whole recv loop so a
        // concurrent caller cannot miss a queued durable via try_lock and then
        // sleep on a consumed Notify permit.
        let mut durable = self.durable_rx.lock().await;
        loop {
            if let Some(message) = Self::try_dequeue(&mut durable, &self.streams) {
                return Ok(message);
            }

            // Register before rechecking closed/queues so close cannot be lost.
            let notified = self.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();

            if let Some(message) = Self::try_dequeue(&mut durable, &self.streams) {
                return Ok(message);
            }

            #[cfg(test)]
            if let Some(hook) = self.close_gap_hook.lock().expect("close gap hook").take() {
                hook();
            }

            if self.closed.load(std::sync::atomic::Ordering::SeqCst) {
                if let Some(message) = Self::try_dequeue(&mut durable, &self.streams) {
                    return Ok(message);
                }
                return Err(IpcError::Unavailable);
            }

            notified.await;
        }
    }
}

enum PendingKind {
    Command(oneshot::Sender<Result<CommandReceipt, IpcError>>),
    Query(oneshot::Sender<Result<QueryReply, IpcError>>),
}

#[derive(Clone, Copy)]
enum PendingKey {
    Command(CommandId),
    Query(RequestId),
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
}

impl ErasedPendingReply {
    fn complete_from_deadline(self) {
        match self {
            Self::Command(pending) => pending.complete_from_deadline(),
            Self::Query(pending) => pending.complete_from_deadline(),
        }
    }

    fn cancel(self) {
        match self {
            Self::Command(pending) => pending.cancel(),
            Self::Query(pending) => pending.cancel(),
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

    /// Receive the next unsolicited message, preferring durable/resync over streams.
    pub async fn recv_unsolicited(&self) -> Result<UnsolicitedServerMessage, IpcError> {
        self.shared.unsolicited.recv().await
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
    drop(guard);
    for (_, pending) in commands {
        pending.complete(Err(IpcError::Unavailable));
    }
    for (_, pending) in queries {
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
}
