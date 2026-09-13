//! Trusted remote-PC roster/enroll/restore/forget helpers for the native shell.
//!
//! Child of [`super`] via `#[path = "native_trusted_hosts.rs"] mod trusted_hosts;`.
//! Not a second transport: reuses [`RemoteTrustStore`], [`HostFleet`], and
//! [`NativeHostClientRuntime::attach_installed`]. Brand-new installs transfer
//! fleet-slot ownership into the real runtime (`owns_fleet_slot = true`);
//! preexisting slots stay non-owning.

use std::fmt;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::client::{
    connect_trusted_host, forget_trusted_host, list_trusted_hosts, pair_enroll_and_connect,
    ConnectTrustedOptions, FleetError, FleetRemoval, HostClient, HostClientFactory, HostFleet,
    HostId, PairEnrollRequest, RemoteTrustError, RemoteTrustStore, TrustedHostRecord,
};
use crate::config::paths::{resolve_app_paths, AppProfile, BuildKind};
use crate::host::IpcError;
use crate::remote::blocking_work::{RemoteBlockingWork, RemoteWorkAdmission, RemoteWorkError};

use super::{
    spawn_pending_host_bootstrap, IsolatedDevProfile, NativeHostBootstrap, NativeHostClientRuntime,
    NativeHostRuntimeAttachment, NativeShellError, NativeShellMode, NativeShutdownDeadline,
    PendingHostBootstrap, NATIVE_SHUTDOWN_BUDGET, NATIVE_STARTUP_BUDGET,
};

pub(crate) const MAX_TRUSTED_REMOTE_HOSTS: usize = 15;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum TrustedHostsError {
    ProfileRoot,
    Trust(RemoteTrustError),
    Fleet(FleetBusyKind),
    Shell,
    Busy,
    RecoveryRequired,
    HostOccupied,
    Capacity,
    Cancelled,
    Deadline,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FleetBusyKind {
    Busy,
    NotFound,
    AlreadyInstalled,
    Capacity,
    StaleGeneration,
    Other,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RecoveryReason {
    /// Enroll/forget disk mutation may still be settling; block new setup/forget.
    PersistenceUncertain,
    /// Fleet removal was admitted but physical join did not settle under the coordinator.
    RemovalIncomplete,
    /// Construction cleanup (exact generation remove) did not settle; never claim success.
    CleanupIncomplete,
}

impl fmt::Debug for TrustedHostsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ProfileRoot => f.write_str("TrustedHostsError::ProfileRoot"),
            Self::Trust(e) => f.debug_tuple("TrustedHostsError::Trust").field(e).finish(),
            Self::Fleet(k) => f.debug_tuple("TrustedHostsError::Fleet").field(k).finish(),
            Self::Shell => f.write_str("TrustedHostsError::Shell"),
            Self::Busy => f.write_str("TrustedHostsError::Busy"),
            Self::RecoveryRequired => f.write_str("TrustedHostsError::RecoveryRequired"),
            Self::HostOccupied => f.write_str("TrustedHostsError::HostOccupied"),
            Self::Capacity => f.write_str("TrustedHostsError::Capacity"),
            Self::Cancelled => f.write_str("TrustedHostsError::Cancelled"),
            Self::Deadline => f.write_str("TrustedHostsError::Deadline"),
        }
    }
}

impl From<RemoteTrustError> for TrustedHostsError {
    fn from(value: RemoteTrustError) -> Self {
        Self::Trust(value)
    }
}

impl TrustedHostsError {
    fn from_fleet(error: FleetError) -> Self {
        match error {
            FleetError::HostBusy => Self::Fleet(FleetBusyKind::Busy),
            FleetError::HostNotFound => Self::Fleet(FleetBusyKind::NotFound),
            FleetError::HostAlreadyInstalled => Self::HostOccupied,
            FleetError::HostCapacityExceeded => Self::Capacity,
            FleetError::StaleGeneration | FleetError::StaleReservation => {
                Self::Fleet(FleetBusyKind::StaleGeneration)
            }
            _ => Self::Fleet(FleetBusyKind::Other),
        }
    }

    fn into_shell(self) -> NativeShellError {
        NativeShellError::HostConnect {
            message: match self {
                Self::ProfileRoot => "trusted host profile root unresolved".into(),
                Self::Trust(e) => e.as_str().into(),
                Self::Fleet(k) => format!("trusted host fleet: {k:?}"),
                Self::Shell => "trusted host attach failed".into(),
                Self::Busy => "trusted host coordinator busy".into(),
                Self::RecoveryRequired => {
                    "trusted host recovery required before further mutations".into()
                }
                Self::HostOccupied => "trusted host already installed".into(),
                Self::Capacity => "trusted host fleet capacity exceeded".into(),
                Self::Cancelled => "trusted host operation cancelled".into(),
                Self::Deadline => "trusted host absolute deadline expired".into(),
            },
        }
    }
}

fn remaining_until(deadline: Instant) -> Result<Duration, TrustedHostsError> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        Err(TrustedHostsError::Deadline)
    } else {
        Ok(remaining)
    }
}

fn clamp_to_deadline(
    requested: Duration,
    deadline: Instant,
) -> Result<Duration, TrustedHostsError> {
    Ok(requested.min(remaining_until(deadline)?))
}

