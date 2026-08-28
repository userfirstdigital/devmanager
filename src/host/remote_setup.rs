//! Host-owned LOCAL Remote Access setup authority.
//!
//! Bounded control mailbox + status board for native Settings. Listener
//! start/stop never runs on [`crate::host::HostRequestExecutor`]. Identity is
//! established only through the canonical Connect store + host vault; this
//! module does not invent a second identity journal or mint Noise keys.
//!
//! Root glue (not edited here):
//! - `mod remote_setup;` + re-exports in `host/mod.rs`
//! - `crate::remote::update_web_listener_config`
//! - `crate::remote::web::tls::{prepare_web_tls, WebTlsConfig, …}` crate-visible
//! - `WebConfig.tls: Option<WebTlsConfig>`
//! - `HostRemoteAccessController` available on `crate::host`

use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::Read;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, SyncSender, TrySendError};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[cfg(test)]
use crate::connect::InMemoryIdentityPersistence;
use crate::connect::{
    derive_machine_binding, ConnectIdentityLiveState, CredentialVault, HostPublicId,
    IdentityCommand, IdentityError, IdentityOp, IdentityPersistence, IsolatedRemoteStore,
    KernelIdentityPersistence, MachineBinding, OsConnectHostVault,
};
use crate::domain::id::CommandId;
use crate::remote::web::tls::{
    prepare_web_tls, WebTlsConfig, MAX_ADVERTISED_ORIGIN_BYTES, MAX_CERTIFICATE_PEM_BYTES,
    MAX_PRIVATE_KEY_PEM_BYTES,
};
use crate::remote::{load_remote_machine_state, RemoteHostConfig};

const CONTROL_MAILBOX_CAPACITY: usize = 8;
const CONTROL_RECV_POLL: Duration = Duration::from_millis(50);
const SHUTDOWN_JOIN_POLL: Duration = Duration::from_millis(20);
const MAX_BIND_ADDRESS_BYTES: usize = 64;
const MAX_PATH_BYTES: usize = 4 * 1024;
const MAX_ERROR_MESSAGE_CHARS: usize = 240;
const TEST_WAIT_BUDGET: Duration = Duration::from_secs(2);

/// Process-wide fence for the singular production `RemoteSetupRuntime`.
///
/// Retained until the owning runtime drops after its worker exits. Test
/// `start_with_parts` paths do not claim this guard.
struct ProductionRemoteSetupOwnerGuard;

fn production_remote_setup_owned() -> &'static Mutex<bool> {
    static OWNED: OnceLock<Mutex<bool>> = OnceLock::new();
    OWNED.get_or_init(|| Mutex::new(false))
}

fn try_claim_production_remote_setup_owner() -> Result<ProductionRemoteSetupOwnerGuard, String> {
    let mut guard = production_remote_setup_owned()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if *guard {
        return Err(
            "production RemoteSetupRuntime already owned; duplicate controller refused".to_string(),
        );
    }
    *guard = true;
    Ok(ProductionRemoteSetupOwnerGuard)
}

impl Drop for ProductionRemoteSetupOwnerGuard {
    fn drop(&mut self) {
        let mut guard = production_remote_setup_owned()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *guard = false;
    }
}

/// Editable listener bind options from native Settings (never private keys).
#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RemoteListenOptions {
    pub bind_address: String,
    pub port: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub advertised_origin: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub certificate_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub private_key_path: Option<String>,
}

impl fmt::Debug for RemoteListenOptions {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RemoteListenOptions")
            .field("bind_address", &self.bind_address)
            .field("port", &self.port)
            .field("advertised_origin", &self.advertised_origin)
            .field(
                "certificate_path",
                &self.certificate_path.as_ref().map(|_| "<path>"),
            )
            .field(
                "private_key_path",
                &self.private_key_path.as_ref().map(|_| "<redacted-path>"),
            )
            .finish()
    }
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "camelCase", deny_unknown_fields)]
pub enum RemoteSetupRequest {
    Snapshot,
    Enable {
        command_id: CommandId,
        options: RemoteListenOptions,
    },
    Disable {
        command_id: CommandId,
    },
    /// Retry the exact retained failed Enable command (same IdentityCommand).
    Retry {
        command_id: CommandId,
    },
    PairingInfo,
}

impl fmt::Debug for RemoteSetupRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Snapshot => formatter.write_str("RemoteSetupRequest::Snapshot"),
            Self::Enable {
                command_id,
                options,
            } => formatter
                .debug_struct("RemoteSetupRequest::Enable")
                .field("command_id", command_id)
                .field("options", options)
                .finish(),
            Self::Disable { command_id } => formatter
                .debug_struct("RemoteSetupRequest::Disable")
                .field("command_id", command_id)
                .finish(),
            Self::Retry { command_id } => formatter
                .debug_struct("RemoteSetupRequest::Retry")
                .field("command_id", command_id)
                .finish(),
            Self::PairingInfo => formatter.write_str("RemoteSetupRequest::PairingInfo"),
        }
    }
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "camelCase", deny_unknown_fields)]
pub enum RemoteSetupReply {
    Accepted { command_id: CommandId },
    Busy,
    Snapshot { status: RemoteSetupStatus },
    PairingInfo { code: String, url: String },
    Error { message: String },
}

impl fmt::Debug for RemoteSetupReply {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Accepted { command_id } => formatter
                .debug_struct("RemoteSetupReply::Accepted")
                .field("command_id", command_id)
                .finish(),
            Self::Busy => formatter.write_str("RemoteSetupReply::Busy"),
            Self::Snapshot { status } => formatter
                .debug_struct("RemoteSetupReply::Snapshot")
                .field("status", status)
                .finish(),
            Self::PairingInfo { url, .. } => formatter
                .debug_struct("RemoteSetupReply::PairingInfo")
                .field("code", &"<redacted>")
                .field("url", url)
                .finish(),
            Self::Error { message } => formatter
                .debug_struct("RemoteSetupReply::Error")
                .field("message", message)
                .finish(),
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum RemoteSetupState {
    Initializing,
    Disabled,
    Starting,
    Listening,
    Stopping,
    Failed,
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RemoteListenerSummary {
    pub bind_address: String,
    pub port: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub advertised_origin: Option<String>,
    pub tls_configured: bool,
}

impl fmt::Debug for RemoteListenerSummary {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RemoteListenerSummary")
            .field("bind_address", &self.bind_address)
            .field("port", &self.port)
            .field("advertised_origin", &self.advertised_origin)
            .field("tls_configured", &self.tls_configured)
            .finish()
    }
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RemoteSetupStatus {
    pub state: RemoteSetupState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_public_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_command_id: Option<CommandId>,
    /// Exact failed Enable command id retained for Retry (process-lifetime board).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_command_id: Option<CommandId>,
    /// Non-secret listener summary for Settings UI (no PEM/paths).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub listener: Option<RemoteListenerSummary>,
}

impl fmt::Debug for RemoteSetupStatus {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RemoteSetupStatus")
            .field("state", &self.state)
            .field("origin", &self.origin)
            .field("host_public_id", &self.host_public_id)
            .field("error", &self.error)
            .field("last_command_id", &self.last_command_id)
            .field("retry_command_id", &self.retry_command_id)
            .field("listener", &self.listener)
            .finish()
    }
}

impl Default for RemoteSetupStatus {
    fn default() -> Self {
        Self {
            state: RemoteSetupState::Initializing,
            origin: None,
            host_public_id: None,
            error: None,
            last_command_id: None,
            retry_command_id: None,
            listener: None,
        }
    }
}

#[derive(Clone)]
struct CachedPairing {
    code: String,
    url: String,
}

/// Cloneable handle: Snapshot/PairingInfo are synchronous board reads.
#[derive(Clone)]
pub struct RemoteSetupHandle {
    tx: SyncSender<ControlMessage>,
    board: Arc<Mutex<SetupBoard>>,
    cancel: Arc<AtomicBool>,
}

impl fmt::Debug for RemoteSetupHandle {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("RemoteSetupHandle")
    }
}

impl RemoteSetupHandle {
    /// Mutations admit without awaiting listener work. Snapshot/PairingInfo
    /// read the shared board/cache only — no mailbox wait or IO.
    pub fn request(&self, request: RemoteSetupRequest) -> RemoteSetupReply {
        if self.cancel.load(Ordering::Acquire) {
            return RemoteSetupReply::Error {
                message: "remote setup is shutting down".to_string(),
            };
        }
        match request {
            RemoteSetupRequest::Snapshot => {
                let board = self.board.lock().unwrap_or_else(|p| p.into_inner());
                RemoteSetupReply::Snapshot {
                    status: board_public_status(&board),
                }
            }
            RemoteSetupRequest::PairingInfo => {
                let board = self.board.lock().unwrap_or_else(|p| p.into_inner());
                match &board.cached_pairing {
                    Some(pairing)
                        if matches!(board.status.state, RemoteSetupState::Listening)
                            && board.status.host_public_id.is_some() =>
                    {
                        RemoteSetupReply::PairingInfo {
                            code: pairing.code.clone(),
                            url: pairing.url.clone(),
                        }
                    }
                    _ => RemoteSetupReply::Error {
                        message:
                            "pairing info unavailable: remote access is not verified listening"
                                .to_string(),
                    },
                }
            }
            RemoteSetupRequest::Enable { .. }
            | RemoteSetupRequest::Disable { .. }
            | RemoteSetupRequest::Retry { .. } => {
                if let Err(message) = validate_request_bounds(&request) {
                    return RemoteSetupReply::Error { message };
                }
                let command_id = match &request {
                    RemoteSetupRequest::Enable { command_id, .. }
                    | RemoteSetupRequest::Disable { command_id }
                    | RemoteSetupRequest::Retry { command_id } => *command_id,
                    RemoteSetupRequest::Snapshot | RemoteSetupRequest::PairingInfo => {
                        unreachable!("mutation arm")
                    }
                };
                // Hold the board lock across inflight check, try_send, and publish.
                let mut board = self.board.lock().unwrap_or_else(|p| p.into_inner());
                if board.mutation_inflight || self.cancel.load(Ordering::Acquire) {
                    return RemoteSetupReply::Busy;
                }
                if let RemoteSetupRequest::Enable { command_id, .. } = &request {
                    if let Some(retained) = &board.retained_enable {
                        if retained.command.command_id != *command_id
                            && board.identity_transition_pending
                        {
                            return RemoteSetupReply::Error {
                                message: "identity transition pending; use Retry with the retained command_id"
                                    .to_string(),
                            };
                        }
                    }
                }
                if let RemoteSetupRequest::Retry { command_id } = &request {
                    match &board.retained_enable {
                        Some(retained) if retained.command.command_id == *command_id => {}
                        Some(_) => {
                            return RemoteSetupReply::Error {
                                message: "retry command_id does not match retained enable"
                                    .to_string(),
                            };
                        }
                        None => {
                            return RemoteSetupReply::Error {
                                message: "no retained enable command available for retry"
                                    .to_string(),
                            };
                        }
                    }
                }
                match self.tx.try_send(ControlMessage::Mutate { request }) {
                    Ok(()) => {
                        board.mutation_inflight = true;
                        board.status.last_command_id = Some(command_id);
                        RemoteSetupReply::Accepted { command_id }
                    }
                    Err(TrySendError::Full(_)) => RemoteSetupReply::Busy,
                    Err(TrySendError::Disconnected(_)) => RemoteSetupReply::Error {
                        message: "remote setup control thread stopped".to_string(),
                    },
                }
            }
        }
    }

    pub fn status(&self) -> RemoteSetupStatus {
        let board = self.board.lock().unwrap_or_else(|p| p.into_inner());
        board_public_status(&board)
    }
}

