//! Host-owned Connect web listener lifetime.
//!
//! The durable `devmanager-host` process owns the existing remote web/Connect
//! listener, not the desktop GPUI window. [`crate::remote::WebListenerHandle`]
//! builds and drops a multi-thread Tokio runtime; that start/drop work must run
//! on a dedicated OS thread so the host's current-thread async executor never
//! drops it.
//!
//! Ownership limits:
//! - Reads active-profile `remote.json` fail-closed; never auto-enables, rewrites
//!   ports, enrolls identity, or polls for config reload.
//! - Reuses [`crate::remote::RemoteHostService::new_web_only`] as an auth/config
//!   shell only; legacy native TCP + snapshot broadcaster stay disabled.
//! - Does not manufacture task projections, CommandBus, provider manager, or a
//!   writable semantic journal. Canonical Connect obtains
//!   [`crate::host::HostRequestHandle`] through the process slot.
//! - Shutdown stops web acceptance by calling `WebListenerHandle::shutdown`
//!   directly on the owned OS worker, then joins that worker before arm ack or
//!   Connect slot unbind. Drop signals and joins the same owned thread; it never
//!   detaches a waiter. Success requires the worker to exit after direct listener
//!   shutdown — not merely `RemoteHostServiceOwner` deferring cleanup to residue.

use crate::persistence::PersistenceError;
use crate::remote::{load_remote_machine_state, RemoteHostConfig, RemoteHostService};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

const HOST_REMOTE_ACCESS_READY_TIMEOUT: Duration = Duration::from_secs(8);
const HOST_REMOTE_ACCESS_JOIN_POLL: Duration = Duration::from_millis(10);

/// Host-owned controller for the Connect web listener lifetime.
pub struct HostRemoteAccessController {
    state: HostRemoteAccessState,
}

enum HostRemoteAccessState {
    /// Persisted web listener is disabled; no OS worker and no bind attempt.
    Disabled,
    Active(ActiveRemoteAccessWorker),
}

struct ActiveRemoteAccessWorker {
    shutdown_tx: Option<mpsc::Sender<()>>,
    /// Owned until the worker actually exits. Never moved into a detached waiter.
    join: Option<thread::JoinHandle<Result<(), String>>>,
}

#[derive(Debug)]
pub enum HostRemoteAccessError {
    RemoteState(PersistenceError),
    WorkerSpawn(String),
    WorkerStart(String),
    WorkerJoin(String),
    /// Network shutdown did not complete; full-quit arm must not be acknowledged.
    ShutdownUncertain(String),
}

impl std::fmt::Display for HostRemoteAccessError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::RemoteState(error) => {
                write!(formatter, "remote host state unavailable: {error}")
            }
            Self::WorkerSpawn(error) => {
                write!(formatter, "remote access worker spawn failed: {error}")
            }
            Self::WorkerStart(error) => {
                write!(formatter, "remote access worker start failed: {error}")
            }
            Self::WorkerJoin(error) => {
                write!(formatter, "remote access worker join failed: {error}")
            }
            Self::ShutdownUncertain(error) => {
                write!(formatter, "remote access shutdown uncertain: {error}")
            }
        }
    }
}

impl HostRemoteAccessController {
    /// Async start from the active profile's remote machine state.
    ///
    /// The profile read is a small sync IO. Worker readiness waits on a Tokio
    /// oneshot with timeout so the host current-thread loop is not blocked for
    /// the full bind interval. RAII owns the thread before the first await.
    pub async fn start_from_active_profile() -> Result<Self, HostRemoteAccessError> {
        let state = load_remote_machine_state().map_err(HostRemoteAccessError::RemoteState)?;
        Self::start_from_config(state.host).await
    }