pub(crate) fn resolve_isolated_trust_store_root(
    host_config_base: &Path,
    named_profile: &str,
) -> Result<PathBuf, TrustedHostsError> {
    let named = AppProfile::named(named_profile).map_err(|_| TrustedHostsError::ProfileRoot)?;
    let paths = resolve_app_paths(host_config_base, named, BuildKind::Debug)
        .map_err(|_| TrustedHostsError::ProfileRoot)?;
    if !paths.root.is_absolute() {
        return Err(TrustedHostsError::ProfileRoot);
    }
    Ok(paths.root)
}

pub(crate) fn trust_store_root_for_profile(
    profile: &IsolatedDevProfile,
) -> Result<PathBuf, TrustedHostsError> {
    match profile.mode() {
        NativeShellMode::Production => {
            let root = profile.root().to_path_buf();
            if !root.is_absolute() {
                return Err(TrustedHostsError::ProfileRoot);
            }
            Ok(root)
        }
        NativeShellMode::IsolatedDebug => {
            resolve_isolated_trust_store_root(profile.host_config_base(), profile.named_profile())
        }
    }
}

fn open_trust_store_blocking(
    explicit_profile_root: PathBuf,
    deadline: Instant,
) -> Result<RemoteTrustStore, TrustedHostsError> {
    let mut job = RemoteBlockingWork::spawn(
        "native-trusted-hosts-open-store",
        deadline,
        move |admission: RemoteWorkAdmission| {
            if admission.cancellation_requested() || !admission.try_admit() {
                return Err(RemoteTrustError::Cancelled);
            }
            RemoteTrustStore::open(explicit_profile_root)
        },
    )
    .map_err(map_remote_work)?;
    match job.wait_blocking() {
        Ok(Ok(store)) => {
            remaining_until(deadline)?;
            Ok(store)
        }
        Ok(Err(error)) => Err(TrustedHostsError::Trust(error)),
        Err(error) => Err(map_remote_work(error)),
    }
}

fn map_remote_work(error: RemoteWorkError) -> TrustedHostsError {
    match error {
        RemoteWorkError::Unavailable => TrustedHostsError::Trust(RemoteTrustError::Unavailable),
        RemoteWorkError::Deadline { admitted: true } => {
            TrustedHostsError::Trust(RemoteTrustError::Timeout)
        }
        RemoteWorkError::Deadline { admitted: false } => TrustedHostsError::Cancelled,
    }
}

fn map_trust_ipc(error: RemoteTrustError) -> IpcError {
    match error {
        RemoteTrustError::Unauthorized | RemoteTrustError::PinChanged => IpcError::Unauthorized,
        RemoteTrustError::Timeout => IpcError::Timeout,
        RemoteTrustError::Cancelled => IpcError::Busy,
        RemoteTrustError::Unsupported => IpcError::Unsupported,
        _ => IpcError::Unavailable,
    }
}

/// Construction-only guard: lives on the bootstrap OS thread until attach owns the slot.
struct ExactRemoteInstallGuard {
    coordinator: Arc<TrustedHostsCoordinator>,
    fleet: Arc<HostFleet>,
    host_id: HostId,
    generation: u64,
    runtime: Arc<tokio::runtime::Runtime>,
    disarmed: bool,
}

impl ExactRemoteInstallGuard {
    fn disarm(&mut self) {
        self.disarmed = true;
    }
}