/// RAII owner of the dedicated Remote Access setup control thread.
pub struct RemoteSetupRuntime<'host> {
    /// Production recovery cannot outlive the exact profile's OS-backed lock.
    _host_lock: Option<&'host crate::host::HostLock>,
    /// Retained through `Drop::drop`, which joins the worker before fields drop.
    _production_owner: Option<ProductionRemoteSetupOwnerGuard>,
    cancel: Arc<AtomicBool>,
    wake_tx: Option<SyncSender<ControlMessage>>,
    join: Option<JoinHandle<Result<(), String>>>,
    board: Arc<Mutex<SetupBoard>>,
}

impl fmt::Debug for RemoteSetupRuntime<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("RemoteSetupRuntime")
    }
}

impl<'host> RemoteSetupRuntime<'host> {
    /// Start the control thread. Returns promptly after arming cleanup.
    ///
    /// Production starts claim a process-wide owner guard retained until this
    /// runtime (and its worker) exit, fencing duplicate production controllers.
    pub fn start(
        host_lock: &'host crate::host::HostLock,
    ) -> Result<(RemoteSetupHandle, Self), String> {
        let root = crate::persistence::app_config_dir()
            .and_then(|root| {
                root.canonicalize()
                    .map_err(|source| crate::persistence::PersistenceError::Io {
                        path: root,
                        source,
                    })
            })
            .map_err(|_| "remote setup profile ownership unavailable".to_string())?;
        if host_lock.profile_root() != root
            || host_lock.identity().profile != resolve_host_profile_name()
        {
            return Err("remote setup requires the active profile's host lock".to_string());
        }
        let owner = try_claim_production_remote_setup_owner()?;
        match Self::start_with_parts(
            Box::new(ProductionListenerFactory),
            Box::new(ProductionBootstrap::new()),
        ) {
            Ok((handle, mut runtime)) => {
                runtime._host_lock = Some(host_lock);
                runtime._production_owner = Some(owner);
                Ok((handle, runtime))
            }
            Err(error) => {
                drop(owner);
                Err(error)
            }
        }
    }

    fn start_with_parts(
        listener_factory: Box<dyn ListenerFactory>,
        bootstrap: Box<dyn SetupBootstrap>,
    ) -> Result<(RemoteSetupHandle, Self), String> {
        let (tx, rx) = mpsc::sync_channel::<ControlMessage>(CONTROL_MAILBOX_CAPACITY);
        let cancel = Arc::new(AtomicBool::new(false));
        let board = Arc::new(Mutex::new(SetupBoard::default()));
        let board_worker = Arc::clone(&board);
        let cancel_worker = Arc::clone(&cancel);
        let join = thread::Builder::new()
            .name("devmanager-remote-setup".to_string())
            .spawn(move || {
                control_thread_main(rx, board_worker, cancel_worker, listener_factory, bootstrap)
            })
            .map_err(|error| format!("remote setup worker spawn failed: {error}"))?;

        let handle = RemoteSetupHandle {
            tx: tx.clone(),
            board: Arc::clone(&board),
            cancel: Arc::clone(&cancel),
        };
        Ok((
            handle,
            Self {
                _host_lock: None,
                _production_owner: None,
                cancel,
                wake_tx: Some(tx),
                join: Some(join),
                board,
            },
        ))
    }

    pub fn handle(&self) -> RemoteSetupHandle {
        RemoteSetupHandle {
            tx: self
                .wake_tx
                .as_ref()
                .expect("remote setup runtime alive")
                .clone(),
            board: Arc::clone(&self.board),
            cancel: Arc::clone(&self.cancel),
        }
    }

    /// Signal cancel (nonblocking wake) and join the exact worker.
    pub fn shutdown(mut self) -> Result<(), String> {
        self.signal_shutdown();
        self.join_blocking()
    }

    /// Host-executor-safe async shutdown: timer poll, then join exact thread.
    pub async fn shutdown_async(mut self) -> Result<(), String> {
        self.signal_shutdown();
        while !self.is_finished() {
            tokio::time::sleep(SHUTDOWN_JOIN_POLL).await;
        }
        self.join_blocking()
    }

    pub fn is_finished(&self) -> bool {
        self.join
            .as_ref()
            .map(|join| join.is_finished())
            .unwrap_or(true)
    }

    fn signal_shutdown(&mut self) {
        self.cancel.store(true, Ordering::SeqCst);
        if let Some(tx) = self.wake_tx.take() {
            // Nonblocking: never queue behind a long Enable.
            let _ = tx.try_send(ControlMessage::Shutdown);
        }
        clear_listening_and_pairing(&self.board);
    }

    fn join_blocking(&mut self) -> Result<(), String> {
        let Some(join) = self.join.take() else {
            return Ok(());
        };
        while !join.is_finished() {
            thread::sleep(SHUTDOWN_JOIN_POLL);
        }
        match join.join() {
            Ok(Ok(())) => {
                clear_listening_and_pairing(&self.board);
                Ok(())
            }
            Ok(Err(error)) => {
                clear_listening_and_pairing(&self.board);
                Err(error)
            }
            Err(_) => {
                clear_listening_and_pairing(&self.board);
                Err("remote setup worker panicked".to_string())
            }
        }
    }
}

impl Drop for RemoteSetupRuntime<'_> {
    fn drop(&mut self) {
        self.signal_shutdown();
        let _ = self.join_blocking();
    }
}

#[derive(Default)]
struct SetupBoard {
    status: RemoteSetupStatus,
    mutation_inflight: bool,
    /// Exact Enable IdentityCommand retained for Retry (including after Disable
    /// when custody/bind previously failed — never auto-executed).
    retained_enable: Option<RetainedEnable>,
    /// True while a retained Enable may still own a store pending transition.
    identity_transition_pending: bool,
    cached_pairing: Option<CachedPairing>,
    last_outcome: Option<RemoteSetupReply>,
}

#[derive(Clone)]
struct RetainedEnable {
    command: IdentityCommand,
    options: RemoteListenOptions,
}

enum ControlMessage {
    Mutate { request: RemoteSetupRequest },
    Shutdown,
}

trait ListenerControl: Send {
    fn stop_join(&mut self) -> Result<(), String>;
    fn start_from_config(&mut self, config: RemoteHostConfig) -> Result<(), String>;
    fn is_active(&self) -> bool;
}

/// Builds the listener controller on the control thread (Tokio runtime owned there).
trait ListenerFactory: Send {
    fn build(self: Box<Self>) -> Result<Box<dyn ListenerControl>, String>;
}

trait SetupBootstrap: Send {
    fn load_host_config(&self) -> Result<RemoteHostConfig, String>;
    fn load_verified_host_id(&self) -> Result<Option<String>, String>;
    fn identity_is_pending(&self) -> bool;
    /// RegisterDevice-only orphan recovery before listener admission.
    ///
    /// Queued cancellation refuses new recovery mutation admission. Already
    /// admitted recovery IO remains owned until settled. No-op when there is
    /// no pending transition. Enable/Repair/Rotate/revocation fail closed.
    fn recover_orphaned_register_device_pending(&self, cancel: &AtomicBool) -> Result<(), String>;
    fn build_enable_command(&self, command_id: CommandId) -> Result<IdentityCommand, String>;
    fn ensure_identity_committed(&self, command: &IdentityCommand) -> Result<String, String>;
    fn persist_enabled(
        &self,
        options: &RemoteListenOptions,
        tls: Option<&WebTlsConfig>,
    ) -> Result<RemoteHostConfig, String>;
    fn persist_disabled(&self) -> Result<RemoteHostConfig, String>;
    fn pairing_from_config(&self, config: &RemoteHostConfig) -> Option<CachedPairing>;
}

struct ProductionListenerFactory;

impl ListenerFactory for ProductionListenerFactory {
    fn build(self: Box<Self>) -> Result<Box<dyn ListenerControl>, String> {
        Ok(Box::new(ProductionListenerControl::new_on_control_thread()?))
    }
}

struct ProductionListenerControl {
    runtime: tokio::runtime::Runtime,
    controller: Option<crate::host::HostRemoteAccessController>,
}

impl ProductionListenerControl {
    fn new_on_control_thread() -> Result<Self, String> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|error| format!("remote setup tokio runtime: {error}"))?;
        Ok(Self {
            runtime,
            controller: None,
        })
    }
}

impl Drop for ProductionListenerControl {
    fn drop(&mut self) {
        let _ = self.stop_join();
    }
}

impl ListenerControl for ProductionListenerControl {
    fn stop_join(&mut self) -> Result<(), String> {
        if let Some(controller) = self.controller.take() {
            self.runtime
                .block_on(controller.shutdown())
                .map_err(|error| error.to_string())?;
        }
        Ok(())
    }

    fn start_from_config(&mut self, config: RemoteHostConfig) -> Result<(), String> {
        self.stop_join()?;
        let controller = self
            .runtime
            .block_on(crate::host::HostRemoteAccessController::start_from_config(
                config,
            ))
            .map_err(|error| error.to_string())?;
        self.controller = Some(controller);
        Ok(())
    }

    fn is_active(&self) -> bool {
        self.controller
            .as_ref()
            .map(|controller| controller.is_active())
            .unwrap_or(false)
    }
}

/// Control-thread identity session: store/vault/binding opened once on explicit
/// Enable and reused for Retry so `claimed_owner` lease survives failures.
struct HostIdentityContext<P: IdentityPersistence + 'static, V: CredentialVault> {
    store: IsolatedRemoteStore<P>,
    vault: V,
    binding: MachineBinding,
}

impl HostIdentityContext<KernelIdentityPersistence, OsConnectHostVault> {
    fn open_production() -> Result<Self, String> {
        let profile = resolve_host_profile_name();
        let binding = derive_machine_binding(&profile)
            .map_err(|error| format!("machine binding unavailable: {error}"))?;
        let vault_root = crate::persistence::app_config_dir()
            .map_err(|error| format!("app config dir unavailable: {error}"))?
            .join("connect-host-vault");
        let vault = OsConnectHostVault::open(vault_root, binding.clone())
            .map_err(|error| format!("host vault unavailable: {error}"))?;
        let store = IsolatedRemoteStore::<KernelIdentityPersistence>::open_active_profile()
            .map_err(|error| format!("identity store unavailable: {error}"))?;
        Ok(Self {
            store,
            vault,
            binding,
        })
    }

    fn ensure_committed_os(&mut self, command: &IdentityCommand) -> Result<String, String> {
        ensure_host_identity_committed_os(&mut self.store, &mut self.vault, &self.binding, command)
    }
}

impl<P, V> HostIdentityContext<P, V>
where
    P: IdentityPersistence + 'static,
    V: CredentialVault,
{
    fn build_enable_command(&mut self, command_id: CommandId) -> Result<IdentityCommand, String> {
        build_enable_identity_command_with(&mut self.store, &self.vault, &self.binding, command_id)
    }

    #[cfg(test)]
    fn ensure_committed(&mut self, command: &IdentityCommand) -> Result<String, String> {
        ensure_host_identity_with_store(&mut self.store, &mut self.vault, &self.binding, command)
    }

    fn identity_is_pending(&mut self) -> bool {
        matches!(
            self.store.identity_live_state(),
            Ok(ConnectIdentityLiveState::Pending)
        )
    }

    fn recover_orphaned_register_device_pending(
        &mut self,
        cancel: &AtomicBool,
    ) -> Result<(), String> {
        if cancel.load(Ordering::Acquire) {
            return Ok(());
        }
        // Admitted: settle even if cancel flips during vault/CAS IO.
        match self
            .store
            .recover_orphaned_register_device_pending(&self.binding, &mut self.vault)
        {
            Ok(_) => Ok(()),
            Err(error) => Err(format!("orphaned RegisterDevice recovery failed: {error}")),
        }
    }
}

struct ProductionBootstrap {
    identity: Mutex<Option<HostIdentityContext<KernelIdentityPersistence, OsConnectHostVault>>>,
}

impl ProductionBootstrap {
    fn new() -> Self {
        Self {
            identity: Mutex::new(None),
        }
    }