    /// Start from an already-loaded host config (tests and explicit seams).
    /// Disabled web config returns immediately without spawning a worker.
    pub async fn start_from_config(
        config: RemoteHostConfig,
    ) -> Result<Self, HostRemoteAccessError> {
        if !config.web.enabled {
            return Ok(Self {
                state: HostRemoteAccessState::Disabled,
            });
        }
        Self::start_owned_worker(move |shutdown_rx, ready_tx| {
            let service = match RemoteHostService::new_web_only(config) {
                Ok(service) => service,
                Err(error) => {
                    let _ = ready_tx.send(Err(error.clone()));
                    return Err(error);
                }
            };
            match service.start_web_listener_for_host() {
                Ok(()) => {
                    let _ = ready_tx.send(Ok(()));
                    let _ = shutdown_rx.recv();
                    // Direct listener shutdown on this OS thread, then drop service.
                    service.shutdown_web_listener_for_host()
                }
                Err(error) => {
                    let _ = ready_tx.send(Err(error.clone()));
                    drop(service);
                    Err(error)
                }
            }
        })
        .await
    }

    async fn start_owned_worker<F>(worker_body: F) -> Result<Self, HostRemoteAccessError>
    where
        F: FnOnce(
                mpsc::Receiver<()>,
                tokio::sync::oneshot::Sender<Result<(), String>>,
            ) -> Result<(), String>
            + Send
            + 'static,
    {
        let (shutdown_tx, shutdown_rx) = mpsc::channel::<()>();
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel::<Result<(), String>>();

        // Arm RAII ownership of shutdown sender + join handle before any await.
        let join = thread::Builder::new()
            .name("devmanager-host-remote".to_string())
            .spawn(move || worker_body(shutdown_rx, ready_tx))
            .map_err(|error| HostRemoteAccessError::WorkerSpawn(error.to_string()))?;

        let mut controller = Self {
            state: HostRemoteAccessState::Active(ActiveRemoteAccessWorker {
                shutdown_tx: Some(shutdown_tx),
                join: Some(join),
            }),
        };

        match tokio::time::timeout(HOST_REMOTE_ACCESS_READY_TIMEOUT, ready_rx).await {
            Ok(Ok(Ok(()))) => Ok(controller),
            Ok(Ok(Err(error))) => {
                let _ = controller.shutdown().await;
                Err(HostRemoteAccessError::WorkerStart(error))
            }
            Ok(Err(_)) => {
                let _ = controller.shutdown().await;
                Err(HostRemoteAccessError::WorkerStart(
                    "remote access worker disconnected before ready".to_string(),
                ))
            }
            Err(_) => {
                let _ = controller.shutdown().await;
                Err(HostRemoteAccessError::WorkerStart(
                    "remote access worker failed to report ready in time".to_string(),
                ))
            }
        }
    }

    pub fn is_active(&self) -> bool {
        matches!(self.state, HostRemoteAccessState::Active(_))
    }

    pub fn is_disabled(&self) -> bool {
        matches!(self.state, HostRemoteAccessState::Disabled)
    }

    /// Stop web acceptance on the owned OS worker, then join that worker.
    ///
    /// The JoinHandle stays owned here and is joined only after `is_finished`.
    /// Callers must treat [`HostRemoteAccessError::ShutdownUncertain`] as a
    /// hard stop for full-quit arm acknowledgement.
    pub async fn shutdown(mut self) -> Result<(), HostRemoteAccessError> {
        match std::mem::replace(&mut self.state, HostRemoteAccessState::Disabled) {
            HostRemoteAccessState::Disabled => Ok(()),
            HostRemoteAccessState::Active(mut worker) => worker.shutdown_and_join_owned().await,
        }
    }
}

impl Drop for HostRemoteAccessController {
    fn drop(&mut self) {
        if let HostRemoteAccessState::Active(mut worker) =
            std::mem::replace(&mut self.state, HostRemoteAccessState::Disabled)
        {
            // Owner future canceled: signal and join the owned thread. Never detach.
            worker.signal_shutdown();
            let _ = worker.join_blocking_owned();
        }
    }
}

impl ActiveRemoteAccessWorker {
    fn signal_shutdown(&mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
    }

    async fn shutdown_and_join_owned(&mut self) -> Result<(), HostRemoteAccessError> {
        self.signal_shutdown();
        self.join_owned_when_finished().await
    }

    async fn join_owned_when_finished(&mut self) -> Result<(), HostRemoteAccessError> {
        let Some(join) = self.join.as_ref() else {
            return Ok(());
        };
        while !join.is_finished() {
            tokio::time::sleep(HOST_REMOTE_ACCESS_JOIN_POLL).await;
        }
        self.join_blocking_owned()
    }