impl Drop for ExactRemoteInstallGuard {
    fn drop(&mut self) {
        if self.disarmed {
            return;
        }
        let remaining = NativeShutdownDeadline::from_now(NATIVE_SHUTDOWN_BUDGET).remaining();
        if remaining.is_zero() {
            // Budget already gone: fleet may still drain the worker; do not claim cleanup done.
            self.coordinator
                .enter_recovery(RecoveryReason::CleanupIncomplete);
            return;
        }
        // Bootstrap OS thread only — never entered on a Tokio worker.
        let fleet = Arc::clone(&self.fleet);
        let host_id = self.host_id.clone();
        let generation = self.generation;
        let settled = self.runtime.block_on(async move {
            match tokio::time::timeout(remaining, fleet.remove_at_generation(&host_id, generation))
                .await
            {
                Ok(Ok(removal)) if removal.generation == generation => true,
                // Stale generation means a newer install owns the slot — never remove it.
                Ok(Err(FleetError::StaleGeneration | FleetError::StaleReservation)) => false,
                Ok(Ok(_)) | Ok(Err(_)) | Err(_) => false,
            }
        });
        if !settled {
            self.coordinator
                .enter_recovery(RecoveryReason::CleanupIncomplete);
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ForgetPersistence {
    Forgotten,
    PersistenceUncertain,
    DefinitelyPreserved,
}

#[derive(Debug)]
pub(crate) struct ForgetTrustedHostResult {
    pub removal: Option<FleetRemoval>,
    pub persistence: ForgetPersistence,
    pub persist_error: Option<RemoteTrustError>,
}

#[derive(Debug)]
pub(crate) enum RosterLoadResult {
    Listed(Vec<TrustedHostRecord>),
    Failed(RemoteTrustError),
}

pub(crate) async fn load_trusted_host_roster_until(
    store: &RemoteTrustStore,
    deadline: Instant,
) -> RosterLoadResult {
    let Ok(budget) = remaining_until(deadline) else {
        return RosterLoadResult::Failed(RemoteTrustError::Timeout);
    };
    match list_trusted_hosts(store, budget).await {
        Ok(records) if records.len() > MAX_TRUSTED_REMOTE_HOSTS => {
            RosterLoadResult::Failed(RemoteTrustError::Corrupt)
        }
        Ok(_) if Instant::now() >= deadline => RosterLoadResult::Failed(RemoteTrustError::Timeout),
        Ok(records) => RosterLoadResult::Listed(records),
        Err(error) => RosterLoadResult::Failed(error),
    }
}

pub(crate) fn trusted_remote_reconnect_factory(
    store_root: PathBuf,
    host_public_id: [u8; 16],
    mut options: ConnectTrustedOptions,
    deadline: Instant,
) -> HostClientFactory {
    Box::new(move || {
        Box::pin(async move {
            let remaining = deadline
                .saturating_duration_since(Instant::now())
                .max(Duration::from_millis(1));
            let mut job = RemoteBlockingWork::spawn(
                "native-trusted-reconnect-open",
                deadline,
                move |admission: RemoteWorkAdmission| {
                    if !admission.try_admit() {
                        return Err(RemoteTrustError::Cancelled);
                    }
                    RemoteTrustStore::open(store_root)
                },
            )
            .map_err(|_| IpcError::Unavailable)?;
            let store = match job.wait().await {
                Ok(Ok(store)) => store,
                Ok(Err(error)) => return Err(map_trust_ipc(error)),
                Err(_) => return Err(IpcError::Timeout),
            };
            options.deadline = options.deadline.min(
                deadline
                    .saturating_duration_since(Instant::now())
                    .min(remaining),
            );
            if options.deadline.is_zero() || Instant::now() >= deadline {
                return Err(IpcError::Timeout);
            }
            connect_trusted_host(&store, host_public_id, options)
                .await
                .map_err(map_trust_ipc)
        }) as Pin<Box<dyn Future<Output = Result<HostClient, IpcError>> + Send>>
    })
}

pub(crate) async fn synchronize_trusted_remote(
    fleet: &HostFleet,
    host_id: &HostId,
) -> Result<(), TrustedHostsError> {
    let admission = fleet
        .admit_host(host_id)
        .map_err(TrustedHostsError::from_fleet)?;
    let owned = fleet
        .synchronize(host_id)
        .await
        .map_err(TrustedHostsError::from_fleet)?;
    if owned.host != admission.host
        || owned.generation != admission.generation
        || owned.client_id != admission.client_id
    {
        return Err(TrustedHostsError::Fleet(FleetBusyKind::StaleGeneration));
    }
    Ok(())
}

fn remote_host_id(host_public_id: [u8; 16]) -> Result<HostId, TrustedHostsError> {
    HostId::remote(host_public_id).map_err(TrustedHostsError::from_fleet)
}

fn install_attach_new_remote(
    profile: &IsolatedDevProfile,
    fleet: Arc<HostFleet>,
    coordinator: Arc<TrustedHostsCoordinator>,
    runtime: Arc<tokio::runtime::Runtime>,
    client: HostClient,
    record: TrustedHostRecord,
    deadline: Instant,
    cancelled: &AtomicBool,
) -> Result<(NativeHostClientRuntime, TrustedBootstrapSuccess), TrustedHostsError> {
    remaining_until(deadline)?;
    if cancelled.load(Ordering::Acquire) {
        return Err(TrustedHostsError::Cancelled);
    }
    let host_id = remote_host_id(record.host_public_id)?;
    if fleet.contains(&host_id) {
        return Err(TrustedHostsError::HostOccupied);
    }
    let generation = fleet
        .install(host_id.clone(), client)
        .map_err(TrustedHostsError::from_fleet)?;
    let mut cleanup = ExactRemoteInstallGuard {
        coordinator,
        fleet: Arc::clone(&fleet),
        host_id: host_id.clone(),
        generation,
        runtime: Arc::clone(&runtime),
        disarmed: false,
    };
    remaining_until(deadline)?;
    if cancelled.load(Ordering::Acquire) {
        drop(cleanup);
        return Err(TrustedHostsError::Cancelled);
    }
    let mut attached =
        NativeHostClientRuntime::attach_installed(profile, fleet, host_id.clone(), Some(runtime))
            .map_err(|_| TrustedHostsError::Shell)?;
    // Validate before transferring construction cleanup custody into the runtime.
    if remaining_until(deadline).is_err() || cancelled.load(Ordering::Acquire) {
        attached.owns_fleet_slot = false;
        drop(attached);
        drop(cleanup);
        return Err(TrustedHostsError::Cancelled);
    }
    // Brand-new install only: transfer slot custody into the real runtime.
    attached.owns_fleet_slot = true;
    cleanup.disarm();
    Ok((
        attached,
        TrustedBootstrapSuccess {
            host_id,
            generation,
            record,
        },
    ))
}

#[derive(Debug)]
enum CoordinatorState {
    Idle,
    Setup { op_id: u64, cancel: Arc<AtomicBool> },
    Forget { op_id: u64 },
    RecoveryRequired { reason: RecoveryReason },
}

/// Serial trust-mutation coordinator: one setup XOR forget, or recovery hold.
pub(crate) struct TrustedHostsCoordinator {
    next_op: AtomicU64,
    state: Mutex<CoordinatorState>,
}

/// RAII setup admission; released when the bootstrap object drops after handoff.
struct SetupAdmissionGuard {
    coordinator: Arc<TrustedHostsCoordinator>,
    op_id: u64,
    cancel: Arc<AtomicBool>,
}

impl SetupAdmissionGuard {
    fn cancel_flag(&self) -> &Arc<AtomicBool> {
        &self.cancel
    }
}

impl Drop for SetupAdmissionGuard {
    fn drop(&mut self) {
        self.coordinator.release_setup(self.op_id);
    }
}

enum ForgetDiskStage {
    /// No fleet removal admitted yet (or join already settled). Drop → Idle.
    PreDisk,
    /// Registry remove admitted; physical join may still be in flight. Drop → RemovalIncomplete.
    RemovalMaybeAdmitted,
    /// Durable trust delete may have been admitted. Drop → PersistenceUncertain.
    DiskMaybeAdmitted,
}

struct ForgetAdmissionGuard {
    coordinator: Arc<TrustedHostsCoordinator>,
    op_id: u64,
    stage: ForgetDiskStage,
}

impl ForgetAdmissionGuard {
    fn mark_removal_maybe_admitted(&mut self) {
        self.stage = ForgetDiskStage::RemovalMaybeAdmitted;
    }

    fn mark_disk_maybe_admitted(&mut self) {
        self.stage = ForgetDiskStage::DiskMaybeAdmitted;
    }

    fn mark_settled_idle(&mut self) {
        self.stage = ForgetDiskStage::PreDisk;
    }
}

impl Drop for ForgetAdmissionGuard {
    fn drop(&mut self) {
        self.coordinator.release_forget(self.op_id, &self.stage);
    }
}

impl Default for TrustedHostsCoordinator {
    fn default() -> Self {
        Self {
            next_op: AtomicU64::new(1),
            state: Mutex::new(CoordinatorState::Idle),
        }
    }
}

impl TrustedHostsCoordinator {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn recovery_reason(&self) -> Option<RecoveryReason> {
        match &*self.state.lock().unwrap_or_else(|e| e.into_inner()) {
            CoordinatorState::RecoveryRequired { reason } => Some(*reason),
            _ => None,
        }
    }

    fn alloc_op(&self) -> u64 {
        self.next_op.fetch_add(1, Ordering::Relaxed)
    }

    fn begin_setup(self: &Arc<Self>) -> Result<SetupAdmissionGuard, TrustedHostsError> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        match &*state {
            CoordinatorState::Idle => {}
            CoordinatorState::RecoveryRequired { .. } => {
                return Err(TrustedHostsError::RecoveryRequired);
            }
            CoordinatorState::Setup { .. } | CoordinatorState::Forget { .. } => {
                return Err(TrustedHostsError::Busy);
            }
        }
        let op_id = self.alloc_op();
        let cancel = Arc::new(AtomicBool::new(false));
        *state = CoordinatorState::Setup {
            op_id,
            cancel: Arc::clone(&cancel),
        };
        Ok(SetupAdmissionGuard {
            coordinator: Arc::clone(self),
            op_id,
            cancel,
        })
    }

    fn release_setup(&self, op_id: u64) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        match &*state {
            CoordinatorState::Setup { op_id: current, .. } if *current == op_id => {
                *state = CoordinatorState::Idle;
            }
            CoordinatorState::RecoveryRequired { .. } => {}
            _ => {}
        }
    }

    fn try_begin_forget(self: &Arc<Self>) -> Result<ForgetAdmissionGuard, TrustedHostsError> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        match &*state {
            CoordinatorState::Idle => {}
            CoordinatorState::RecoveryRequired { .. } => {
                return Err(TrustedHostsError::RecoveryRequired);
            }
            CoordinatorState::Forget { .. } => return Err(TrustedHostsError::Busy),
            CoordinatorState::Setup { cancel, .. } => {
                cancel.store(true, Ordering::Release);
                // Parent must drop/join that pending bootstrap, then retry.
                return Err(TrustedHostsError::Busy);
            }
        }
        let op_id = self.alloc_op();
        *state = CoordinatorState::Forget { op_id };
        Ok(ForgetAdmissionGuard {
            coordinator: Arc::clone(self),
            op_id,
            stage: ForgetDiskStage::PreDisk,
        })
    }