    fn with_identity_mut<R>(
        &self,
        f: impl FnOnce(
            &mut HostIdentityContext<KernelIdentityPersistence, OsConnectHostVault>,
        ) -> Result<R, String>,
    ) -> Result<R, String> {
        let mut guard = self.identity.lock().unwrap_or_else(|p| p.into_inner());
        if guard.is_none() {
            *guard = Some(HostIdentityContext::open_production()?);
        }
        f(guard.as_mut().expect("identity context initialized"))
    }
}

impl SetupBootstrap for ProductionBootstrap {
    fn load_host_config(&self) -> Result<RemoteHostConfig, String> {
        load_remote_machine_state()
            .map(|state| state.host)
            .map_err(|error| format!("remote config unavailable: {error}"))
    }

    fn load_verified_host_id(&self) -> Result<Option<String>, String> {
        // Startup/snapshot: load-only; do not open the mutable Enable session.
        load_only_host_public_id()
    }

    fn identity_is_pending(&self) -> bool {
        let mut guard = self.identity.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(ctx) = guard.as_mut() {
            return ctx.identity_is_pending();
        }
        IsolatedRemoteStore::<KernelIdentityPersistence>::open_active_profile()
            .ok()
            .and_then(|store| store.identity_live_state().ok())
            .is_some_and(|state| matches!(state, ConnectIdentityLiveState::Pending))
    }

    fn recover_orphaned_register_device_pending(&self, cancel: &AtomicBool) -> Result<(), String> {
        if cancel.load(Ordering::Acquire) {
            return Ok(());
        }
        self.with_identity_mut(|ctx| ctx.recover_orphaned_register_device_pending(cancel))
    }

    fn build_enable_command(&self, command_id: CommandId) -> Result<IdentityCommand, String> {
        // Lazy-init on explicit Enable only.
        self.with_identity_mut(|ctx| ctx.build_enable_command(command_id))
    }

    fn ensure_identity_committed(&self, command: &IdentityCommand) -> Result<String, String> {
        self.with_identity_mut(|ctx| ctx.ensure_committed_os(command))
    }

    fn persist_enabled(
        &self,
        options: &RemoteListenOptions,
        tls: Option<&WebTlsConfig>,
    ) -> Result<RemoteHostConfig, String> {
        persist_enabled_listener(options, tls)
    }

    fn persist_disabled(&self) -> Result<RemoteHostConfig, String> {
        // Do not drop the identity session — Retry must keep claimed_owner.
        crate::remote::update_web_listener_config(|web| {
            web.enabled = false;
        })
        .map_err(|error| format!("failed to persist disabled remote access: {error}"))
    }

    fn pairing_from_config(&self, config: &RemoteHostConfig) -> Option<CachedPairing> {
        if !config.web.enabled || config.web.pairing_token.is_empty() {
            return None;
        }
        Some(CachedPairing {
            code: config.web.pairing_token.clone(),
            url: config.web.display_url(),
        })
    }
}

