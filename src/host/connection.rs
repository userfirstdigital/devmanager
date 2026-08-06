//! Single host-owned CommandBus executor boundary.
//!
//! Transport connection tasks never mutate the bus or projections directly.
//! They submit decoded requests through [`HostRequestHandle`]; one
//! [`HostRequestExecutor`] task exclusively owns [`CommandBus`] and services
//! them in arrival order. This same boundary is the later owner for pinned
//! SnapshotSession state and ordered fan-out.

use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

use crate::domain::ClientId;
use crate::kernel::{CommandBus, StoreError};
use crate::protocol::{ClientRequest, ServerResponse};

use super::ipc::IpcError;

/// Fixed capacity for the host request queue.
///
/// When the queue is full, [`HostRequestHandle::execute`] awaits send capacity
/// (bounded backpressure). Requests are never silently dropped.
pub const HOST_REQUEST_QUEUE_CAPACITY: usize = 32;

struct HostRequestJob {
    authenticated_client_id: ClientId,
    request: ClientRequest,
    reply: oneshot::Sender<Result<ServerResponse, IpcError>>,
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
        authenticated_client_id: ClientId,
        request: ClientRequest,
    ) -> Result<ServerResponse, IpcError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx
            .send(HostRequestJob {
                authenticated_client_id,
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
        let mut executor = Self { bus, rx };
        let join = tokio::spawn(async move {
            executor.run().await;
        });
        (handle, join)
    }

    async fn run(&mut self) {
        while let Some(job) = self.rx.recv().await {
            let result = dispatch_authenticated_request(
                job.authenticated_client_id,
                &mut self.bus,
                job.request,
            );
            // If the connection task went away, drop the reply; do not panic.
            let _ = job.reply.send(result);
        }
    }
}

fn map_store_error(error: StoreError) -> IpcError {
    match error {
        StoreError::Busy => IpcError::Busy,
        _ => IpcError::Unavailable,
    }
}

/// Authenticated client_id check plus CommandBus execute/query dispatch.
///
/// Used by the executor boundary and by the exclusive [`super::ipc::HostConnection::serve_request`]
/// compatibility path.
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
            let reply = bus.query(envelope).map_err(map_store_error)?;
            Ok(ServerResponse::QueryReply(reply))
        }
    }
}