    fn release_forget(&self, op_id: u64, stage: &ForgetDiskStage) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        match &*state {
            CoordinatorState::Forget { op_id: current } if *current == op_id => {
                *state = match stage {
                    ForgetDiskStage::PreDisk => CoordinatorState::Idle,
                    ForgetDiskStage::RemovalMaybeAdmitted => CoordinatorState::RecoveryRequired {
                        reason: RecoveryReason::RemovalIncomplete,
                    },
                    ForgetDiskStage::DiskMaybeAdmitted => CoordinatorState::RecoveryRequired {
                        reason: RecoveryReason::PersistenceUncertain,
                    },
                };
            }
            CoordinatorState::RecoveryRequired { .. } => {}
            _ => {}
        }
    }

    pub(crate) fn enter_recovery(&self, reason: RecoveryReason) {
        *self.state.lock().unwrap_or_else(|e| e.into_inner()) =
            CoordinatorState::RecoveryRequired { reason };
    }

    /// Parent must dispose/join any pending or attached runtime for this host
    /// before retrying forget after Busy.
    pub(crate) async fn forget_exact(
        self: &Arc<Self>,
        fleet: &HostFleet,
        store: &RemoteTrustStore,
        expected: TrustedHostRecord,
        deadline: Instant,
    ) -> Result<ForgetTrustedHostResult, TrustedHostsError> {
        let mut admission = self.try_begin_forget()?;
        let host_id = remote_host_id(expected.host_public_id)?;
        let removal = if fleet.contains(&host_id) {
            let generation = fleet
                .generation(&host_id)
                .map_err(TrustedHostsError::from_fleet)?;
            let client_id = fleet
                .client_id(&host_id)
                .map_err(TrustedHostsError::from_fleet)?;
            if client_id != expected.assigned_client_id {
                return Err(TrustedHostsError::Fleet(FleetBusyKind::StaleGeneration));
            }
            // Hold before await: cancel during physical join must not return to Idle
            // while a same-host install could reuse capacity with a draining prior gen.
            admission.mark_removal_maybe_admitted();
            let removed = match fleet.remove_at_generation(&host_id, generation).await {
                Ok(removed) => removed,
                Err(error) => {
                    // Stage stays RemovalMaybeAdmitted so Drop enters RemovalIncomplete
                    // unless the caller already upgraded recovery.
                    return Err(TrustedHostsError::from_fleet(error));
                }
            };
            if removed.generation != generation {
                return Err(TrustedHostsError::Fleet(FleetBusyKind::StaleGeneration));
            }
            // Physical join settled for this generation; disk phase has not started.
            admission.mark_settled_idle();
            Some(removed)
        } else {
            None
        };

        // Keep fleet removal even if the disk budget has expired.
        let disk_budget = match remaining_until(deadline) {
            Ok(budget) => budget,
            Err(_) => {
                admission.mark_settled_idle();
                return Ok(ForgetTrustedHostResult {
                    removal,
                    persistence: ForgetPersistence::DefinitelyPreserved,
                    persist_error: Some(RemoteTrustError::Timeout),
                });
            }
        };
        admission.mark_disk_maybe_admitted();
        match forget_trusted_host(store, expected, disk_budget).await {
            Ok(()) => {
                admission.mark_settled_idle();
                Ok(ForgetTrustedHostResult {
                    removal,
                    persistence: ForgetPersistence::Forgotten,
                    persist_error: None,
                })
            }
            Err(RemoteTrustError::PersistenceUncertain) => {
                self.enter_recovery(RecoveryReason::PersistenceUncertain);
                Ok(ForgetTrustedHostResult {
                    removal,
                    persistence: ForgetPersistence::PersistenceUncertain,
                    persist_error: Some(RemoteTrustError::PersistenceUncertain),
                })
            }
            Err(error) => {
                admission.mark_settled_idle();
                Ok(ForgetTrustedHostResult {
                    removal,
                    persistence: ForgetPersistence::DefinitelyPreserved,
                    persist_error: Some(error),
                })
            }
        }
    }

    pub(crate) fn spawn_enroll(
        self: &Arc<Self>,
        profile: IsolatedDevProfile,
        fleet: Arc<HostFleet>,
        store_root: PathBuf,
        request: PairEnrollRequest,
        deadline: Instant,
    ) -> Result<(PendingHostBootstrap, Arc<TrustedBootstrapOutcomeSlot>), NativeShellError> {
        let setup = self.begin_setup().map_err(TrustedHostsError::into_shell)?;
        let outcome = Arc::new(TrustedBootstrapOutcomeSlot::default());
        let pending = spawn_pending_host_bootstrap(
            profile,
            TrustedHostAttachBootstrap {
                setup,
                fleet,
                store_root,
                kind: Some(TrustedBootstrapKind::Enroll { request }),
                outcome: Arc::clone(&outcome),
                absolute_deadline: deadline,
            },
        )?;
        Ok((pending, outcome))
    }

    pub(crate) fn spawn_restore(
        self: &Arc<Self>,
        profile: IsolatedDevProfile,
        fleet: Arc<HostFleet>,
        store_root: PathBuf,
        host_public_id: [u8; 16],
        options: ConnectTrustedOptions,
        deadline: Instant,
    ) -> Result<(PendingHostBootstrap, Arc<TrustedBootstrapOutcomeSlot>), NativeShellError> {
        let setup = self.begin_setup().map_err(TrustedHostsError::into_shell)?;
        let outcome = Arc::new(TrustedBootstrapOutcomeSlot::default());
        let pending = spawn_pending_host_bootstrap(
            profile,
            TrustedHostAttachBootstrap {
                setup,
                fleet,
                store_root,
                kind: Some(TrustedBootstrapKind::Restore {
                    host_public_id,
                    options,
                }),
                outcome: Arc::clone(&outcome),
                absolute_deadline: deadline,
            },
        )?;
        Ok((pending, outcome))
    }
}