fn control_thread_main(
    rx: mpsc::Receiver<ControlMessage>,
    board: Arc<Mutex<SetupBoard>>,
    cancel: Arc<AtomicBool>,
    listener_factory: Box<dyn ListenerFactory>,
    bootstrap: Box<dyn SetupBootstrap>,
) -> Result<(), String> {
    if cancel.load(Ordering::Acquire) {
        clear_listening_and_pairing(&board);
        set_status(
            &board,
            RemoteSetupState::Disabled,
            None,
            None,
            None,
            ClearPairing::Yes,
        );
        return Ok(());
    }

    let mut listener = match listener_factory.build() {
        Ok(listener) => listener,
        Err(error) => {
            clear_listening_and_pairing(&board);
            set_status(
                &board,
                RemoteSetupState::Failed,
                None,
                None,
                Some(redact_error(&error)),
                ClearPairing::Yes,
            );
            return Err(error);
        }
    };

    if cancel.load(Ordering::Acquire) {
        let stop_error = listener.stop_join().err();
        clear_listening_and_pairing(&board);
        set_status(
            &board,
            RemoteSetupState::Disabled,
            None,
            None,
            stop_error.as_ref().map(|error| redact_error(error)),
            ClearPairing::Yes,
        );
        return match stop_error {
            Some(error) => Err(error),
            None => Ok(()),
        };
    }

    startup_bootstrap(&board, &mut *listener, &*bootstrap, &cancel);

    let mut stop_error = None;
    loop {
        if cancel.load(Ordering::Acquire) {
            break;
        }
        match rx.recv_timeout(CONTROL_RECV_POLL) {
            Ok(ControlMessage::Shutdown) | Err(RecvTimeoutError::Disconnected) => break,
            Err(RecvTimeoutError::Timeout) => continue,
            Ok(ControlMessage::Mutate { request }) => {
                if cancel.load(Ordering::Acquire) {
                    if let Ok(mut guard) = board.lock() {
                        guard.mutation_inflight = false;
                    }
                    break;
                }
                handle_mutation(&board, &mut *listener, &*bootstrap, &cancel, request);
                if let Ok(mut guard) = board.lock() {
                    guard.mutation_inflight = false;
                }
                if cancel.load(Ordering::Acquire) {
                    break;
                }
            }
        }
    }

    clear_listening_and_pairing(&board);
    set_status(
        &board,
        RemoteSetupState::Stopping,
        None,
        None,
        None,
        ClearPairing::Yes,
    );
    if let Err(error) = listener.stop_join() {
        stop_error = Some(error);
    }
    clear_listening_and_pairing(&board);
    set_status(
        &board,
        RemoteSetupState::Disabled,
        None,
        None,
        stop_error.as_ref().map(|error| redact_error(error)),
        ClearPairing::Yes,
    );
    match stop_error {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

fn startup_bootstrap(
    board: &Arc<Mutex<SetupBoard>>,
    listener: &mut dyn ListenerControl,
    bootstrap: &dyn SetupBootstrap,
    cancel: &AtomicBool,
) {
    if cancel.load(Ordering::Acquire) {
        clear_listening_and_pairing(board);
        set_status(
            board,
            RemoteSetupState::Disabled,
            None,
            None,
            None,
            ClearPairing::Yes,
        );
        return;
    }
    let mut status = RemoteSetupStatus {
        state: RemoteSetupState::Initializing,
        ..RemoteSetupStatus::default()
    };
    match bootstrap.load_host_config() {
        Ok(config) => {
            // RegisterDevice-only orphan recovery before load-only host verify
            // or any listener admission. Disabled remote startup must not mutate.
            if config.web.enabled {
                if cancel.load(Ordering::Acquire) {
                    clear_listening_and_pairing(board);
                    set_status(
                        board,
                        RemoteSetupState::Disabled,
                        None,
                        None,
                        None,
                        ClearPairing::Yes,
                    );
                    return;
                }
                if let Err(error) = bootstrap.recover_orphaned_register_device_pending(cancel) {
                    status.state = RemoteSetupState::Failed;
                    status.error = Some(redact_error(&error));
                    status.listener = Some(listener_summary_from_config(&config));
                    publish(board, status, None, ClearPairing::Yes);
                    return;
                }
            }
            match bootstrap.load_verified_host_id() {
                Ok(host_id) => {
                    status.host_public_id = host_id.clone();
                    status.listener = Some(listener_summary_from_config(&config));
                    if config.web.enabled {
                        match host_id {
                            Some(host_id) => {
                                if cancel.load(Ordering::Acquire) {
                                    clear_listening_and_pairing(board);
                                    set_status(
                                        board,
                                        RemoteSetupState::Disabled,
                                        None,
                                        None,
                                        None,
                                        ClearPairing::Yes,
                                    );
                                    return;
                                }
                                status.state = RemoteSetupState::Starting;
                                publish(board, status.clone(), None, ClearPairing::Yes);
                                match listener.start_from_config(config.clone()) {
                                    Ok(()) if listener.is_active() => {
                                        if cancel.load(Ordering::Acquire) {
                                            let _ = listener.stop_join();
                                            clear_listening_and_pairing(board);
                                            set_status(
                                                board,
                                                RemoteSetupState::Disabled,
                                                None,
                                                None,
                                                Some("remote setup cancelled".to_string()),
                                                ClearPairing::Yes,
                                            );
                                            return;
                                        }
                                        status.state = RemoteSetupState::Listening;
                                        status.origin = Some(config.web.display_url());
                                        status.host_public_id = Some(host_id);
                                        status.error = None;
                                        status.listener =
                                            Some(listener_summary_from_config(&config));
                                        let pairing = bootstrap.pairing_from_config(&config);
                                        publish(board, status, None, ClearPairing::No);
                                        if let Some(pairing) = pairing {
                                            set_cached_pairing(board, Some(pairing));
                                        }
                                        return;
                                    }
                                    Ok(()) => {
                                        let _ = listener.stop_join();
                                        status.state = RemoteSetupState::Failed;
                                        status.error = Some(
                                        "remote access enabled but listener did not become active"
                                            .to_string(),
                                    );
                                    }
                                    Err(error) => {
                                        let _ = listener.stop_join();
                                        status.state = RemoteSetupState::Failed;
                                        status.error = Some(redact_error(&error));
                                    }
                                }
                            }
                            None => {
                                status.state = RemoteSetupState::Failed;
                                status.error = Some(
                                    "remote access is enabled but verified host custody is missing"
                                        .to_string(),
                                );
                            }
                        }
                    } else {
                        status.state = RemoteSetupState::Disabled;
                    }
                }
                Err(error) => {
                    status.state = RemoteSetupState::Failed;
                    status.error = Some(redact_error(&error));
                }
            }
        }
        Err(error) => {
            status.state = RemoteSetupState::Failed;
            status.error = Some(redact_error(&error));
        }
    }
    publish(board, status, None, ClearPairing::Yes);
}

fn handle_mutation(
    board: &Arc<Mutex<SetupBoard>>,
    listener: &mut dyn ListenerControl,
    bootstrap: &dyn SetupBootstrap,
    cancel: &AtomicBool,
    request: RemoteSetupRequest,
) {
    if cancel.load(Ordering::Acquire) {
        return;
    }
    let outcome = match request {
        RemoteSetupRequest::Enable {
            command_id,
            options,
        } => enable_remote_access(
            board, listener, bootstrap, cancel, command_id, options, None,
        ),
        RemoteSetupRequest::Retry { command_id } => {
            let retained = board
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .retained_enable
                .clone();
            match retained {
                Some(retained) if retained.command.command_id == command_id => {
                    enable_remote_access(
                        board,
                        listener,
                        bootstrap,
                        cancel,
                        command_id,
                        retained.options,
                        Some(retained.command),
                    )
                }
                Some(_) => {
                    fail_status(
                        board,
                        Some(command_id),
                        "retry command_id does not match retained enable",
                    );
                    RemoteSetupReply::Error {
                        message: "retry command_id does not match retained enable".to_string(),
                    }
                }
                None => {
                    fail_status(
                        board,
                        Some(command_id),
                        "no retained enable command available for retry",
                    );
                    RemoteSetupReply::Error {
                        message: "no retained enable command available for retry".to_string(),
                    }
                }
            }
        }
        RemoteSetupRequest::Disable { command_id } => {
            disable_remote_access(board, listener, bootstrap, cancel, command_id)
        }
        RemoteSetupRequest::Snapshot | RemoteSetupRequest::PairingInfo => RemoteSetupReply::Error {
            message: "mutation path received a query request".to_string(),
        },
    };
    if let Ok(mut guard) = board.lock() {
        guard.last_outcome = Some(outcome);
    }
}

fn enable_remote_access(
    board: &Arc<Mutex<SetupBoard>>,
    listener: &mut dyn ListenerControl,
    bootstrap: &dyn SetupBootstrap,
    cancel: &AtomicBool,
    command_id: CommandId,
    options: RemoteListenOptions,
    retained_command: Option<IdentityCommand>,
) -> RemoteSetupReply {
    set_status(
        board,
        RemoteSetupState::Starting,
        Some(command_id),
        None,
        None,
        ClearPairing::Yes,
    );
    if cancel.load(Ordering::Acquire) {
        fail_status(board, Some(command_id), "remote setup cancelled");
        return RemoteSetupReply::Error {
            message: "remote setup cancelled".to_string(),
        };
    }

    let persisted_tls = bootstrap
        .load_host_config()
        .ok()
        .and_then(|config| config.web.tls.clone());
    let tls = match validate_listen_options(&options, persisted_tls.as_ref()) {
        Ok(tls) => tls,
        Err(message) => {
            fail_status(board, Some(command_id), &message);
            return RemoteSetupReply::Error { message };
        }
    };

    // A disabled startup deliberately does not mutate. The next explicit Enable
    // may recover a RegisterDevice orphan, but only without a listener that
    // could still own a live enrollment. Retained Enable retries stay exact.
    if retained_command.is_none() && !listener.is_active() {
        if let Err(message) = bootstrap.recover_orphaned_register_device_pending(cancel) {
            fail_status(board, Some(command_id), &message);
            return RemoteSetupReply::Error { message };
        }
        if cancel.load(Ordering::Acquire) {
            return RemoteSetupReply::Error {
                message: "remote setup cancelled".to_string(),
            };
        }
    }

    let identity_command = match retained_command {
        Some(command) => command,
        None => match bootstrap.build_enable_command(command_id) {
            Ok(command) => command,
            Err(message) => {
                fail_status(board, Some(command_id), &message);
                return RemoteSetupReply::Error { message };
            }
        },
    };

    {
        let mut guard = board.lock().unwrap_or_else(|p| p.into_inner());
        // Keep prior exact failed command until this Enable retains its own.
        guard.retained_enable = Some(RetainedEnable {
            command: identity_command.clone(),
            options: options.clone(),
        });
    }

    let host_public_id = match bootstrap.ensure_identity_committed(&identity_command) {
        Ok(host_id) => {
            if let Ok(mut guard) = board.lock() {
                guard.identity_transition_pending = false;
            }
            host_id
        }
        Err(message) => {
            if let Ok(mut guard) = board.lock() {
                guard.identity_transition_pending = bootstrap.identity_is_pending();
            }
            fail_status(board, Some(command_id), &message);
            return RemoteSetupReply::Error { message };
        }
    };
    set_host_public_id(board, host_public_id.clone());

    if cancel.load(Ordering::Acquire) {
        fail_status(board, Some(command_id), "remote setup cancelled");
        return RemoteSetupReply::Error {
            message: "remote setup cancelled".to_string(),
        };
    }

    if let Err(error) = listener.stop_join() {
        let message = redact_error(&error);
        fail_status(board, Some(command_id), &message);
        return RemoteSetupReply::Error { message };
    }

    let config = match bootstrap.persist_enabled(&options, tls.as_ref()) {
        Ok(config) => config,
        Err(message) => {
            fail_status(board, Some(command_id), &message);
            return RemoteSetupReply::Error { message };
        }
    };

    if cancel.load(Ordering::Acquire) {
        fail_status(board, Some(command_id), "remote setup cancelled");
        return RemoteSetupReply::Error {
            message: "remote setup cancelled".to_string(),
        };
    }

    match listener.start_from_config(config.clone()) {
        Ok(()) if listener.is_active() => {
            if cancel.load(Ordering::Acquire) {
                let _ = listener.stop_join();
                clear_listening_and_pairing(board);
                fail_status(board, Some(command_id), "remote setup cancelled");
                return RemoteSetupReply::Error {
                    message: "remote setup cancelled".to_string(),
                };
            }
            clear_retained_enable(board);
            if let Ok(mut guard) = board.lock() {
                guard.identity_transition_pending = false;
                guard.status.listener = Some(listener_summary_from_config(&config));
            }
            set_status(
                board,
                RemoteSetupState::Listening,
                Some(command_id),
                Some(config.web.display_url()),
                None,
                ClearPairing::No,
            );
            set_host_public_id(board, host_public_id);
            if let Some(pairing) = bootstrap.pairing_from_config(&config) {
                set_cached_pairing(board, Some(pairing));
            }
            RemoteSetupReply::Accepted { command_id }
        }
        Ok(()) => {
            let _ = listener.stop_join();
            let message = "listener start reported success but is not active".to_string();
            fail_status(board, Some(command_id), &message);
            RemoteSetupReply::Error { message }
        }
        Err(error) => {
            let _ = listener.stop_join();
            let message = redact_error(&error);
            fail_status(board, Some(command_id), &message);
            RemoteSetupReply::Error { message }
        }
    }
}

fn disable_remote_access(
    board: &Arc<Mutex<SetupBoard>>,
    listener: &mut dyn ListenerControl,
    bootstrap: &dyn SetupBootstrap,
    cancel: &AtomicBool,
    command_id: CommandId,
) -> RemoteSetupReply {
    set_status(
        board,
        RemoteSetupState::Stopping,
        Some(command_id),
        None,
        None,
        ClearPairing::Yes,
    );
    if let Err(error) = listener.stop_join() {
        let message = redact_error(&error);
        fail_status(board, Some(command_id), &message);
        return RemoteSetupReply::Error { message };
    }
    if cancel.load(Ordering::Acquire) {
        // Listener already stopped; still persist disabled when possible.
    }
    match bootstrap.persist_disabled() {
        Ok(_) => {
            // Preserve retained_enable so an explicit Retry remains possible
            // after a prior custody/bind failure. Never auto-execute it.
            let host_id = bootstrap.load_verified_host_id().ok().flatten();
            set_status(
                board,
                RemoteSetupState::Disabled,
                Some(command_id),
                None,
                None,
                ClearPairing::Yes,
            );
            if let Some(host_id) = host_id {
                set_host_public_id(board, host_id);
            }
            RemoteSetupReply::Accepted { command_id }
        }
        Err(message) => {
            fail_status(board, Some(command_id), &message);
            RemoteSetupReply::Error { message }
        }
    }
}

fn validate_request_bounds(request: &RemoteSetupRequest) -> Result<(), String> {
    match request {
        RemoteSetupRequest::Enable { options, .. } => validate_options_bounds(options),
        RemoteSetupRequest::Retry { .. } | RemoteSetupRequest::Disable { .. } => Ok(()),
        RemoteSetupRequest::Snapshot | RemoteSetupRequest::PairingInfo => Ok(()),
    }
}

fn validate_options_bounds(options: &RemoteListenOptions) -> Result<(), String> {
    if options.bind_address.len() > MAX_BIND_ADDRESS_BYTES {
        return Err("bind_address exceeds bound".to_string());
    }
    if options.port == 0 {
        return Err("listen port must be non-zero".to_string());
    }
    if let Some(origin) = &options.advertised_origin {
        if origin.len() > MAX_ADVERTISED_ORIGIN_BYTES {
            return Err("advertised_origin exceeds bound".to_string());
        }
    }
    for path in [
        options.certificate_path.as_deref(),
        options.private_key_path.as_deref(),
    ]
    .into_iter()
    .flatten()
    {
        if path.len() > MAX_PATH_BYTES {
            return Err("TLS material path exceeds bound".to_string());
        }
    }
    Ok(())
}

fn validate_listen_options(
    options: &RemoteListenOptions,
    persisted_tls: Option<&WebTlsConfig>,
) -> Result<Option<WebTlsConfig>, String> {
    validate_options_bounds(options)?;
    let address = options
        .bind_address
        .trim()
        .parse::<IpAddr>()
        .map_err(|_| "bind_address must be a literal IP address".to_string())?;

    // Reject public/wildcard/invalid bind before any TLS path IO.
    let loopback = is_loopback_ip(address);
    if !loopback && (is_wildcard_ip(address) || !is_private_lan_ip(address)) {
        return Err(
            "non-loopback bind requires a private LAN IPv4 or ULA IPv6 address (wildcards and public addresses are rejected)"
                .to_string(),
        );
    }

    if let Some(origin) = options.advertised_origin.as_deref() {
        validate_origin_port_matches_listen(origin, options.port)?;
    }

    let tls_fields = (
        options.advertised_origin.as_ref(),
        options.certificate_path.as_ref(),
        options.private_key_path.as_ref(),
    );
    let tls_config = match tls_fields {
        (None, None, None) => None,
        (Some(origin), None, None) => match persisted_tls {
            Some(persisted) if persisted.advertised_origin == *origin => {
                prepare_web_tls(persisted).map_err(|error| redact_error(&error))?;
                Some(persisted.clone())
            }
            Some(_) => {
                return Err(
                    "certificate_path and private_key_path required when replacing TLS material"
                        .to_string(),
                );
            }
            None => {
                return Err(
                    "certificate_path and private_key_path required when no persisted TLS exists"
                        .to_string(),
                );
            }
        },
        (Some(origin), Some(cert), Some(key)) => Some(load_tls_config(origin, cert, key)?),
        (None, Some(_), Some(_)) => {
            return Err("advertised_origin required when supplying TLS material paths".to_string());
        }
        _ => {
            return Err(
                "advertised_origin, certificate_path, and private_key_path must all be set, all omitted, or origin-only to reuse persisted TLS"
                    .to_string(),
            );
        }
    };

    if loopback {
        return Ok(tls_config);
    }
    let tls = tls_config.ok_or_else(|| {
        "non-loopback LAN bind requires HTTPS/WSS TLS material (advertised_origin + certificate + private key, or matching persisted TLS)"
            .to_string()
    })?;
    Ok(Some(tls))
}

fn validate_origin_port_matches_listen(origin: &str, listen_port: u16) -> Result<(), String> {
    let origin_port = parse_advertised_origin_port(origin)?;
    if origin_port != listen_port {
        return Err(format!(
            "advertised_origin port ({origin_port}) must match listen port ({listen_port})"
        ));
    }
    Ok(())
}

fn parse_advertised_origin_port(origin: &str) -> Result<u16, String> {
    let url = url::Url::parse(origin)
        .map_err(|_| "advertised_origin must be an HTTPS origin".to_string())?;
    if url.scheme() != "https"
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.path() != "/"
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(
            "advertised_origin must be an HTTPS origin without credentials, path or query"
                .to_string(),
        );
    }
    url.port_or_known_default()
        .filter(|port| *port != 0)
        .ok_or_else(|| "advertised_origin has invalid port".to_string())
}

fn load_tls_config(
    advertised_origin: &str,
    certificate_path: &str,
    private_key_path: &str,
) -> Result<WebTlsConfig, String> {
    if advertised_origin.len() > MAX_ADVERTISED_ORIGIN_BYTES {
        return Err("advertised_origin exceeds bound".to_string());
    }
    let certificate_pem =
        read_bounded_pem_file(Path::new(certificate_path), MAX_CERTIFICATE_PEM_BYTES)?;
    let private_key_pem =
        read_bounded_pem_file(Path::new(private_key_path), MAX_PRIVATE_KEY_PEM_BYTES)?;
    let config = WebTlsConfig {
        advertised_origin: advertised_origin.to_string(),
        certificate_pem,
        private_key_pem,
    };
    // Single prepare after paths are loaded.
    prepare_web_tls(&config).map_err(|error| redact_error(&error))?;
    Ok(config)
}

fn read_bounded_pem_file(path: &Path, max_bytes: usize) -> Result<String, String> {
    if !path.is_absolute() {
        return Err("TLS material path must be absolute".to_string());
    }
    let meta =
        fs::symlink_metadata(path).map_err(|_| "TLS material path unavailable".to_string())?;
    if !meta.is_file() || metadata_is_reparse(&meta) {
        return Err("TLS material path must be a regular non-reparse file".to_string());
    }
    if meta.len() > max_bytes as u64 {
        return Err("TLS material file exceeds bound".to_string());
    }
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        use windows::Win32::Storage::FileSystem::{FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_READ};
        options
            .share_mode(FILE_SHARE_READ.0)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT.0);
    }
    let mut file = options
        .open(path)
        .map_err(|_| "TLS material open failed".to_string())?;
    let opened = file
        .metadata()
        .map_err(|_| "TLS material metadata failed".to_string())?;
    if !opened.is_file() || metadata_is_reparse(&opened) {
        return Err("TLS material handle is not a regular file".to_string());
    }
    let mut buf = Vec::new();
    file.by_ref()
        .take(max_bytes as u64 + 1)
        .read_to_end(&mut buf)
        .map_err(|_| "TLS material read failed".to_string())?;
    if buf.len() > max_bytes {
        return Err("TLS material file exceeds bound".to_string());
    }
    String::from_utf8(buf).map_err(|_| "TLS material must be UTF-8 PEM text".to_string())
}

