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
    ServerMessage, MAX_PHYSICAL_FRAME_BYTES, MAX_REASSEMBLED_MESSAGE_BYTES,
};

const WRITE_QUEUE_CAPACITY: usize = 32;
const UNSOLICITED_QUEUE_CAPACITY: usize = 64;

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
    unsolicited_rx: tokio::sync::Mutex<mpsc::Receiver<UnsolicitedServerMessage>>,
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

    /// Receive the next unsolicited durable/resync message.
    pub async fn recv_unsolicited(&self) -> Result<UnsolicitedServerMessage, IpcError> {
        let mut rx = self.shared.unsolicited_rx.lock().await;
        rx.recv().await.ok_or(IpcError::Unavailable)
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
    let (unsolicited_tx, unsolicited_rx) =
        mpsc::channel::<UnsolicitedServerMessage>(UNSOLICITED_QUEUE_CAPACITY);

    let state = Arc::new(Mutex::new(SharedState::default()));
    let reader_state = Arc::clone(&state);
    let terminal_state = Arc::clone(&state);
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
                dispatch_server_message(&reader_state, &unsolicited_tx, server_message).await?;
            }
        };

        let _ = supervise_duplex_halves(reader, writer).await;
        poison_mutex(&terminal_state);
    });

    ClientConnection {
        shared: Arc::new(SharedConnection {
            client_id,
            server_hello,
            state,
            write_tx,
            unsolicited_rx: tokio::sync::Mutex::new(unsolicited_rx),
            io_task: Mutex::new(Some(io_task)),
        }),
    }
}

async fn dispatch_server_message(
    state: &Arc<Mutex<SharedState>>,
    unsolicited_tx: &mpsc::Sender<UnsolicitedServerMessage>,
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
        } => unsolicited_tx
            .try_send(UnsolicitedServerMessage::DurableEvent {
                subscription_id,
                event,
            })
            .map_err(|_| IpcError::Unavailable),
        ServerMessage::ResyncRequired {
            subscription_id,
            last_delivered_sequence,
            newest_sequence,
        } => unsolicited_tx
            .try_send(UnsolicitedServerMessage::ResyncRequired {
                subscription_id,
                last_delivered_sequence,
                newest_sequence,
            })
            .map_err(|_| IpcError::Unavailable),
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

    use super::{PendingKey, PendingRegistration, PendingReply, SharedState};
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
}