enum TrustedBootstrapKind {
    Enroll {
        request: PairEnrollRequest,
    },
    Restore {
        host_public_id: [u8; 16],
        options: ConnectTrustedOptions,
    },
}

#[derive(Default)]
pub(crate) struct TrustedBootstrapOutcomeSlot {
    inner: Mutex<Option<TrustedBootstrapSuccess>>,
}

/// POD metadata only — no cleanup handles.
#[derive(Debug, Clone)]
pub(crate) struct TrustedBootstrapSuccess {
    pub host_id: HostId,
    pub generation: u64,
    pub record: TrustedHostRecord,
}

impl TrustedBootstrapOutcomeSlot {
    pub(crate) fn take(&self) -> Option<TrustedBootstrapSuccess> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).take()
    }

    fn publish(&self, success: TrustedBootstrapSuccess) {
        *self.inner.lock().unwrap_or_else(|e| e.into_inner()) = Some(success);
    }
}

struct TrustedHostAttachBootstrap {
    setup: SetupAdmissionGuard,
    fleet: Arc<HostFleet>,
    store_root: PathBuf,
    kind: Option<TrustedBootstrapKind>,
    outcome: Arc<TrustedBootstrapOutcomeSlot>,
    absolute_deadline: Instant,
}

impl NativeHostBootstrap for TrustedHostAttachBootstrap {
    fn start_until(
        &mut self,
        profile: &IsolatedDevProfile,
        bootstrap_deadline: Instant,
    ) -> Result<NativeHostRuntimeAttachment, NativeShellError> {
        let deadline = self.absolute_deadline.min(bootstrap_deadline);
        self.run_setup(profile, deadline)
            .map_err(TrustedHostsError::into_shell)
    }
}