fn metadata_is_reparse(metadata: &fs::Metadata) -> bool {
    if metadata.file_type().is_symlink() {
        return true;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
        return metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0;
    }
    #[cfg(not(windows))]
    {
        false
    }
}

fn is_loopback_ip(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(v4) => v4.is_loopback(),
        IpAddr::V6(v6) => v6.is_loopback(),
    }
}

fn is_wildcard_ip(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(v4) => v4.is_unspecified(),
        IpAddr::V6(v6) => v6.is_unspecified(),
    }
}

fn is_private_lan_ip(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(v4) => is_private_ipv4(v4),
        IpAddr::V6(v6) => is_unique_local_ipv6(v6),
    }
}

fn is_private_ipv4(address: Ipv4Addr) -> bool {
    let octets = address.octets();
    matches!(octets[0], 10)
        || (octets[0] == 172 && (16..=31).contains(&octets[1]))
        || (octets[0] == 192 && octets[1] == 168)
}

fn is_unique_local_ipv6(address: Ipv6Addr) -> bool {
    (address.octets()[0] & 0xfe) == 0xfc
}

fn build_enable_identity_command_with<P, V>(
    store: &mut IsolatedRemoteStore<P>,
    vault: &V,
    binding: &MachineBinding,
    command_id: CommandId,
) -> Result<IdentityCommand, String>
where
    P: IdentityPersistence + 'static,
    V: CredentialVault,
{
    match store.identity_live_state() {
        Ok(ConnectIdentityLiveState::Pending) => {
            Err("identity transition pending; explicit recovery required before enable".to_string())
        }
        Ok(ConnectIdentityLiveState::Live) | Ok(ConnectIdentityLiveState::Absent) => {
            let loaded = store
                .load(binding, vault)
                .map_err(|error| format!("identity load failed: {error}"))?;
            // Logical document revision — not the persistence CAS epoch.
            let expected_revision = loaded.revision();
            let now_epoch_ms = current_epoch_ms_for_identity()?;
            Ok(IdentityCommand {
                command_id,
                expected_revision,
                op: IdentityOp::Enable {
                    host_build: connect_host_build(),
                    now_epoch_ms,
                },
            })
        }
        Err(error) => Err(format!("identity live state unavailable: {error}")),
    }
}

fn resolve_host_profile_name() -> String {
    crate::persistence::app_instance_profile().unwrap_or_else(|| "production".to_string())
}

/// Production helper seam: Live verifies; Absent/Pending execute the exact
/// command (Pending resumes matching command_id+digest), then verify.
fn ensure_host_identity_with_store<P, V>(
    store: &mut IsolatedRemoteStore<P>,
    vault: &mut V,
    binding: &MachineBinding,
    command: &IdentityCommand,
) -> Result<String, String>
where
    P: IdentityPersistence + 'static,
    V: CredentialVault,
{
    match store.identity_live_state() {
        Ok(ConnectIdentityLiveState::Live) => {
            let loaded = store
                .load(binding, vault)
                .map_err(|error| format!("identity verify failed: {error}"))?;
            if loaded.has_pending_transition() {
                return Err("identity transition pending after live state".to_string());
            }
            let identity = loaded
                .identity()
                .ok_or_else(|| "identity missing after live state".to_string())?;
            Ok(host_public_id_string(identity.host_public_id()))
        }
        Ok(ConnectIdentityLiveState::Absent) | Ok(ConnectIdentityLiveState::Pending) => {
            store
                .execute(binding, vault, command.clone())
                .map_err(|error| format!("identity enable failed: {error}"))?;
            let loaded = store
                .load(binding, vault)
                .map_err(|error| format!("identity reload failed: {error}"))?;
            if loaded.has_pending_transition() {
                return Err("identity transition still pending after execute".to_string());
            }
            let identity = loaded
                .identity()
                .ok_or_else(|| "identity missing after enable".to_string())?;
            Ok(host_public_id_string(identity.host_public_id()))
        }
        Err(error) => Err(format!("identity live state unavailable: {error}")),
    }
}

fn ensure_host_identity_committed_os<P: IdentityPersistence + 'static>(
    store: &mut IsolatedRemoteStore<P>,
    vault: &mut OsConnectHostVault,
    binding: &MachineBinding,
    command: &IdentityCommand,
) -> Result<String, String> {
    let host_id = ensure_host_identity_with_store(store, vault, binding, command)?;
    let loaded = store
        .load(binding, vault)
        .map_err(|error| format!("identity reload failed: {error}"))?;
    let identity = loaded
        .identity()
        .ok_or_else(|| "identity missing after ensure".to_string())?;
    vault
        .load_host_noise(identity)
        .map_err(map_vault_repair_error)?;
    Ok(host_id)
}

fn map_vault_repair_error(error: IdentityError) -> String {
    match error {
        IdentityError::MissingCredentialProof | IdentityError::UnsupportedOperation => {
            "host vault repair required: committed custody missing or mismatched".to_string()
        }
        other => format!("host vault verify failed: {other}"),
    }
}

fn persist_enabled_listener(
    options: &RemoteListenOptions,
    tls: Option<&WebTlsConfig>,
) -> Result<RemoteHostConfig, String> {
    let bind_address = options.bind_address.trim().to_string();
    let port = options.port;
    let tls_owned = tls.cloned();
    crate::remote::update_web_listener_config(|web| {
        web.enabled = true;
        web.bind_address = bind_address;
        web.port = port;
        web.tls = tls_owned;
        web.ensure_secrets();
    })
    .map_err(|error| format!("failed to persist remote listener config: {error}"))
}

fn load_only_host_public_id() -> Result<Option<String>, String> {
    let profile = resolve_host_profile_name();
    let binding = derive_machine_binding(&profile)
        .map_err(|error| format!("machine binding unavailable: {error}"))?;
    let vault_root = crate::persistence::app_config_dir()
        .map_err(|error| format!("app config dir unavailable: {error}"))?
        .join("connect-host-vault");
    let vault = OsConnectHostVault::open(vault_root, binding.clone())
        .map_err(|error| format!("host vault unavailable: {error}"))?;
    let mut store = IsolatedRemoteStore::<KernelIdentityPersistence>::open_active_profile()
        .map_err(|error| format!("identity store unavailable: {error}"))?;
    match store.identity_live_state() {
        Ok(ConnectIdentityLiveState::Live) => {}
        Ok(ConnectIdentityLiveState::Absent) => return Ok(None),
        Ok(ConnectIdentityLiveState::Pending) => {
            return Err("identity transition pending during load-only open".to_string());
        }
        Err(error) => return Err(format!("identity live state unavailable: {error}")),
    }
    let loaded = store
        .load(&binding, &vault)
        .map_err(|error| format!("identity load failed: {error}"))?;
    let identity = loaded
        .identity()
        .ok_or_else(|| "identity missing while live".to_string())?;
    vault
        .load_host_noise(identity)
        .map_err(map_vault_repair_error)?;
    Ok(Some(host_public_id_string(identity.host_public_id())))
}

fn host_public_id_string(host: HostPublicId) -> String {
    Uuid::from_bytes(*host.as_bytes()).to_string()
}

fn connect_host_build() -> u32 {
    let bytes = env!("CARGO_PKG_VERSION").as_bytes();
    let mut acc = 1_u32;
    for (index, byte) in bytes.iter().enumerate() {
        acc = acc
            .wrapping_mul(33)
            .wrapping_add(u32::from(*byte))
            .wrapping_add(index as u32);
    }
    if acc == 0 {
        1
    } else {
        acc
    }
}

fn redact_error(message: &str) -> String {
    let lower = message.to_ascii_lowercase();
    if lower.contains("private")
        || lower.contains("pem")
        || lower.contains("key")
        || lower.contains("secret")
        || lower.contains("token")
        || lower.contains("cookie")
    {
        return "remote access configuration error".to_string();
    }
    truncate_chars(message, MAX_ERROR_MESSAGE_CHARS)
}

fn truncate_chars(value: &str, max_chars: usize) -> String {
    match value.char_indices().nth(max_chars) {
        None => value.to_string(),
        Some((end, _)) => format!("{}…", &value[..end]),
    }
}

fn current_epoch_ms_for_identity() -> Result<u64, String> {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| "system clock before unix epoch".to_string())
        .and_then(|duration| {
            u64::try_from(duration.as_millis()).map_err(|_| "epoch millis overflow".to_string())
        })
}

enum ClearPairing {
    Yes,
    No,
}

fn publish(
    board: &Arc<Mutex<SetupBoard>>,
    status: RemoteSetupStatus,
    outcome: Option<RemoteSetupReply>,
    clear_pairing: ClearPairing,
) {
    if let Ok(mut guard) = board.lock() {
        guard.status = status;
        if matches!(clear_pairing, ClearPairing::Yes) {
            guard.cached_pairing = None;
        }
        if outcome.is_some() {
            guard.last_outcome = outcome;
        }
    }
}

fn set_status(
    board: &Arc<Mutex<SetupBoard>>,
    state: RemoteSetupState,
    command_id: Option<CommandId>,
    origin: Option<String>,
    error: Option<String>,
    clear_pairing: ClearPairing,
) {
    if let Ok(mut guard) = board.lock() {
        let clear_origin = origin.is_none()
            && matches!(
                state,
                RemoteSetupState::Disabled
                    | RemoteSetupState::Failed
                    | RemoteSetupState::Stopping
                    | RemoteSetupState::Starting
            );
        guard.status.state = state;
        if let Some(command_id) = command_id {
            guard.status.last_command_id = Some(command_id);
        }
        if clear_origin {
            guard.status.origin = None;
        } else if let Some(origin) = origin {
            guard.status.origin = Some(origin);
        }
        guard.status.error = error;
        if matches!(clear_pairing, ClearPairing::Yes) {
            guard.cached_pairing = None;
        }
    }
}

fn board_public_status(board: &SetupBoard) -> RemoteSetupStatus {
    let mut status = board.status.clone();
    status.retry_command_id = board
        .retained_enable
        .as_ref()
        .map(|retained| retained.command.command_id);
    if status.listener.is_none() {
        status.listener = board.status.listener.clone();
    }
    status
}

fn listener_summary_from_config(config: &RemoteHostConfig) -> RemoteListenerSummary {
    RemoteListenerSummary {
        bind_address: config.web.bind_address.clone(),
        port: config.web.port,
        advertised_origin: config
            .web
            .tls
            .as_ref()
            .map(|tls| tls.advertised_origin.clone()),
        tls_configured: config.web.tls.is_some(),
    }
}

fn set_host_public_id(board: &Arc<Mutex<SetupBoard>>, host_public_id: String) {
    if let Ok(mut guard) = board.lock() {
        guard.status.host_public_id = Some(host_public_id);
    }
}

fn set_cached_pairing(board: &Arc<Mutex<SetupBoard>>, pairing: Option<CachedPairing>) {
    if let Ok(mut guard) = board.lock() {
        guard.cached_pairing = pairing;
    }
}

fn fail_status(board: &Arc<Mutex<SetupBoard>>, command_id: Option<CommandId>, message: &str) {
    set_status(
        board,
        RemoteSetupState::Failed,
        command_id,
        None,
        Some(message.to_string()),
        ClearPairing::Yes,
    );
}