    fn join_blocking_owned(&mut self) -> Result<(), HostRemoteAccessError> {
        let Some(join) = self.join.take() else {
            return Ok(());
        };
        match join.join() {
            Ok(Ok(())) => Ok(()),
            Ok(Err(error)) => Err(HostRemoteAccessError::ShutdownUncertain(error)),
            Err(_) => Err(HostRemoteAccessError::ShutdownUncertain(
                "remote access worker panicked".to_string(),
            )),
        }
    }
}

impl Drop for ActiveRemoteAccessWorker {
    fn drop(&mut self) {
        // Also covers cancellation while shutdown has moved the worker out of
        // its controller and is awaiting completion.
        self.signal_shutdown();
        let _ = self.join_blocking_owned();
    }
}

/// Full-quit arm seam: stop remote network acceptance and join the owned worker
/// before the caller may acknowledge physical arm. On uncertainty the controller
/// remains absent (`None`) and the error must not be followed by an arm ack.
pub async fn shutdown_remote_access_before_fullquit_arm(
    remote_access: &mut Option<HostRemoteAccessController>,
) -> Result<(), HostRemoteAccessError> {
    let Some(controller) = remote_access.take() else {
        return Ok(());
    };
    match controller.shutdown().await {
        Ok(()) => Ok(()),
        Err(error) => Err(match error {
            HostRemoteAccessError::ShutdownUncertain(message)
            | HostRemoteAccessError::WorkerJoin(message) => {
                HostRemoteAccessError::ShutdownUncertain(message)
            }
            other => HostRemoteAccessError::ShutdownUncertain(other.to_string()),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::Instant;

    fn block_on<T>(future: impl std::future::Future<Output = T>) -> T {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime")
            .block_on(future)
    }

    #[test]
    fn disabled_web_config_starts_no_listener_worker() {
        let mut config = RemoteHostConfig::default();
        config.web.enabled = false;
        let controller = block_on(HostRemoteAccessController::start_from_config(config))
            .expect("disabled start");
        assert!(controller.is_disabled());
        assert!(!controller.is_active());
    }

    #[test]
    fn web_only_shell_rejects_missing_durable_secrets_without_minting() {
        let mut config = RemoteHostConfig::default();
        config.enabled = true;
        config.web.enabled = true;
        config.web.pairing_token.clear();
        let error = RemoteHostService::new_web_only(config)
            .err()
            .expect("missing pairing");
        assert!(
            error.contains("pairing"),
            "expected durable pairing fail-closed, got {error}"
        );
    }

    #[test]
    fn web_only_shell_preserves_persisted_native_enabled_flag() {
        let mut config = RemoteHostConfig::default();
        config.enabled = true;
        config.web.enabled = false;
        // Disabled web skips construction via controller; exercise shell directly.
        let mut enabled_web = config.clone();
        enabled_web.web.enabled = true;
        // Default WebConfig already has durable-shaped secrets from Default.
        let service = RemoteHostService::new_web_only(enabled_web).expect("web only");
        assert!(service.web_only_execution());
        assert!(service.config().enabled);
        assert!(!service.web_listener_is_installed());
    }

    #[test]
    fn injected_worker_ready_handshake_then_shutdown_joins() {
        let ready_seen = Arc::new(AtomicBool::new(false));
        let shutdown_ran = Arc::new(AtomicBool::new(false));
        let ready_flag = Arc::clone(&ready_seen);
        let shutdown_flag = Arc::clone(&shutdown_ran);

        let controller = block_on(HostRemoteAccessController::start_owned_worker(
            move |shutdown_rx, ready_tx| {
                ready_flag.store(true, Ordering::SeqCst);
                let _ = ready_tx.send(Ok(()));
                let _ = shutdown_rx.recv();
                shutdown_flag.store(true, Ordering::SeqCst);
                Ok(())
            },
        ))
        .expect("injected start");
        assert!(controller.is_active());
        assert!(ready_seen.load(Ordering::SeqCst));

        block_on(controller.shutdown()).expect("shutdown joins");
        assert!(shutdown_ran.load(Ordering::SeqCst));
    }

    #[test]
    fn drop_after_thread_start_signals_and_joins_owned_worker() {
        let shutdown_ran = Arc::new(AtomicBool::new(false));
        let shutdown_flag = Arc::clone(&shutdown_ran);
        let controller = block_on(HostRemoteAccessController::start_owned_worker(
            move |shutdown_rx, ready_tx| {
                let _ = ready_tx.send(Ok(()));
                let _ = shutdown_rx.recv();
                shutdown_flag.store(true, Ordering::SeqCst);
                Ok(())
            },
        ))
        .expect("start");
        drop(controller);
        assert!(shutdown_ran.load(Ordering::SeqCst));
    }

    #[test]
    fn delayed_shutdown_waits_for_owned_worker_without_detach() {
        let finished = Arc::new(AtomicBool::new(false));
        let finished_flag = Arc::clone(&finished);
        let controller = block_on(HostRemoteAccessController::start_owned_worker(
            move |shutdown_rx, ready_tx| {
                let _ = ready_tx.send(Ok(()));
                let _ = shutdown_rx.recv();
                thread::sleep(Duration::from_millis(50));
                finished_flag.store(true, Ordering::SeqCst);
                Ok(())
            },
        ))
        .expect("start");

        let started = Instant::now();
        block_on(controller.shutdown()).expect("delayed join");
        assert!(finished.load(Ordering::SeqCst));
        assert!(started.elapsed() >= Duration::from_millis(50));
    }

    #[test]
    fn ready_failure_shuts_down_worker_and_surfaces_error() {
        let worker_exited = Arc::new(AtomicBool::new(false));
        let exited = Arc::clone(&worker_exited);
        let error = block_on(HostRemoteAccessController::start_owned_worker(
            move |_shutdown_rx, ready_tx| {
                let _ = ready_tx.send(Err("bind failed".to_string()));
                exited.store(true, Ordering::SeqCst);
                Err("bind failed".to_string())
            },
        ))
        .err()
        .expect("ready failure");
        assert!(matches!(error, HostRemoteAccessError::WorkerStart(_)));
        assert!(worker_exited.load(Ordering::SeqCst));
    }

    #[test]
    fn shutdown_before_fullquit_arm_helper_requires_clean_join() {
        let mut remote_access = Some(
            block_on(HostRemoteAccessController::start_owned_worker(
                move |shutdown_rx, ready_tx| {
                    let _ = ready_tx.send(Ok(()));
                    let _ = shutdown_rx.recv();
                    Ok(())
                },
            ))
            .expect("start"),
        );
        block_on(shutdown_remote_access_before_fullquit_arm(
            &mut remote_access,
        ))
        .expect("arm seam");
        assert!(remote_access.is_none());

        let mut remote_access = Some(
            block_on(HostRemoteAccessController::start_owned_worker(
                move |shutdown_rx, ready_tx| {
                    let _ = ready_tx.send(Ok(()));
                    let _ = shutdown_rx.recv();
                    Err("listener runtime still draining".to_string())
                },
            ))
            .expect("start"),
        );
        let error = block_on(shutdown_remote_access_before_fullquit_arm(
            &mut remote_access,
        ))
        .expect_err("uncertain shutdown");
        assert!(matches!(error, HostRemoteAccessError::ShutdownUncertain(_)));
        assert!(remote_access.is_none());
    }

    #[test]
    fn fake_listener_shutdown_closure_runs_on_owned_worker() {
        let shutdown_steps = Arc::new(AtomicUsize::new(0));
        let steps = Arc::clone(&shutdown_steps);
        let controller = block_on(HostRemoteAccessController::start_owned_worker(
            move |shutdown_rx, ready_tx| {
                let _ = ready_tx.send(Ok(()));
                let _ = shutdown_rx.recv();
                // Fake direct WebListenerHandle::shutdown on the OS worker.
                steps.fetch_add(1, Ordering::SeqCst);
                thread::sleep(Duration::from_millis(5));
                steps.fetch_add(1, Ordering::SeqCst);
                Ok(())
            },
        ))
        .expect("start");
        block_on(controller.shutdown()).expect("join after fake listener shutdown");
        assert_eq!(shutdown_steps.load(Ordering::SeqCst), 2);
    }
}