impl TrustedHostAttachBootstrap {
    fn run_setup(
        &mut self,
        profile: &IsolatedDevProfile,
        deadline: Instant,
    ) -> Result<NativeHostRuntimeAttachment, TrustedHostsError> {
        let cancel = Arc::clone(self.setup.cancel_flag());
        if Instant::now() >= deadline || cancel.load(Ordering::Acquire) {
            return Err(TrustedHostsError::Cancelled);
        }
        let Some(kind) = self.kind.take() else {
            return Err(TrustedHostsError::Cancelled);
        };
        let runtime = Arc::new(
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(1)
                .enable_all()
                .build()
                .map_err(|_| TrustedHostsError::Shell)?,
        );
        let (attached, success) = match kind {
            TrustedBootstrapKind::Enroll { request } => {
                self.enroll(profile, Arc::clone(&runtime), request, deadline, &cancel)?
            }
            TrustedBootstrapKind::Restore {
                host_public_id,
                options,
            } => self.restore(
                profile,
                Arc::clone(&runtime),
                host_public_id,
                options,
                deadline,
                &cancel,
            )?,
        };
        remaining_until(deadline)?;
        if cancel.load(Ordering::Acquire) {
            // Owning runtime Drop removes the brand-new slot.
            drop(attached);
            return Err(TrustedHostsError::Cancelled);
        }
        self.outcome.publish(success);
        Ok(NativeHostRuntimeAttachment::Client(attached))
    }

    fn enroll(
        &self,
        profile: &IsolatedDevProfile,
        runtime: Arc<tokio::runtime::Runtime>,
        mut request: PairEnrollRequest,
        deadline: Instant,
        cancel: &AtomicBool,
    ) -> Result<(NativeHostClientRuntime, TrustedBootstrapSuccess), TrustedHostsError> {
        if cancel.load(Ordering::Acquire) {
            return Err(TrustedHostsError::Cancelled);
        }
        let store = open_trust_store_blocking(self.store_root.clone(), deadline)?;
        request.deadline = clamp_to_deadline(request.deadline, deadline)?;
        let pair = runtime.block_on(pair_enroll_and_connect(&store, request));
        let (client, record) = match pair {
            Ok(pair) => pair,
            Err(RemoteTrustError::PersistenceUncertain) => {
                self.setup
                    .coordinator
                    .enter_recovery(RecoveryReason::PersistenceUncertain);
                return Err(TrustedHostsError::Trust(
                    RemoteTrustError::PersistenceUncertain,
                ));
            }
            Err(error) => return Err(TrustedHostsError::Trust(error)),
        };
        remaining_until(deadline)?;
        if cancel.load(Ordering::Acquire) {
            drop(client);
            // Trust may already be durable; recovery is parent/restart concern if forget races.
            return Err(TrustedHostsError::Cancelled);
        }
        install_attach_new_remote(
            profile,
            Arc::clone(&self.fleet),
            Arc::clone(&self.setup.coordinator),
            runtime,
            client,
            record,
            deadline,
            cancel,
        )
    }

    fn restore(
        &self,
        profile: &IsolatedDevProfile,
        runtime: Arc<tokio::runtime::Runtime>,
        host_public_id: [u8; 16],
        mut options: ConnectTrustedOptions,
        deadline: Instant,
        cancel: &AtomicBool,
    ) -> Result<(NativeHostClientRuntime, TrustedBootstrapSuccess), TrustedHostsError> {
        if cancel.load(Ordering::Acquire) {
            return Err(TrustedHostsError::Cancelled);
        }
        let store = open_trust_store_blocking(self.store_root.clone(), deadline)?;
        options.deadline = clamp_to_deadline(options.deadline, deadline)?;
        let client = runtime
            .block_on(connect_trusted_host(
                &store,
                host_public_id,
                options.clone(),
            ))
            .map_err(TrustedHostsError::Trust)?;
        remaining_until(deadline)?;
        let list_budget = clamp_to_deadline(options.deadline, deadline)?;
        let record = runtime
            .block_on(async {
                let records = list_trusted_hosts(&store, list_budget).await?;
                records
                    .into_iter()
                    .find(|record| record.host_public_id == host_public_id)
                    .ok_or(RemoteTrustError::NotFound)
            })
            .map_err(TrustedHostsError::Trust)?;
        remaining_until(deadline)?;
        if cancel.load(Ordering::Acquire) {
            drop(client);
            return Err(TrustedHostsError::Cancelled);
        }
        install_attach_new_remote(
            profile,
            Arc::clone(&self.fleet),
            Arc::clone(&self.setup.coordinator),
            runtime,
            client,
            record,
            deadline,
            cancel,
        )
    }
}