fn clear_retained_enable(board: &Arc<Mutex<SetupBoard>>) {
    if let Ok(mut guard) = board.lock() {
        guard.retained_enable = None;
    }
}

fn clear_listening_and_pairing(board: &Arc<Mutex<SetupBoard>>) {
    if let Ok(mut guard) = board.lock() {
        guard.cached_pairing = None;
        if matches!(
            guard.status.state,
            RemoteSetupState::Listening
                | RemoteSetupState::Starting
                | RemoteSetupState::Stopping
                | RemoteSetupState::Initializing
        ) {
            guard.status.state = RemoteSetupState::Disabled;
            guard.status.origin = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering as AtomicOrdering};
    use std::sync::{Arc, Barrier, Mutex as StdMutex};
    use std::time::Instant;

    struct UnblockOnDrop(Arc<AtomicBool>);
    impl Drop for UnblockOnDrop {
        fn drop(&mut self) {
            self.0.store(false, AtomicOrdering::SeqCst);
        }
    }

    fn wait_until(deadline: Instant, mut pred: impl FnMut() -> bool) -> bool {
        while Instant::now() < deadline {
            if pred() {
                return true;
            }
            thread::sleep(Duration::from_millis(5));
        }
        pred()
    }

    struct FakeListener {
        active: bool,
        starts: Arc<AtomicU32>,
        stops: Arc<AtomicU32>,
        fail_next_start: Arc<AtomicBool>,
        block_start: Arc<AtomicBool>,
        started_gate: Arc<AtomicBool>,
    }

    impl ListenerControl for FakeListener {
        fn stop_join(&mut self) -> Result<(), String> {
            self.stops.fetch_add(1, AtomicOrdering::SeqCst);
            self.active = false;
            Ok(())
        }

        fn start_from_config(&mut self, config: RemoteHostConfig) -> Result<(), String> {
            self.starts.fetch_add(1, AtomicOrdering::SeqCst);
            self.started_gate.store(true, AtomicOrdering::SeqCst);
            while self.block_start.load(AtomicOrdering::SeqCst) {
                thread::sleep(Duration::from_millis(5));
            }
            if self.fail_next_start.swap(false, AtomicOrdering::SeqCst) {
                return Err("injected start failure".to_string());
            }
            self.active = config.web.enabled;
            Ok(())
        }

        fn is_active(&self) -> bool {
            self.active
        }
    }

    struct FakeListenerFactory {
        starts: Arc<AtomicU32>,
        stops: Arc<AtomicU32>,
        fail_next_start: Arc<AtomicBool>,
        block_start: Arc<AtomicBool>,
        started_gate: Arc<AtomicBool>,
        block_build: Arc<AtomicBool>,
        build_entered: Arc<AtomicBool>,
        fail_build: Arc<AtomicBool>,
    }

    impl ListenerFactory for FakeListenerFactory {
        fn build(self: Box<Self>) -> Result<Box<dyn ListenerControl>, String> {
            self.build_entered.store(true, AtomicOrdering::SeqCst);
            while self.block_build.load(AtomicOrdering::SeqCst) {
                thread::sleep(Duration::from_millis(5));
            }
            if self.fail_build.load(AtomicOrdering::SeqCst) {
                return Err("injected factory failure".to_string());
            }
            Ok(Box::new(FakeListener {
                active: false,
                starts: self.starts,
                stops: self.stops,
                fail_next_start: self.fail_next_start,
                block_start: self.block_start,
                started_gate: self.started_gate,
            }))
        }
    }

    #[derive(Clone)]
    struct FakeBootstrap {
        config: Arc<StdMutex<RemoteHostConfig>>,
        host_id: Arc<StdMutex<Option<String>>>,
        ensure_calls: Arc<AtomicU32>,
        recover_calls: Arc<AtomicU32>,
        last_command: Arc<StdMutex<Option<IdentityCommand>>>,
        fail_ensure: Arc<AtomicBool>,
        pending: Arc<AtomicBool>,
    }

    impl FakeBootstrap {
        fn new() -> Self {
            let mut config = RemoteHostConfig::default();
            config.web.enabled = false;
            config.web.bind_address = "127.0.0.1".to_string();
            config.web.port = 18080;
            config.web.pairing_token = "PAIRTEST1".to_string();
            Self {
                config: Arc::new(StdMutex::new(config)),
                host_id: Arc::new(StdMutex::new(Some(
                    "00000000-0000-7000-8000-000000000001".to_string(),
                ))),
                ensure_calls: Arc::new(AtomicU32::new(0)),
                recover_calls: Arc::new(AtomicU32::new(0)),
                last_command: Arc::new(StdMutex::new(None)),
                fail_ensure: Arc::new(AtomicBool::new(false)),
                pending: Arc::new(AtomicBool::new(false)),
            }
        }
    }

    impl SetupBootstrap for FakeBootstrap {
        fn load_host_config(&self) -> Result<RemoteHostConfig, String> {
            Ok(self.config.lock().unwrap().clone())
        }

        fn load_verified_host_id(&self) -> Result<Option<String>, String> {
            Ok(self.host_id.lock().unwrap().clone())
        }

        fn identity_is_pending(&self) -> bool {
            self.pending.load(AtomicOrdering::SeqCst)
        }

        fn recover_orphaned_register_device_pending(
            &self,
            cancel: &AtomicBool,
        ) -> Result<(), String> {
            if cancel.load(AtomicOrdering::Acquire) {
                return Ok(());
            }
            self.recover_calls.fetch_add(1, AtomicOrdering::SeqCst);
            Ok(())
        }

        fn build_enable_command(&self, command_id: CommandId) -> Result<IdentityCommand, String> {
            if self.pending.load(AtomicOrdering::SeqCst) {
                return Err(
                    "identity transition pending; explicit recovery required before enable"
                        .to_string(),
                );
            }
            Ok(IdentityCommand {
                command_id,
                expected_revision: 3,
                op: IdentityOp::Enable {
                    host_build: 11,
                    now_epoch_ms: 42,
                },
            })
        }

        fn ensure_identity_committed(&self, command: &IdentityCommand) -> Result<String, String> {
            self.ensure_calls.fetch_add(1, AtomicOrdering::SeqCst);
            *self.last_command.lock().unwrap() = Some(command.clone());
            if self.fail_ensure.load(AtomicOrdering::SeqCst) {
                // Model partial custody failure: pending appears during ensure,
                // after Enable already retained the exact command.
                self.pending.store(true, AtomicOrdering::SeqCst);
                return Err("injected custody failure".to_string());
            }
            self.pending.store(false, AtomicOrdering::SeqCst);
            self.host_id
                .lock()
                .unwrap()
                .clone()
                .ok_or_else(|| "missing host id".to_string())
        }

        fn persist_enabled(
            &self,
            options: &RemoteListenOptions,
            tls: Option<&WebTlsConfig>,
        ) -> Result<RemoteHostConfig, String> {
            let mut config = self.config.lock().unwrap();
            config.web.enabled = true;
            config.web.bind_address = options.bind_address.clone();
            config.web.port = options.port;
            config.web.tls = tls.cloned();
            Ok(config.clone())
        }

        fn persist_disabled(&self) -> Result<RemoteHostConfig, String> {
            let mut config = self.config.lock().unwrap();
            config.web.enabled = false;
            Ok(config.clone())
        }

        fn pairing_from_config(&self, config: &RemoteHostConfig) -> Option<CachedPairing> {
            if !config.web.enabled || config.web.pairing_token.is_empty() {
                return None;
            }
            Some(CachedPairing {
                code: config.web.pairing_token.clone(),
                url: config.web.display_url(),
            })
        }
    }

    fn start_fake_parts(
        bootstrap: FakeBootstrap,
        block_start: bool,
        block_build: bool,
        fail_build: bool,
    ) -> (
        RemoteSetupHandle,
        RemoteSetupRuntime<'static>,
        Arc<AtomicBool>,
        Arc<AtomicBool>,
        Arc<AtomicBool>,
        Arc<AtomicBool>,
    ) {
        let starts = Arc::new(AtomicU32::new(0));
        let stops = Arc::new(AtomicU32::new(0));
        let fail_next_start = Arc::new(AtomicBool::new(false));
        let block_start = Arc::new(AtomicBool::new(block_start));
        let started_gate = Arc::new(AtomicBool::new(false));
        let block_build = Arc::new(AtomicBool::new(block_build));
        let build_entered = Arc::new(AtomicBool::new(false));
        let fail_build = Arc::new(AtomicBool::new(fail_build));
        let factory = FakeListenerFactory {
            starts,
            stops,
            fail_next_start,
            block_start: Arc::clone(&block_start),
            started_gate: Arc::clone(&started_gate),
            block_build: Arc::clone(&block_build),
            build_entered: Arc::clone(&build_entered),
            fail_build,
        };
        let (handle, runtime) =
            RemoteSetupRuntime::start_with_parts(Box::new(factory), Box::new(bootstrap))
                .expect("start returns promptly");
        (
            handle,
            runtime,
            block_start,
            started_gate,
            block_build,
            build_entered,
        )
    }

    fn start_fake(
        bootstrap: FakeBootstrap,
        block_start: bool,
    ) -> (
        RemoteSetupHandle,
        RemoteSetupRuntime<'static>,
        Arc<AtomicBool>,
        Arc<AtomicBool>,
    ) {
        let (handle, runtime, block_start, started_gate, _block_build, _entered) =
            start_fake_parts(bootstrap, block_start, false, false);
        (handle, runtime, block_start, started_gate)
    }

    #[test]
    fn loopback_options_accept_http_without_tls() {
        let options = RemoteListenOptions {
            bind_address: "127.0.0.1".to_string(),
            port: 8443,
            advertised_origin: None,
            certificate_path: None,
            private_key_path: None,
        };
        assert!(validate_listen_options(&options, None).unwrap().is_none());
    }

    #[test]
    fn wildcard_and_public_bind_rejected_before_tls_paths() {
        for address in ["0.0.0.0", "8.8.8.8", "::", "2001:db8::1"] {
            let options = RemoteListenOptions {
                bind_address: address.to_string(),
                port: 8443,
                advertised_origin: Some("https://x:8443".to_string()),
                certificate_path: Some("C:\\missing-cert.pem".to_string()),
                private_key_path: Some("C:\\missing-key.pem".to_string()),
            };
            let err = validate_listen_options(&options, None).expect_err("reject");
            assert!(
                !err.to_ascii_lowercase().contains("open"),
                "must reject bind before path IO: {err}"
            );
        }
    }

    #[test]
    fn advertised_origin_port_must_match_listen_port() {
        let options = RemoteListenOptions {
            bind_address: "127.0.0.1".to_string(),
            port: 8443,
            advertised_origin: Some("https://example.local".to_string()),
            certificate_path: None,
            private_key_path: None,
        };
        let err = validate_listen_options(&options, None).expect_err("port mismatch");
        assert!(err.contains("must match listen port"));
        assert_eq!(
            parse_advertised_origin_port("https://example.local").unwrap(),
            443
        );
        assert_eq!(
            parse_advertised_origin_port("https://example.local:8443").unwrap(),
            8443
        );
    }

    #[test]
    fn redact_error_truncates_on_char_boundary() {
        let message = "á".repeat(300);
        let redacted = redact_error(&message);
        assert!(redacted.ends_with('…'));
        assert!(redacted.is_char_boundary(redacted.len() - '…'.len_utf8()));
    }

    #[test]
    fn snapshot_and_pairing_are_synchronous_during_blocked_mutation() {
        let bootstrap = FakeBootstrap::new();
        let (handle, runtime, block_start, started_gate) = start_fake(bootstrap, true);
        let _unblock = UnblockOnDrop(Arc::clone(&block_start));
        let command_id = CommandId::new();
        assert!(matches!(
            handle.request(RemoteSetupRequest::Enable {
                command_id,
                options: RemoteListenOptions {
                    bind_address: "127.0.0.1".to_string(),
                    port: 18080,
                    advertised_origin: None,
                    certificate_path: None,
                    private_key_path: None,
                },
            }),
            RemoteSetupReply::Accepted { .. }
        ));
        assert!(wait_until(Instant::now() + TEST_WAIT_BUDGET, || {
            started_gate.load(AtomicOrdering::SeqCst)
        }));
        let started = Instant::now();
        assert!(matches!(
            handle.request(RemoteSetupRequest::Snapshot),
            RemoteSetupReply::Snapshot { .. }
        ));
        assert!(started.elapsed() < Duration::from_millis(200));
        drop(_unblock);
        runtime.shutdown().expect("shutdown");
    }

    #[test]
    fn concurrent_mutation_admission_races_at_barrier() {
        let bootstrap = FakeBootstrap::new();
        let (handle, runtime, block_start, _started) = start_fake(bootstrap, true);
        let _unblock = UnblockOnDrop(Arc::clone(&block_start));
        let barrier = Arc::new(Barrier::new(3));
        let h1 = handle.clone();
        let h2 = handle.clone();
        let b1 = Arc::clone(&barrier);
        let b2 = Arc::clone(&barrier);
        let t1 = thread::spawn(move || {
            b1.wait();
            h1.request(RemoteSetupRequest::Enable {
                command_id: CommandId::new(),
                options: RemoteListenOptions {
                    bind_address: "127.0.0.1".to_string(),
                    port: 18080,
                    advertised_origin: None,
                    certificate_path: None,
                    private_key_path: None,
                },
            })
        });
        let t2 = thread::spawn(move || {
            b2.wait();
            h2.request(RemoteSetupRequest::Enable {
                command_id: CommandId::new(),
                options: RemoteListenOptions {
                    bind_address: "127.0.0.1".to_string(),
                    port: 18081,
                    advertised_origin: None,
                    certificate_path: None,
                    private_key_path: None,
                },
            })
        });
        barrier.wait();
        let r1 = t1.join().expect("t1");
        let r2 = t2.join().expect("t2");
        let accepted = matches!(r1, RemoteSetupReply::Accepted { .. }) as u8
            + matches!(r2, RemoteSetupReply::Accepted { .. }) as u8;
        let busy =
            matches!(r1, RemoteSetupReply::Busy) as u8 + matches!(r2, RemoteSetupReply::Busy) as u8;
        assert_eq!(accepted, 1, "exactly one admission wins");
        assert_eq!(busy, 1, "loser is Busy");
        drop(_unblock);
        runtime.shutdown().expect("shutdown");
    }

    #[test]
    fn blocked_factory_allows_prompt_start_and_cancel() {
        let bootstrap = FakeBootstrap::new();
        let (handle, runtime, _block_start, _started, block_build, build_entered) =
            start_fake_parts(bootstrap, false, true, false);
        let _unblock = UnblockOnDrop(Arc::clone(&block_build));
        assert!(wait_until(Instant::now() + TEST_WAIT_BUDGET, || {
            build_entered.load(AtomicOrdering::SeqCst)
        }));
        assert!(matches!(
            handle.status().state,
            RemoteSetupState::Initializing
        ));
        drop(_unblock);
        runtime.shutdown().expect("shutdown");
    }

    #[test]
    fn factory_failure_publishes_failed_and_join_errs() {
        let bootstrap = FakeBootstrap::new();
        let (handle, runtime, _a, _b, _c, entered) =
            start_fake_parts(bootstrap, false, false, true);
        assert!(wait_until(Instant::now() + TEST_WAIT_BUDGET, || {
            entered.load(AtomicOrdering::SeqCst)
                && matches!(handle.status().state, RemoteSetupState::Failed)
        }));
        assert!(handle.board.lock().unwrap().cached_pairing.is_none());
        let err = runtime.shutdown().expect_err("factory failure");
        assert!(err.contains("injected factory failure"));
    }

    #[test]
    fn cancel_during_blocked_start_does_not_publish_listening() {
        let bootstrap = FakeBootstrap::new();
        let (handle, runtime, block_start, started_gate) = start_fake(bootstrap, true);
        let _unblock = UnblockOnDrop(Arc::clone(&block_start));
        assert!(matches!(
            handle.request(RemoteSetupRequest::Enable {
                command_id: CommandId::new(),
                options: RemoteListenOptions {
                    bind_address: "127.0.0.1".to_string(),
                    port: 18080,
                    advertised_origin: None,
                    certificate_path: None,
                    private_key_path: None,
                },
            }),
            RemoteSetupReply::Accepted { .. }
        ));
        assert!(wait_until(Instant::now() + TEST_WAIT_BUDGET, || {
            started_gate.load(AtomicOrdering::SeqCst)
        }));
        // Cancel while start is blocked; unblock so stop can finish.
        let join = thread::spawn(move || runtime.shutdown());
        thread::sleep(Duration::from_millis(30));
        block_start.store(false, AtomicOrdering::SeqCst);
        join.join().expect("join").expect("shutdown ok");
        assert!(!matches!(
            handle.status().state,
            RemoteSetupState::Listening | RemoteSetupState::Starting
        ));
        assert!(handle.board.lock().unwrap().cached_pairing.is_none());
    }

    #[test]
    fn exact_retry_reuses_controller_retained_command_not_clone_tautology() {
        let bootstrap = FakeBootstrap::new();
        bootstrap.fail_ensure.store(true, AtomicOrdering::SeqCst);
        let (handle, runtime, block_start, _started) = start_fake(bootstrap.clone(), false);
        let _unblock = UnblockOnDrop(Arc::clone(&block_start));
        let command_id = CommandId::new();
        assert!(matches!(
            handle.request(RemoteSetupRequest::Enable {
                command_id,
                options: RemoteListenOptions {
                    bind_address: "127.0.0.1".to_string(),
                    port: 18080,
                    advertised_origin: None,
                    certificate_path: None,
                    private_key_path: None,
                },
            }),
            RemoteSetupReply::Accepted { .. }
        ));
        assert!(wait_until(Instant::now() + TEST_WAIT_BUDGET, || {
            !handle.board.lock().unwrap().mutation_inflight
        }));
        assert_eq!(handle.status().retry_command_id, Some(command_id));
        bootstrap.fail_ensure.store(false, AtomicOrdering::SeqCst);
        assert!(matches!(
            handle.request(RemoteSetupRequest::Retry { command_id }),
            RemoteSetupReply::Accepted { .. }
        ));
        assert!(wait_until(Instant::now() + TEST_WAIT_BUDGET, || {
            bootstrap.ensure_calls.load(AtomicOrdering::SeqCst) >= 2
        }));
        let retained = bootstrap.last_command.lock().unwrap().clone().expect("cmd");
        assert_eq!(retained.command_id, command_id);
        assert_eq!(retained.expected_revision, 3);
        match retained.op {
            IdentityOp::Enable {
                host_build,
                now_epoch_ms,
            } => {
                assert_eq!(host_build, 11);
                assert_eq!(now_epoch_ms, 42);
            }
            _ => panic!("expected Enable"),
        }
        drop(_unblock);
        runtime.shutdown().expect("shutdown");
    }

    #[test]
    fn pending_blocks_new_enable_keeps_retained_for_retry() {
        let bootstrap = FakeBootstrap::new();
        bootstrap.fail_ensure.store(true, AtomicOrdering::SeqCst);
        let (handle, runtime, block_start, _) = start_fake(bootstrap.clone(), false);
        let _unblock = UnblockOnDrop(Arc::clone(&block_start));
        let command_id = CommandId::new();
        assert!(matches!(
            handle.request(RemoteSetupRequest::Enable {
                command_id,
                options: RemoteListenOptions {
                    bind_address: "127.0.0.1".to_string(),
                    port: 18080,
                    advertised_origin: None,
                    certificate_path: None,
                    private_key_path: None,
                },
            }),
            RemoteSetupReply::Accepted { .. }
        ));
        assert!(wait_until(Instant::now() + TEST_WAIT_BUDGET, || {
            let board = handle.board.lock().unwrap();
            !board.mutation_inflight && board.identity_transition_pending
        }));
        assert!(bootstrap.pending.load(AtomicOrdering::SeqCst));
        let other = handle.request(RemoteSetupRequest::Enable {
            command_id: CommandId::new(),
            options: RemoteListenOptions {
                bind_address: "127.0.0.1".to_string(),
                port: 18081,
                advertised_origin: None,
                certificate_path: None,
                private_key_path: None,
            },
        });
        assert!(matches!(other, RemoteSetupReply::Error { .. }));
        assert_eq!(handle.status().retry_command_id, Some(command_id));
        drop(_unblock);
        runtime.shutdown().expect("shutdown");
    }

    #[test]
    fn disable_preserves_retained_enable_after_custody_failure() {
        let bootstrap = FakeBootstrap::new();
        bootstrap.fail_ensure.store(true, AtomicOrdering::SeqCst);
        let (handle, runtime, block_start, _) = start_fake(bootstrap.clone(), false);
        let _unblock = UnblockOnDrop(Arc::clone(&block_start));
        let command_id = CommandId::new();
        assert!(matches!(
            handle.request(RemoteSetupRequest::Enable {
                command_id,
                options: RemoteListenOptions {
                    bind_address: "127.0.0.1".to_string(),
                    port: 18080,
                    advertised_origin: None,
                    certificate_path: None,
                    private_key_path: None,
                },
            }),
            RemoteSetupReply::Accepted { .. }
        ));
        assert!(wait_until(Instant::now() + TEST_WAIT_BUDGET, || {
            !handle.board.lock().unwrap().mutation_inflight
        }));
        assert!(handle.board.lock().unwrap().retained_enable.is_some());
        assert!(matches!(
            handle.request(RemoteSetupRequest::Disable {
                command_id: CommandId::new(),
            }),
            RemoteSetupReply::Accepted { .. }
        ));
        assert!(wait_until(Instant::now() + TEST_WAIT_BUDGET, || {
            !handle.board.lock().unwrap().mutation_inflight
        }));
        assert!(
            handle.board.lock().unwrap().retained_enable.is_some(),
            "Disable must keep retained Enable for explicit Retry"
        );
        drop(_unblock);
        runtime.shutdown().expect("shutdown");
    }

    #[test]
    fn pairing_info_debug_redacts_code() {
        let reply = RemoteSetupReply::PairingInfo {
            code: "SECRETCODE".to_string(),
            url: "http://127.0.0.1:1".to_string(),
        };
        let rendered = format!("{reply:?}");
        assert!(rendered.contains("<redacted>"));
        assert!(!rendered.contains("SECRETCODE"));
    }

    #[cfg(windows)]
    #[test]
    fn pending_exact_command_resumes_via_store_execute_not_fresh_timestamps() {
        use crate::connect::{
            DeviceEstablishmentHandle, DeviceId, DeviceKeyProof, DeviceKind, DeviceRepairHandle,
            HostEstablishmentHandle, HostKeyProof, HostPublicId, HostRotationHandle,
        };

        struct FailOnceCommitVault {
            inner: OsConnectHostVault,
            fail_once: Arc<AtomicBool>,
        }

        impl CredentialVault for FailOnceCommitVault {
            fn establish_host(
                &mut self,
                host_id: HostPublicId,
                transition_nonce: [u8; 16],
            ) -> Result<HostEstablishmentHandle, IdentityError> {
                self.inner.establish_host(host_id, transition_nonce)
            }
            fn commit_host_establishment(
                &mut self,
                handle: &HostEstablishmentHandle,
            ) -> Result<(), IdentityError> {
                if self.fail_once.swap(false, AtomicOrdering::SeqCst) {
                    return Err(IdentityError::PersistFailed);
                }
                self.inner.commit_host_establishment(handle)
            }
            fn rollback_host_establishment(
                &mut self,
                handle: &HostEstablishmentHandle,
            ) -> Result<(), IdentityError> {
                self.inner.rollback_host_establishment(handle)
            }
            fn recover_host_establishment(
                &mut self,
                host_id: HostPublicId,
                transition_nonce: [u8; 16],
            ) -> Result<Option<HostEstablishmentHandle>, IdentityError> {
                self.inner
                    .recover_host_establishment(host_id, transition_nonce)
            }
            fn host_establishment_committed(
                &self,
                handle: &HostEstablishmentHandle,
            ) -> Result<bool, IdentityError> {
                self.inner.host_establishment_committed(handle)
            }
            fn prepare_host_rotation(
                &mut self,
                host_id: HostPublicId,
                transition_nonce: [u8; 16],
            ) -> Result<HostRotationHandle, IdentityError> {
                self.inner.prepare_host_rotation(host_id, transition_nonce)
            }
            fn commit_host_rotation(
                &mut self,
                handle: &HostRotationHandle,
            ) -> Result<(), IdentityError> {
                self.inner.commit_host_rotation(handle)
            }
            fn abort_host_rotation(
                &mut self,
                handle: &HostRotationHandle,
            ) -> Result<(), IdentityError> {
                self.inner.abort_host_rotation(handle)
            }
            fn recover_host_rotation(
                &mut self,
                host_id: HostPublicId,
                transition_nonce: [u8; 16],
            ) -> Result<Option<HostRotationHandle>, IdentityError> {
                self.inner.recover_host_rotation(host_id, transition_nonce)
            }
            fn verify_host(
                &self,
                host_id: HostPublicId,
                proof: &HostKeyProof,
            ) -> Result<(), IdentityError> {
                self.inner.verify_host(host_id, proof)
            }
            fn establish_device(
                &mut self,
                device_id: DeviceId,
                kind: DeviceKind,
                transition_nonce: [u8; 16],
            ) -> Result<DeviceEstablishmentHandle, IdentityError> {
                self.inner
                    .establish_device(device_id, kind, transition_nonce)
            }
            fn commit_device_establishment(
                &mut self,
                handle: &DeviceEstablishmentHandle,
            ) -> Result<(), IdentityError> {
                self.inner.commit_device_establishment(handle)
            }
            fn recover_device_establishment(
                &mut self,
                device_id: DeviceId,
                transition_nonce: [u8; 16],
            ) -> Result<Option<DeviceEstablishmentHandle>, IdentityError> {
                self.inner
                    .recover_device_establishment(device_id, transition_nonce)
            }
            fn device_establishment_committed(
                &self,
                handle: &DeviceEstablishmentHandle,
            ) -> Result<bool, IdentityError> {
                self.inner.device_establishment_committed(handle)
            }
            fn prepare_device_repair(
                &mut self,
                device_id: DeviceId,
                kind: DeviceKind,
                transition_nonce: [u8; 16],
            ) -> Result<DeviceRepairHandle, IdentityError> {
                self.inner
                    .prepare_device_repair(device_id, kind, transition_nonce)
            }
            fn commit_device_repair(
                &mut self,
                handle: &DeviceRepairHandle,
            ) -> Result<(), IdentityError> {
                self.inner.commit_device_repair(handle)
            }
            fn device_repair_committed(
                &self,
                handle: &DeviceRepairHandle,
            ) -> Result<bool, IdentityError> {
                self.inner.device_repair_committed(handle)
            }
            fn rollback_device_repair(
                &mut self,
                handle: &DeviceRepairHandle,
            ) -> Result<(), IdentityError> {
                self.inner.rollback_device_repair(handle)
            }
            fn abort_device_repair(
                &mut self,
                handle: &DeviceRepairHandle,
            ) -> Result<(), IdentityError> {
                self.inner.abort_device_repair(handle)
            }
            fn recover_device_repair(
                &mut self,
                device_id: DeviceId,
                transition_nonce: [u8; 16],
            ) -> Result<Option<DeviceRepairHandle>, IdentityError> {
                self.inner
                    .recover_device_repair(device_id, transition_nonce)
            }
            fn invalidate_device_credential(
                &mut self,
                device_id: DeviceId,
                revocation_epoch: u64,
            ) -> Result<(), IdentityError> {
                self.inner
                    .invalidate_device_credential(device_id, revocation_epoch)
            }
            fn restore_device_credential(
                &mut self,
                device_id: DeviceId,
                revocation_epoch: u64,
            ) -> Result<(), IdentityError> {
                self.inner
                    .restore_device_credential(device_id, revocation_epoch)
            }
            fn rollback_device_establishment(
                &mut self,
                handle: &DeviceEstablishmentHandle,
            ) -> Result<(), IdentityError> {
                self.inner.rollback_device_establishment(handle)
            }
            fn verify_device(
                &self,
                device_id: DeviceId,
                proof: &DeviceKeyProof,
            ) -> Result<(), IdentityError> {
                self.inner.verify_device(device_id, proof)
            }
        }

        let root = tempfile::Builder::new()
            .prefix("devmanager-remote-setup-pending-")
            .tempdir()
            .expect("temp vault");
        let profile = format!("remote-setup-pending-{}", Uuid::new_v4().simple());
        let binding = derive_machine_binding(&profile).expect("binding");
        let inner =
            OsConnectHostVault::open(root.path().to_path_buf(), binding.clone()).expect("vault");
        let fail_once = Arc::new(AtomicBool::new(true));
        let vault = FailOnceCommitVault {
            inner,
            fail_once: Arc::clone(&fail_once),
        };
        let store =
            IsolatedRemoteStore::new(InMemoryIdentityPersistence::default()).expect("memory store");
        let command = IdentityCommand {
            command_id: CommandId::new(),
            expected_revision: 0,
            op: IdentityOp::Enable {
                host_build: 7,
                now_epoch_ms: 99,
            },
        };
        let mut ctx = HostIdentityContext {
            store,
            vault,
            binding,
        };
        let first = ctx.ensure_committed(&command);
        assert!(first.is_err(), "first commit should fail closed");
        assert!(matches!(
            ctx.store.identity_live_state().expect("live state"),
            ConnectIdentityLiveState::Pending
        ));
        let foreign = IdentityCommand {
            command_id: CommandId::new(),
            expected_revision: command.expected_revision,
            op: command.op.clone(),
        };
        assert!(
            ctx.ensure_committed(&foreign).is_err(),
            "foreign command must not settle pending Enable"
        );
        // Exact retained command resumes via execute (command_id+digest match).
        let resumed = ctx.ensure_committed(&command).expect("resume");
        assert!(!resumed.is_empty());
        assert!(matches!(
            ctx.store.identity_live_state().expect("live"),
            ConnectIdentityLiveState::Live
        ));
    }

    #[test]
    fn fake_ensure_marks_pending_only_after_failing_ensure_not_before_build() {
        let bootstrap = FakeBootstrap::new();
        bootstrap.fail_ensure.store(true, AtomicOrdering::SeqCst);
        assert!(!bootstrap.pending.load(AtomicOrdering::SeqCst));
        let command = bootstrap
            .build_enable_command(CommandId::new())
            .expect("build must succeed while not pending");
        assert!(!bootstrap.pending.load(AtomicOrdering::SeqCst));
        assert!(bootstrap.ensure_identity_committed(&command).is_err());
        assert!(bootstrap.pending.load(AtomicOrdering::SeqCst));
        // Exact command retained for Retry; build of a fresh Enable must now refuse.
        assert!(bootstrap.build_enable_command(CommandId::new()).is_err());
        assert_eq!(
            bootstrap
                .last_command
                .lock()
                .unwrap()
                .as_ref()
                .map(|c| c.command_id),
            Some(command.command_id)
        );
        assert_eq!(
            match &command.op {
                IdentityOp::Enable { now_epoch_ms, .. } => *now_epoch_ms,
                _ => panic!("Enable"),
            },
            42,
            "exact timestamp must be retained — no fresh stamp on failure path"
        );
    }

    #[test]
    fn production_owner_fence_rejects_duplicate_until_release() {
        let first = try_claim_production_remote_setup_owner().expect("first owner");
        let duplicate = try_claim_production_remote_setup_owner();
        assert!(
            duplicate.is_err(),
            "second production controller must fail closed while first owns"
        );
        drop(first);
        let _reclaim = try_claim_production_remote_setup_owner().expect("reclaim after release");
    }

    #[test]
    fn disabled_startup_and_snapshot_do_not_run_register_recovery() {
        let bootstrap = FakeBootstrap::new();
        assert!(!bootstrap.config.lock().unwrap().web.enabled);
        let (handle, runtime, _, _, _, _) =
            start_fake_parts(bootstrap.clone(), false, false, false);
        wait_until(Instant::now() + TEST_WAIT_BUDGET, || {
            matches!(
                handle.status().state,
                RemoteSetupState::Disabled | RemoteSetupState::Failed
            )
        });
        assert_eq!(
            bootstrap.recover_calls.load(AtomicOrdering::SeqCst),
            0,
            "disabled remote startup must not mutate via RegisterDevice recovery"
        );
        let before = bootstrap.recover_calls.load(AtomicOrdering::SeqCst);
        let _ = handle.request(RemoteSetupRequest::Snapshot);
        assert_eq!(
            bootstrap.recover_calls.load(AtomicOrdering::SeqCst),
            before,
            "ordinary Snapshot must not admit recovery mutation"
        );
        let options = RemoteListenOptions {
            bind_address: "127.0.0.1".to_string(),
            port: 18080,
            advertised_origin: None,
            certificate_path: None,
            private_key_path: None,
        };
        for expected_recoveries in [1, 1] {
            assert!(matches!(
                handle.request(RemoteSetupRequest::Enable {
                    command_id: CommandId::new(),
                    options: options.clone(),
                }),
                RemoteSetupReply::Accepted { .. }
            ));
            assert!(wait_until(Instant::now() + TEST_WAIT_BUDGET, || {
                let board = handle.board.lock().unwrap();
                !board.mutation_inflight && board.status.state == RemoteSetupState::Listening
            }));
            assert_eq!(
                bootstrap.recover_calls.load(AtomicOrdering::SeqCst),
                expected_recoveries,
                "first explicit Enable recovers; a live listener's claim is never reclaimed"
            );
        }
        drop(runtime);
    }

    #[test]
    fn enabled_startup_runs_register_recovery_before_listener_start() {
        let bootstrap = FakeBootstrap::new();
        {
            let mut config = bootstrap.config.lock().unwrap();
            config.web.enabled = true;
        }
        let starts = Arc::new(AtomicU32::new(0));
        let stops = Arc::new(AtomicU32::new(0));
        let fail_next_start = Arc::new(AtomicBool::new(false));
        let block_start = Arc::new(AtomicBool::new(false));
        let started_gate = Arc::new(AtomicBool::new(false));
        let block_build = Arc::new(AtomicBool::new(false));
        let build_entered = Arc::new(AtomicBool::new(false));
        let fail_build = Arc::new(AtomicBool::new(false));
        let factory = FakeListenerFactory {
            starts: Arc::clone(&starts),
            stops,
            fail_next_start,
            block_start,
            started_gate: Arc::clone(&started_gate),
            block_build,
            build_entered,
            fail_build,
        };
        let (handle, runtime) =
            RemoteSetupRuntime::start_with_parts(Box::new(factory), Box::new(bootstrap.clone()))
                .expect("start");
        wait_until(Instant::now() + TEST_WAIT_BUDGET, || {
            started_gate.load(AtomicOrdering::SeqCst)
                || matches!(handle.status().state, RemoteSetupState::Listening)
        });
        assert_eq!(
            bootstrap.recover_calls.load(AtomicOrdering::SeqCst),
            1,
            "enabled startup must run RegisterDevice recovery once before admission"
        );
        assert!(
            starts.load(AtomicOrdering::SeqCst) >= 1,
            "listener start follows recovery"
        );
        drop(runtime);
    }
}