pub(crate) fn default_trust_op_deadline() -> Instant {
    Instant::now() + NATIVE_STARTUP_BUDGET
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::{ClientConnection, HostClient, HostClientConfig};
    use crate::domain::ClientId;
    use crate::protocol::{
        Capability, CapabilitySet, FrameLimits, ProfileFingerprint, ServerHello, PROTOCOL_MAJOR,
        PROTOCOL_MINOR,
    };
    use std::collections::BTreeMap;
    use tempfile::tempdir;

    fn fixed_uuid_v7(tail: u8) -> [u8; 16] {
        [
            0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, tail,
        ]
    }

    fn owned_runtime() -> Arc<tokio::runtime::Runtime> {
        Arc::new(
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(1)
                .enable_all()
                .build()
                .expect("runtime"),
        )
    }

    fn local_client(profile: &str, client_tail: u8, connection_tail: u8) -> HostClient {
        let normalized = match AppProfile::named(profile).expect("profile") {
            AppProfile::Named(name) => name,
            other => panic!("expected named, got {other:?}"),
        };
        let client_id = ClientId::from_bytes(fixed_uuid_v7(client_tail)).expect("client");
        let hello = ServerHello {
            protocol_major: PROTOCOL_MAJOR,
            protocol_minor: PROTOCOL_MINOR,
            server_build: "devmanager-host/trusted-hosts-test".into(),
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
        };
        let stub = ClientConnection::inert_stub_for_test(client_id, hello.clone());
        HostClient::from_parts_for_test(
            HostClientConfig {
                named_profile: normalized,
                client_build: "devmanager/trusted-hosts-test".into(),
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
            BTreeMap::new(),
        )
    }

    #[test]
    fn isolated_trust_root_uses_tempfile_absolute_paths() {
        let base = tempdir().expect("temp");
        let named = "wt_example_profile";
        let resolved = resolve_isolated_trust_store_root(base.path(), named).expect("root");
        assert!(resolved.is_absolute());
        assert!(resolved.starts_with(base.path()));
        // Retain TempDir until assertions finish (no into_path leak).
        drop(base);
    }

    #[test]
    fn pair_request_debug_redacts_secrets() {
        let request = PairEnrollRequest {
            endpoint: "https://office.example:8443/".into(),
            pairing_code: zeroize::Zeroizing::new("super-secret-pair-code".into()),
            label: Some("Office".into()),
            additional_ca_pem: Some(
                "-----BEGIN CERTIFICATE-----\nMIIB\n-----END CERTIFICATE-----\n".into(),
            ),
            deadline: Duration::from_secs(5),
        };
        let rendered = format!("{request:?}");
        assert!(!rendered.contains("super-secret-pair-code"));
        assert!(!rendered.contains("MIIB"));
    }

    #[test]
    fn clamp_after_elapsed_deadline_fails() {
        let deadline = Instant::now() + Duration::from_millis(20);
        std::thread::sleep(Duration::from_millis(30));
        assert!(matches!(
            clamp_to_deadline(Duration::from_secs(5), deadline),
            Err(TrustedHostsError::Deadline)
        ));
    }

    #[test]
    fn concurrent_forget_while_setup_cancels_and_returns_busy() {
        let coordinator = Arc::new(TrustedHostsCoordinator::new());
        let setup = coordinator.begin_setup().expect("setup");
        let cancel = Arc::clone(setup.cancel_flag());
        assert!(matches!(
            coordinator.try_begin_forget(),
            Err(TrustedHostsError::Busy)
        ));
        assert!(cancel.load(Ordering::Acquire));
        drop(setup);
        let _forget = coordinator
            .try_begin_forget()
            .expect("idle after setup drop");
    }

    #[test]
    fn concurrent_second_forget_returns_busy() {
        let coordinator = Arc::new(TrustedHostsCoordinator::new());
        let first = coordinator.try_begin_forget().expect("forget");
        assert!(matches!(
            coordinator.try_begin_forget(),
            Err(TrustedHostsError::Busy)
        ));
        drop(first);
        let _second = coordinator.try_begin_forget().expect("retry");
    }

    #[test]
    fn forget_drop_pre_disk_returns_to_idle() {
        let coordinator = Arc::new(TrustedHostsCoordinator::new());
        let guard = coordinator.try_begin_forget().expect("forget");
        drop(guard); // PreDisk — no removal admitted
        assert!(coordinator.recovery_reason().is_none());
        let _ = coordinator.begin_setup().expect("setup after idle");
    }

    #[test]
    fn forget_drop_removal_maybe_admitted_enters_removal_incomplete() {
        let coordinator = Arc::new(TrustedHostsCoordinator::new());
        let mut guard = coordinator.try_begin_forget().expect("forget");
        guard.mark_removal_maybe_admitted();
        drop(guard);
        assert_eq!(
            coordinator.recovery_reason(),
            Some(RecoveryReason::RemovalIncomplete)
        );
        assert!(matches!(
            coordinator.begin_setup(),
            Err(TrustedHostsError::RecoveryRequired)
        ));
    }

    #[test]
    fn forget_drop_post_join_pre_disk_returns_idle_after_real_removal() {
        let name = "trust_forget_post_join";
        let host = HostId::local_profile(name).expect("host");
        let fleet = Arc::new(HostFleet::new());
        let runtime = owned_runtime();
        let generation = fleet
            .install(host.clone(), local_client(name, 0x91, 0x92))
            .expect("install");
        let coordinator = Arc::new(TrustedHostsCoordinator::new());
        let mut guard = coordinator.try_begin_forget().expect("forget");
        guard.mark_removal_maybe_admitted();
        let removed = runtime
            .block_on(fleet.remove_at_generation(&host, generation))
            .expect("remove");
        assert_eq!(removed.generation, generation);
        assert!(!fleet.contains(&host));
        guard.mark_settled_idle();
        drop(guard);
        assert!(coordinator.recovery_reason().is_none());
        let _ = coordinator
            .begin_setup()
            .expect("idle after joined removal");
    }

    #[test]
    fn forget_disk_stage_hold_blocks_setup() {
        let coordinator = Arc::new(TrustedHostsCoordinator::new());
        let mut guard = coordinator.try_begin_forget().expect("forget");
        guard.mark_disk_maybe_admitted();
        drop(guard);
        assert_eq!(
            coordinator.recovery_reason(),
            Some(RecoveryReason::PersistenceUncertain)
        );
        assert!(matches!(
            coordinator.begin_setup(),
            Err(TrustedHostsError::RecoveryRequired)
        ));
    }

    #[test]
    fn setup_guard_releases_only_on_drop() {
        let coordinator = Arc::new(TrustedHostsCoordinator::new());
        let setup = coordinator.begin_setup().expect("setup");
        assert!(matches!(
            coordinator.begin_setup(),
            Err(TrustedHostsError::Busy)
        ));
        drop(setup);
        let _ = coordinator.begin_setup().expect("after drop");
    }

    #[test]
    fn abandoned_exact_guard_removes_install_on_os_thread() {
        let name = "trust_guard_abandon";
        let host = HostId::local_profile(name).expect("host");
        let fleet = Arc::new(HostFleet::new());
        let runtime = owned_runtime();
        let coordinator = Arc::new(TrustedHostsCoordinator::new());
        let generation = fleet
            .install(host.clone(), local_client(name, 0x81, 0x82))
            .expect("install");
        // Plain OS/test thread + owned multi-thread runtime — no entered Tokio handle.
        assert!(tokio::runtime::Handle::try_current().is_err());
        drop(ExactRemoteInstallGuard {
            coordinator: Arc::clone(&coordinator),
            fleet: Arc::clone(&fleet),
            host_id: host.clone(),
            generation,
            runtime,
            disarmed: false,
        });
        assert!(!fleet.contains(&host));
        assert!(coordinator.recovery_reason().is_none());
    }

    #[test]
    fn exact_guard_failed_cleanup_enters_cleanup_incomplete() {
        let name = "trust_guard_cleanup_fail";
        let host = HostId::local_profile(name).expect("host");
        let fleet = Arc::new(HostFleet::new());
        let runtime = owned_runtime();
        let coordinator = Arc::new(TrustedHostsCoordinator::new());
        let generation = fleet
            .install(host.clone(), local_client(name, 0x85, 0x86))
            .expect("install");
        // Settle the exact generation first so the armed guard cannot claim cleanup.
        let _ = runtime
            .block_on(fleet.remove_at_generation(&host, generation))
            .expect("pre-remove");
        assert!(!fleet.contains(&host));
        drop(ExactRemoteInstallGuard {
            coordinator: Arc::clone(&coordinator),
            fleet: Arc::clone(&fleet),
            host_id: host,
            generation,
            runtime,
            disarmed: false,
        });
        assert_eq!(
            coordinator.recovery_reason(),
            Some(RecoveryReason::CleanupIncomplete)
        );
    }

    #[test]
    fn disarmed_exact_guard_retains_install() {
        let name = "trust_guard_disarm";
        let host = HostId::local_profile(name).expect("host");
        let fleet = Arc::new(HostFleet::new());
        let runtime = owned_runtime();
        let coordinator = Arc::new(TrustedHostsCoordinator::new());
        let generation = fleet
            .install(host.clone(), local_client(name, 0x83, 0x84))
            .expect("install");
        let mut guard = ExactRemoteInstallGuard {
            coordinator: Arc::clone(&coordinator),
            fleet: Arc::clone(&fleet),
            host_id: host.clone(),
            generation,
            runtime: Arc::clone(&runtime),
            disarmed: false,
        };
        guard.disarm();
        drop(guard);
        assert!(fleet.contains(&host));
        assert_eq!(fleet.generation(&host).expect("gen"), generation);
        assert!(coordinator.recovery_reason().is_none());
        let _ = runtime.block_on(fleet.remove_at_generation(&host, generation));
    }

    #[test]
    fn outcome_slot_is_pod_only() {
        let slot = TrustedBootstrapOutcomeSlot::default();
        let pin = crate::connect::ConnectNoiseStaticPublicKey::from_bytes([5u8; 32]).expect("pin");
        slot.publish(TrustedBootstrapSuccess {
            host_id: HostId::remote([2u8; 16]).expect("remote"),
            generation: 3,
            record: TrustedHostRecord {
                host_public_id: [2u8; 16],
                host_key_pin: pin,
                endpoint: "https://a.example/".into(),
                connect_path: "/connect".into(),
                assigned_client_id: ClientId::new(),
                additional_ca_pem: None,
            },
        });
        let taken = slot.take().expect("pod");
        assert_eq!(taken.generation, 3);
        assert!(slot.take().is_none());
    }
}
