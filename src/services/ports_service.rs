use crate::models::{AppConfig, PortConflict, PortConflictEntry, PortStatus};
use crate::process::ports::{
    ListenerIdentity, PortInventorySnapshot, PortObservation, PortObservationIssue, PortScanError,
    PortStartError, ScanCancellation, TcpEndpoint, TcpEndpointRecord, MAX_ENDPOINTS_PER_SCAN,
    MAX_PORTS_PER_SCAN, MAX_SCAN_ERRORS, MAX_SCAN_WAITERS,
};
use crate::services::platform_service;
use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::Weak;
use std::sync::{Arc, Condvar, Mutex, RwLock};
use std::thread;
use std::time::{Duration, Instant};

pub const DEFAULT_PORT_SCAN_TIMEOUT: Duration = Duration::from_secs(2);

pub trait PortScanner: Send + Sync + 'static {
    fn scan(
        &self,
        ports: &[u16],
        cancellation: &ScanCancellation,
    ) -> Result<PortInventorySnapshot, String>;
}

impl<F> PortScanner for F
where
    F: Fn(&[u16], &ScanCancellation) -> Result<PortInventorySnapshot, String>
        + Send
        + Sync
        + 'static,
{
    fn scan(
        &self,
        ports: &[u16],
        cancellation: &ScanCancellation,
    ) -> Result<PortInventorySnapshot, String> {
        self(ports, cancellation)
    }
}

#[derive(Debug)]
struct ScanWaiter {
    result: Mutex<Option<Result<Arc<PortInventorySnapshot>, PortScanError>>>,
    ready: Condvar,
}

impl ScanWaiter {
    fn new() -> Self {
        Self {
            result: Mutex::new(None),
            ready: Condvar::new(),
        }
    }

    fn complete(&self, result: Result<Arc<PortInventorySnapshot>, PortScanError>) {
        let mut slot = self.result.lock().expect("port scan waiter lock");
        if slot.is_none() {
            *slot = Some(result);
            self.ready.notify_all();
        }
    }
}

#[derive(Debug, Clone)]
pub struct PortScanRequest {
    waiter: Arc<ScanWaiter>,
}

impl PortScanRequest {
    pub fn wait(&self, timeout: Duration) -> Result<Arc<PortInventorySnapshot>, PortScanError> {
        let deadline = Instant::now()
            .checked_add(timeout)
            .unwrap_or_else(Instant::now);
        let mut result = self.waiter.result.lock().expect("port scan waiter lock");
        loop {
            if let Some(result) = result.take() {
                return result;
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(PortScanError::TimedOut);
            }
            let (next, timeout_result) = self
                .waiter
                .ready
                .wait_timeout(result, remaining)
                .expect("port scan waiter condition variable");
            result = next;
            if timeout_result.timed_out() && result.is_none() {
                return Err(PortScanError::TimedOut);
            }
        }
    }
}

/// A short-lived admission fence owned by the exact start operation. It
/// serializes cooperating DevManager starts for one port and is released even
/// when the owner returns an error or is dropped. It does not claim ownership
/// of an external listener; the start owner must still perform its exact bind
/// or revalidation before spawning.
#[derive(Debug)]
pub struct PortStartReservation {
    inner: Weak<PortInventoryInner>,
    port: u16,
    token: u64,
}

impl PortStartReservation {
    pub fn port(&self) -> u16 {
        self.port
    }

    pub fn is_active(&self) -> bool {
        self.inner.upgrade().is_some_and(|inner| {
            inner
                .reservations
                .lock()
                .ok()
                .and_then(|reservations| reservations.get(&self.port).copied())
                == Some(self.token)
        })
    }
}

impl Drop for PortStartReservation {
    fn drop(&mut self) {
        let Some(inner) = self.inner.upgrade() else {
            return;
        };
        let Ok(mut reservations) = inner.reservations.lock() else {
            return;
        };
        if reservations.get(&self.port).copied() == Some(self.token) {
            reservations.remove(&self.port);
        }
    }
}

#[derive(Debug)]
struct PendingScan {
    ports: Vec<u16>,
    waiters: Vec<Arc<ScanWaiter>>,
}

#[derive(Debug)]
struct CoordinatorState {
    pending: Option<PendingScan>,
    active_waiters: Vec<Arc<ScanWaiter>>,
    active_cancellation: Option<ScanCancellation>,
    worker_active: bool,
    shutdown: bool,
}

struct PortInventoryInner {
    snapshot: RwLock<Arc<PortInventorySnapshot>>,
    publication_sequence: AtomicU64,
    reservation_sequence: AtomicU64,
    reservations: Mutex<BTreeMap<u16, u64>>,
    external_handles: AtomicUsize,
    scanner: Arc<dyn PortScanner>,
    scan_timeout: Duration,
    coordinator: Mutex<CoordinatorState>,
    wake_worker: Condvar,
    worker: Mutex<Option<thread::JoinHandle<()>>>,
}

/// Background-owned listener inventory with immutable read-only snapshots.
///
/// There is at most one logical scan in flight. While that scan is running,
/// requests replace one bounded pending request with the newest port set. A
/// timed-out native operation is cancelled and drained before the next scan is
/// admitted, so the service never overlaps listener-table probes. Published
/// snapshots carry a monotonic sequence and late results cannot overwrite a
/// newer one.
pub struct PortInventory {
    inner: Arc<PortInventoryInner>,
}

impl Clone for PortInventory {
    fn clone(&self) -> Self {
        self.inner.external_handles.fetch_add(1, Ordering::AcqRel);
        Self {
            inner: self.inner.clone(),
        }
    }
}

impl Drop for PortInventory {
    fn drop(&mut self) {
        if self.inner.external_handles.fetch_sub(1, Ordering::AcqRel) == 1 {
            // The worker may temporarily hold a strong Arc while it waits on
            // the coordinator. Signal shutdown from the final public handle
            // rather than relying on PortInventoryInner::drop, so that the
            // worker is woken and releases that Arc deterministically.
            self.shutdown();
        }
    }
}

impl std::fmt::Debug for PortInventory {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PortInventory")
            .field("cached_snapshot", &self.cached_snapshot())
            .field("scan_timeout", &self.inner.scan_timeout)
            .finish()
    }
}

impl Default for PortInventory {
    fn default() -> Self {
        Self::new()
    }
}

impl PortInventory {
    pub fn new() -> Self {
        Self::with_scanner_and_timeout(NativePortScanner, DEFAULT_PORT_SCAN_TIMEOUT)
    }

    pub fn with_scanner<S>(scanner: S) -> Self
    where
        S: PortScanner,
    {
        Self::with_scanner_and_timeout(scanner, DEFAULT_PORT_SCAN_TIMEOUT)
    }

    pub fn with_scanner_and_timeout<S>(scanner: S, scan_timeout: Duration) -> Self
    where
        S: PortScanner,
    {
        Self {
            inner: Arc::new(PortInventoryInner {
                snapshot: RwLock::new(Arc::new(PortInventorySnapshot::new(BTreeMap::new()))),
                publication_sequence: AtomicU64::new(0),
                reservation_sequence: AtomicU64::new(0),
                reservations: Mutex::new(BTreeMap::new()),
                external_handles: AtomicUsize::new(1),
                scanner: Arc::new(scanner),
                scan_timeout: scan_timeout.max(Duration::from_millis(1)),
                coordinator: Mutex::new(CoordinatorState {
                    pending: None,
                    active_waiters: Vec::new(),
                    active_cancellation: None,
                    worker_active: false,
                    shutdown: false,
                }),
                wake_worker: Condvar::new(),
                worker: Mutex::new(None),
            }),
        }
    }

    pub fn cached_snapshot(&self) -> Arc<PortInventorySnapshot> {
        self.inner
            .snapshot
            .read()
            .expect("port inventory cache lock")
            .clone()
    }

    /// Compatibility publication hook for existing callers. Sequenced
    /// snapshots are admitted only when newer; an unsequenced snapshot is
    /// accepted only before the first sequenced publication so old tests and
    /// callers retain pointer identity without weakening late-result fencing.
    pub fn publish(&self, snapshot: Arc<PortInventorySnapshot>) {
        if snapshot.publication_sequence() == 0 {
            let mut current = self
                .inner
                .snapshot
                .write()
                .expect("port inventory cache lock");
            if current.publication_sequence() == 0 {
                *current = snapshot;
            }
            return;
        }
        let _ = self.publish_if_newer(snapshot);
    }

    pub fn publish_if_newer(&self, snapshot: Arc<PortInventorySnapshot>) -> bool {
        let candidate_sequence = snapshot.publication_sequence();
        if candidate_sequence == 0 {
            return false;
        }
        let mut current = self
            .inner
            .snapshot
            .write()
            .expect("port inventory cache lock");
        if candidate_sequence <= current.publication_sequence() {
            return false;
        }
        *current = snapshot;
        self.inner
            .publication_sequence
            .fetch_max(candidate_sequence, Ordering::Release);
        true
    }

    /// Request a scan without doing any listener or process work on the
    /// caller. Only one pending request is retained; all waiters coalesced
    /// into that slot receive the newest request's exact result.
    pub fn request_scan(&self, ports: &[u16]) -> Result<PortScanRequest, PortScanError> {
        let ports = normalize_scan_ports(ports)?;
        let waiter = Arc::new(ScanWaiter::new());
        let mut previous_worker = None;
        {
            let mut state = self
                .inner
                .coordinator
                .lock()
                .expect("port inventory coordinator lock");
            if state.shutdown {
                return Err(PortScanError::Shutdown);
            }
            let queued_waiters = state.active_waiters.len()
                + state
                    .pending
                    .as_ref()
                    .map_or(0, |pending| pending.waiters.len());
            if queued_waiters >= MAX_SCAN_WAITERS {
                return Err(PortScanError::QueueFull {
                    actual: queued_waiters.saturating_add(1),
                    max: MAX_SCAN_WAITERS,
                });
            }
            if let Some(pending) = state.pending.as_mut() {
                pending.ports = ports;
                pending.waiters.push(waiter.clone());
            } else {
                state.pending = Some(PendingScan {
                    ports,
                    waiters: vec![waiter.clone()],
                });
            }
            if !state.worker_active {
                state.worker_active = true;
                let weak_inner = Arc::downgrade(&self.inner);
                match thread::Builder::new()
                    .name("devmanager-port-inventory".to_string())
                    .spawn(move || run_inventory_worker(weak_inner))
                {
                    Ok(handle) => {
                        let mut slot = self
                            .inner
                            .worker
                            .lock()
                            .expect("port inventory worker lock");
                        previous_worker = slot.replace(handle);
                    }
                    Err(error) => {
                        let error =
                            PortScanError::Scan(format!("could not start scan worker: {error}"));
                        waiter.complete(Err(error.clone()));
                        state.worker_active = false;
                        state.pending = None;
                        return Err(error);
                    }
                }
            }
            self.inner.wake_worker.notify_one();
        }
        if let Some(previous_worker) = previous_worker {
            let _ = previous_worker.join();
        }
        Ok(PortScanRequest { waiter })
    }

    /// Run one background scan and return the exact immutable result that was
    /// considered for publication. The cache is never reread or merged into
    /// this return value.
    pub fn refresh(&self, ports: &[u16]) -> Result<Arc<PortInventorySnapshot>, String> {
        let request = self
            .request_scan(ports)
            .map_err(|error| error.to_string())?;
        match request.wait(self.inner.scan_timeout) {
            Ok(snapshot) => Ok(snapshot),
            Err(PortScanError::Scan(error)) => Err(error),
            Err(error) => Err(error.to_string()),
        }
    }

    pub fn reserve_start(
        &self,
        snapshot: &PortInventorySnapshot,
        port: u16,
    ) -> Result<PortStartReservation, PortStartError> {
        crate::process::ports::ensure_managed_start_allowed(snapshot, port)?;
        let token = self
            .inner
            .reservation_sequence
            .fetch_add(1, Ordering::AcqRel)
            .saturating_add(1);
        let mut reservations =
            self.inner
                .reservations
                .lock()
                .map_err(|_| PortStartError::ProbeFailed {
                    port,
                    detail: "port start reservation lock was poisoned".to_string(),
                })?;
        if reservations.contains_key(&port) {
            return Err(PortStartError::ReservationConflict { port });
        }
        reservations.insert(port, token);
        Ok(PortStartReservation {
            inner: Arc::downgrade(&self.inner),
            port,
            token,
        })
    }

    pub fn cancel_active_scan(&self) {
        let state = self
            .inner
            .coordinator
            .lock()
            .expect("port inventory coordinator lock");
        if let Some(cancellation) = state.active_cancellation.as_ref() {
            cancellation.cancel();
        }
    }

    pub fn shutdown(&self) {
        let (active_waiters, pending_waiters, worker) = {
            let mut state = self
                .inner
                .coordinator
                .lock()
                .expect("port inventory coordinator lock");
            state.shutdown = true;
            if let Some(cancellation) = state.active_cancellation.as_ref() {
                cancellation.cancel();
            }
            let active_waiters = std::mem::take(&mut state.active_waiters);
            let pending_waiters = state
                .pending
                .take()
                .map(|pending| pending.waiters)
                .unwrap_or_default();
            self.inner.wake_worker.notify_all();
            let worker = self
                .inner
                .worker
                .lock()
                .expect("port inventory worker lock")
                .take();
            (active_waiters, pending_waiters, worker)
        };
        for waiter in active_waiters.into_iter().chain(pending_waiters) {
            waiter.complete(Err(PortScanError::Shutdown));
        }
        if let Some(worker) = worker {
            if worker.thread().id() != thread::current().id() {
                let _ = worker.join();
            }
        }
    }
}

impl Drop for PortInventoryInner {
    fn drop(&mut self) {
        if let Ok(mut state) = self.coordinator.lock() {
            state.shutdown = true;
            if let Some(cancellation) = state.active_cancellation.as_ref() {
                cancellation.cancel();
            }
            self.wake_worker.notify_all();
        }
        if let Ok(mut worker) = self.worker.lock() {
            if let Some(worker) = worker.take() {
                if worker.thread().id() != thread::current().id() {
                    let _ = worker.join();
                }
            }
        }
    }
}

struct NativePortScanner;

impl PortScanner for NativePortScanner {
    fn scan(
        &self,
        ports: &[u16],
        cancellation: &ScanCancellation,
    ) -> Result<PortInventorySnapshot, String> {
        if cancellation.is_cancelled() {
            return Err("scan cancelled before listener enumeration".to_string());
        }
        let observation_time = Instant::now();
        let snapshot = scan_listener_inventory_with_deadline(
            ports,
            platform_service::snapshot_listener_endpoints,
            capture_listener_identity,
            observation_time,
            cancellation.deadline(),
        )?;
        if cancellation.is_cancelled() {
            return Err("scan cancelled after listener enumeration".to_string());
        }
        Ok(snapshot)
    }
}

fn normalize_scan_ports(ports: &[u16]) -> Result<Vec<u16>, PortScanError> {
    let mut ports = ports.to_vec();
    ports.sort_unstable();
    ports.dedup();
    if ports.len() > MAX_PORTS_PER_SCAN {
        return Err(PortScanError::TooManyPorts {
            actual: ports.len(),
            max: MAX_PORTS_PER_SCAN,
        });
    }
    Ok(ports)
}

fn next_publication_sequence(inner: &PortInventoryInner) -> u64 {
    inner
        .publication_sequence
        .fetch_add(1, Ordering::AcqRel)
        .saturating_add(1)
}

struct ScanExecution {
    result: Result<Arc<PortInventorySnapshot>, PortScanError>,
    worker: Option<thread::JoinHandle<()>>,
}

fn execute_scan(
    weak_inner: &Weak<PortInventoryInner>,
    scanner: Arc<dyn PortScanner>,
    ports: Vec<u16>,
    cancellation: ScanCancellation,
) -> ScanExecution {
    let sequence = weak_inner
        .upgrade()
        .map(|inner| next_publication_sequence(&inner))
        .unwrap_or(0);
    let child_cancellation = cancellation.clone();
    let failure_ports = ports.clone();
    let (sender, receiver) = mpsc::sync_channel(1);
    let worker = match thread::Builder::new()
        .name("devmanager-port-scan".to_string())
        .spawn(move || {
            let result = scanner.scan(&ports, &child_cancellation);
            let _ = sender.send(result);
        }) {
        Ok(worker) => worker,
        Err(error) => {
            return ScanExecution {
                result: Err(PortScanError::Scan(format!(
                    "could not start scan: {error}"
                ))),
                worker: None,
            }
        }
    };

    let remaining = cancellation
        .deadline()
        .saturating_duration_since(Instant::now());
    match receiver.recv_timeout(remaining) {
        Ok(Ok(snapshot)) => {
            if cancellation.is_cancelled() {
                if Instant::now() >= cancellation.deadline() {
                    let failure = Arc::new(
                        PortInventorySnapshot::probe_failure(
                            failure_ports,
                            "listener inventory scan timed out",
                        )
                        .with_publication_sequence(sequence),
                    );
                    if let Some(inner) = weak_inner.upgrade() {
                        let _ = inner_publish_snapshot(&inner, failure);
                    }
                    return ScanExecution {
                        result: Err(PortScanError::TimedOut),
                        worker: Some(worker),
                    };
                }
                return ScanExecution {
                    result: Err(PortScanError::Cancelled),
                    worker: Some(worker),
                };
            }
            let snapshot = Arc::new(snapshot.with_publication_sequence(sequence));
            if let Some(inner) = weak_inner.upgrade() {
                let _ = inner_publish_snapshot(&inner, snapshot.clone());
            }
            ScanExecution {
                result: Ok(snapshot),
                worker: Some(worker),
            }
        }
        Ok(Err(error)) => {
            if cancellation.is_cancelled() {
                let timeout = Instant::now() >= cancellation.deadline();
                if timeout {
                    let failure = Arc::new(
                        PortInventorySnapshot::probe_failure(
                            failure_ports,
                            "listener inventory scan timed out",
                        )
                        .with_publication_sequence(sequence),
                    );
                    if let Some(inner) = weak_inner.upgrade() {
                        let _ = inner_publish_snapshot(&inner, failure);
                    }
                }
                return ScanExecution {
                    result: Err(if timeout {
                        PortScanError::TimedOut
                    } else {
                        PortScanError::Cancelled
                    }),
                    worker: Some(worker),
                };
            }
            let error = bounded_scan_error(&error);
            let failure = Arc::new(
                PortInventorySnapshot::probe_failure(failure_ports, error.clone())
                    .with_publication_sequence(sequence),
            );
            if let Some(inner) = weak_inner.upgrade() {
                let _ = inner_publish_snapshot(&inner, failure);
            }
            ScanExecution {
                result: Err(PortScanError::Scan(error)),
                worker: Some(worker),
            }
        }
        Err(RecvTimeoutError::Timeout) => {
            cancellation.cancel();
            let failure = Arc::new(
                PortInventorySnapshot::probe_failure(
                    failure_ports,
                    "listener inventory scan timed out",
                )
                .with_publication_sequence(sequence),
            );
            if let Some(inner) = weak_inner.upgrade() {
                let _ = inner_publish_snapshot(&inner, failure);
            }
            ScanExecution {
                result: Err(PortScanError::TimedOut),
                worker: Some(worker),
            }
        }
        Err(RecvTimeoutError::Disconnected) => {
            let error = "port scanner terminated without a result";
            let failure = Arc::new(
                PortInventorySnapshot::probe_failure(failure_ports, error)
                    .with_publication_sequence(sequence),
            );
            if let Some(inner) = weak_inner.upgrade() {
                let _ = inner_publish_snapshot(&inner, failure);
            }
            ScanExecution {
                result: Err(PortScanError::Scan(error.to_string())),
                worker: Some(worker),
            }
        }
    }
}

fn inner_publish_snapshot(
    inner: &PortInventoryInner,
    snapshot: Arc<PortInventorySnapshot>,
) -> bool {
    let mut current = inner.snapshot.write().expect("port inventory cache lock");
    if snapshot.publication_sequence() <= current.publication_sequence() {
        return false;
    }
    *current = snapshot;
    true
}

fn run_inventory_worker(weak_inner: Weak<PortInventoryInner>) {
    loop {
        let pending = {
            let Some(inner) = weak_inner.upgrade() else {
                return;
            };
            let mut state = inner
                .coordinator
                .lock()
                .expect("port inventory coordinator lock");
            loop {
                if state.shutdown {
                    state.worker_active = false;
                    return;
                }
                if let Some(pending) = state.pending.take() {
                    state.active_waiters = pending.waiters.clone();
                    let cancellation = ScanCancellation::new(Instant::now() + inner.scan_timeout);
                    state.active_cancellation = Some(cancellation.clone());
                    break (
                        pending.ports,
                        pending.waiters,
                        cancellation,
                        inner.scanner.clone(),
                    );
                }
                state = inner
                    .wake_worker
                    .wait(state)
                    .expect("port inventory worker condition variable");
            }
        };

        let execution = execute_scan(&weak_inner, pending.3, pending.0, pending.2);
        for waiter in &pending.1 {
            waiter.complete(execution.result.clone());
        }

        // A timed-out scanner is never allowed to mutate the inventory after
        // the coordinator advances. Complete callers at the deadline above,
        // then join the child before admitting any next scan.
        if let Some(worker) = execution.worker {
            let _ = worker.join();
        }

        let Some(inner) = weak_inner.upgrade() else {
            return;
        };
        let mut state = inner
            .coordinator
            .lock()
            .expect("port inventory coordinator lock");
        state.active_waiters.clear();
        state.active_cancellation = None;
        if state.shutdown {
            state.worker_active = false;
            return;
        }
        if state.pending.is_none() {
            state.worker_active = false;
            return;
        }
    }
}

/// Probe all requested ports with one native listener-table query, then
/// revalidate the same endpoint rows after PID creation-time capture.
pub fn scan_listener_inventory(ports: &[u16]) -> Result<PortInventorySnapshot, String> {
    let observation_time = Instant::now();
    scan_listener_inventory_with_deadline(
        ports,
        platform_service::snapshot_listener_endpoints,
        capture_listener_identity,
        observation_time,
        observation_time
            .checked_add(crate::process::ports::DEFAULT_FREE_PROOF_MAX_AGE)
            .unwrap_or(observation_time),
    )
}

/// Testable boundary for the enumerate/capture/revalidate sequence. The
/// enumerator is called exactly twice. A changed endpoint row is retained in
/// the immutable result but marked as a reconciliation fault, which makes the
/// authority Unknown and prevents a Free launch proof.
pub fn scan_listener_inventory_with<Enumerate, Capture>(
    ports: &[u16],
    enumerate: Enumerate,
    capture: Capture,
) -> Result<PortInventorySnapshot, String>
where
    Enumerate: FnMut(&[u16]) -> Result<BTreeMap<u16, Vec<TcpEndpointRecord>>, String>,
    Capture: FnMut(u32) -> Result<ListenerIdentity, String>,
{
    let observation_time = Instant::now();
    let deadline = observation_time
        .checked_add(crate::process::ports::DEFAULT_FREE_PROOF_MAX_AGE)
        .unwrap_or(observation_time);
    scan_listener_inventory_with_deadline(ports, enumerate, capture, observation_time, deadline)
}

/// Testable boundary for a bounded, single-observation listener scan. The
/// caller owns both the absolute deadline and the observation timestamp so
/// the first and second listener publications cannot silently observe time at
/// different instants.
pub fn scan_listener_inventory_with_deadline<Enumerate, Capture>(
    ports: &[u16],
    mut enumerate: Enumerate,
    mut capture: Capture,
    observation_time: Instant,
    deadline: Instant,
) -> Result<PortInventorySnapshot, String>
where
    Enumerate: FnMut(&[u16]) -> Result<BTreeMap<u16, Vec<TcpEndpointRecord>>, String>,
    Capture: FnMut(u32) -> Result<ListenerIdentity, String>,
{
    let ensure_before_deadline = || {
        if Instant::now() > deadline || observation_time > deadline {
            Err("port listener scan deadline expired".to_string())
        } else {
            Ok(())
        }
    };
    ensure_before_deadline()?;
    let ports = normalize_scan_ports(ports).map_err(|error| error.to_string())?;
    ensure_before_deadline()?;
    let first = normalize_listener_table(enumerate(&ports)?, &ports)?;
    ensure_before_deadline()?;
    let mut first_identities = BTreeMap::<u32, Result<ListenerIdentity, String>>::new();
    for rows in first.values() {
        for row in rows {
            ensure_before_deadline()?;
            first_identities
                .entry(row.pid())
                .or_insert_with(|| capture(row.pid()));
            ensure_before_deadline()?;
        }
    }

    ensure_before_deadline()?;
    let second = normalize_listener_table(enumerate(&ports)?, &ports)?;
    ensure_before_deadline()?;
    let mut second_identities = BTreeMap::<u32, Result<ListenerIdentity, String>>::new();
    for rows in second.values() {
        for row in rows {
            ensure_before_deadline()?;
            second_identities
                .entry(row.pid())
                .or_insert_with(|| capture(row.pid()));
            ensure_before_deadline()?;
        }
    }

    ensure_before_deadline()?;
    let mut observations = BTreeMap::new();
    let mut endpoints = BTreeMap::new();
    let mut issues = BTreeMap::new();
    let mut error_count = 0usize;
    for port in ports {
        let first_rows = first.get(&port).map(Vec::as_slice).unwrap_or(&[]);
        let second_rows = second.get(&port).map(Vec::as_slice).unwrap_or(&[]);
        let changed = first_rows != second_rows;
        let identity_changed = second_rows.iter().any(|row| {
            matches!(
                (first_identities.get(&row.pid()), second_identities.get(&row.pid())),
                (Some(Ok(first)), Some(Ok(second))) if first != second
            )
        });
        let mut endpoint_values = Vec::with_capacity(second_rows.len());
        let mut listener_values = Vec::with_capacity(second_rows.len());
        let mut errors = Vec::new();
        for row in second_rows {
            match second_identities.get(&row.pid()) {
                Some(Ok(identity)) => {
                    endpoint_values.push(TcpEndpoint::from_record(row, identity.clone()));
                    listener_values.push(identity.clone());
                }
                Some(Err(error)) => {
                    if errors.len() < MAX_SCAN_ERRORS {
                        errors.push(bounded_scan_error(error));
                    }
                }
                None => {
                    if errors.len() < MAX_SCAN_ERRORS {
                        errors.push(format!("listener PID {} was not captured", row.pid()));
                    }
                }
            }
            if let Some(Err(error)) = first_identities.get(&row.pid()) {
                if errors.len() < MAX_SCAN_ERRORS {
                    errors.push(format!(
                        "listener PID {} identity was unavailable during reconciliation: {}",
                        row.pid(),
                        bounded_scan_error(error),
                    ));
                }
            }
        }
        if !errors.is_empty() {
            error_count = error_count.saturating_add(errors.len());
            let detail = errors.join("; ");
            observations.insert(port, PortObservation::ProbeError(detail.clone()));
            issues.insert(port, PortObservationIssue::ProbeError(detail));
        } else {
            observations.insert(port, PortObservation::from_listeners(listener_values));
            if !endpoint_values.is_empty() {
                endpoint_values.sort_unstable();
                endpoint_values.dedup();
                endpoints.insert(port, Arc::from(endpoint_values.into_boxed_slice()));
            }
            if (changed || identity_changed) && (!first_rows.is_empty() || !second_rows.is_empty())
            {
                issues.insert(
                    port,
                    PortObservationIssue::ReconciliationFault(if identity_changed {
                        "listener process identity changed during endpoint capture; retry"
                            .to_string()
                    } else {
                        "listener endpoint table changed during PID identity capture; retry"
                            .to_string()
                    }),
                );
                error_count = error_count.saturating_add(1);
            }
        }
    }

    if error_count > MAX_SCAN_ERRORS {
        return Err(format!(
            "port inventory produced more than {} bounded errors",
            MAX_SCAN_ERRORS
        ));
    }
    ensure_before_deadline()?;
    Ok(PortInventorySnapshot::from_parts(
        observations,
        endpoints,
        issues,
        observation_time,
    ))
}

fn normalize_listener_table(
    table: BTreeMap<u16, Vec<TcpEndpointRecord>>,
    ports: &[u16],
) -> Result<BTreeMap<u16, Vec<TcpEndpointRecord>>, String> {
    let allowed = ports
        .iter()
        .copied()
        .collect::<std::collections::BTreeSet<_>>();
    let mut normalized = BTreeMap::new();
    let mut endpoint_count = 0usize;
    for (port, mut rows) in table {
        if !allowed.contains(&port) {
            return Err(format!(
                "listener endpoint table returned unrequested port {port}"
            ));
        }
        if rows.iter().any(|row| row.port() != port) {
            return Err(format!(
                "listener endpoint table row does not match requested port {port}"
            ));
        }
        rows.sort_unstable();
        rows.dedup();
        endpoint_count = endpoint_count.saturating_add(rows.len());
        if endpoint_count > MAX_ENDPOINTS_PER_SCAN {
            return Err(format!(
                "listener endpoint count exceeds {}",
                MAX_ENDPOINTS_PER_SCAN
            ));
        }
        if !rows.is_empty() {
            normalized.insert(port, rows);
        }
    }
    Ok(normalized)
}

fn bounded_scan_error(error: &str) -> String {
    let mut characters = error.chars();
    let mut detail = String::new();
    while detail.chars().count() < 255 {
        let Some(character) = characters.next() else {
            return detail;
        };
        detail.push(if character.is_control() {
            ' '
        } else {
            character
        });
    }
    if characters.next().is_some() {
        detail.push('…');
    }
    detail
}

#[cfg(windows)]
fn capture_listener_identity(pid: u32) -> Result<ListenerIdentity, String> {
    use std::ffi::c_void;
    use std::ffi::OsString;
    use std::os::windows::ffi::OsStringExt;

    const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;

    #[repr(C)]
    #[derive(Default)]
    struct FileTime {
        low_date_time: u32,
        high_date_time: u32,
    }

    #[link(name = "kernel32")]
    extern "system" {
        fn OpenProcess(desired_access: u32, inherit_handle: i32, process_id: u32) -> *mut c_void;
        fn GetProcessTimes(
            process: *mut c_void,
            creation: *mut FileTime,
            exit: *mut FileTime,
            kernel: *mut FileTime,
            user: *mut FileTime,
        ) -> i32;
        fn QueryFullProcessImageNameW(
            process: *mut c_void,
            flags: u32,
            exe_name: *mut u16,
            size: *mut u32,
        ) -> i32;
        fn CloseHandle(object: *mut c_void) -> i32;
    }

    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if process.is_null() {
        return Err(format!(
            "could not open listener PID {pid} for identity verification: {}",
            std::io::Error::last_os_error()
        ));
    }

    let mut creation = FileTime::default();
    let mut exit = FileTime::default();
    let mut kernel = FileTime::default();
    let mut user = FileTime::default();
    let result =
        unsafe { GetProcessTimes(process, &mut creation, &mut exit, &mut kernel, &mut user) };
    let mut executable_buffer = vec![0u16; 32_768];
    let mut executable_length = executable_buffer.len() as u32;
    let executable_result = unsafe {
        QueryFullProcessImageNameW(
            process,
            0,
            executable_buffer.as_mut_ptr(),
            &mut executable_length,
        )
    };
    let executable = (executable_result != 0 && executable_length > 0)
        .then(|| OsString::from_wide(&executable_buffer[..executable_length as usize]));
    let close_result = unsafe { CloseHandle(process) };
    if result == 0 {
        return Err(format!(
            "could not read listener PID {pid} creation time: {}",
            std::io::Error::last_os_error()
        ));
    }
    if close_result == 0 {
        return Err(format!(
            "could not close listener PID {pid} identity handle: {}",
            std::io::Error::last_os_error()
        ));
    }

    let creation_time_100ns =
        ((creation.high_date_time as u64) << 32) | creation.low_date_time as u64;
    let executable = executable.ok_or_else(|| {
        format!(
            "could not read listener PID {pid} canonical executable: {}",
            std::io::Error::last_os_error()
        )
    })?;
    ListenerIdentity::with_executable(pid, creation_time_100ns, executable)
        .map_err(|error| error.to_string())
}

#[cfg(not(windows))]
fn capture_listener_identity(pid: u32) -> Result<ListenerIdentity, String> {
    let creation_time_100ns = platform_service::capture_process_creation_time_100ns(pid)
        .ok_or_else(|| format!("listener PID {pid} creation time is unavailable"))?;
    let executable = platform_service::capture_process_executable(pid).ok_or_else(|| {
        format!("listener PID {pid} executable identity is unavailable on this platform")
    })?;
    ListenerIdentity::with_executable(pid, creation_time_100ns, executable)
        .map_err(|error| format!("could not verify listener PID {pid}: {error}"))
}

pub fn snapshot_ports(ports: &[u16]) -> Result<HashMap<u16, PortStatus>, String> {
    let snapshot = scan_listener_inventory(ports)?;
    legacy_statuses_from_snapshot(&snapshot, ports)
}

/// Convert an inventory snapshot to the existing UI/remote port model.
///
/// The legacy model has one optional PID, so an ambiguous multi-listener
/// observation intentionally retains only `in_use = true` and no PID. Any
/// probe or reconciliation issue is an error instead of a blue/external
/// compatibility status.
pub fn legacy_statuses_from_snapshot(
    snapshot: &PortInventorySnapshot,
    ports: &[u16],
) -> Result<HashMap<u16, PortStatus>, String> {
    if !snapshot.is_exactly_for(ports) {
        return Err(bounded_scan_error(
            "port inventory snapshot does not exactly match the requested port set",
        ));
    }
    if !snapshot.is_valid() {
        return Err(bounded_scan_error(
            snapshot
                .validation_error()
                .unwrap_or("port inventory snapshot failed validation"),
        ));
    }
    let mut statuses = HashMap::with_capacity(ports.len());

    for &port in ports {
        if let Some(issue) = snapshot.issue(port) {
            return Err(bounded_scan_error(issue.detail()));
        }
        let status = match snapshot.observation(port) {
            Some(PortObservation::Listeners(listeners)) => PortStatus {
                port,
                in_use: true,
                pid: (listeners.len() == 1).then(|| listeners[0].pid()),
                process_name: None,
            },
            Some(PortObservation::Free) => PortStatus {
                port,
                in_use: false,
                pid: None,
                process_name: None,
            },
            Some(PortObservation::ProbeError(error)) => return Err(bounded_scan_error(error)),
            None => {
                return Err(format!(
                    "port {port} was not included in listener inventory"
                ))
            }
        };
        statuses.insert(port, status);
    }

    Ok(statuses)
}

pub fn check_port_in_use(port: u16) -> Result<PortStatus, String> {
    let mut status = snapshot_ports(&[port])?
        .remove(&port)
        .unwrap_or(PortStatus {
            port,
            in_use: false,
            pid: None,
            process_name: None,
        });
    if let Some(pid) = status.pid {
        status.process_name = platform_service::get_process_name(pid)?;
    }
    Ok(status)
}

pub fn get_port_conflicts(config: &AppConfig) -> Vec<PortConflict> {
    let mut port_map: BTreeMap<u16, Vec<PortConflictEntry>> = BTreeMap::new();

    for project in &config.projects {
        for folder in &project.folders {
            for command in &folder.commands {
                if let Some(port) = command.port {
                    port_map.entry(port).or_default().push(PortConflictEntry {
                        project_name: project.name.clone(),
                        command_label: command.label.clone(),
                        command_id: command.id.clone(),
                    });
                }
            }
        }
    }

    port_map
        .into_iter()
        .filter(|(_, commands)| commands.len() > 1)
        .map(|(port, commands)| PortConflict { port, commands })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::get_port_conflicts;
    use crate::models::{AppConfig, Project, ProjectFolder, RunCommand};

    #[test]
    fn duplicate_ports_are_reported_once() {
        let config = AppConfig {
            projects: vec![
                Project {
                    id: "project-a".to_string(),
                    name: "Project A".to_string(),
                    folders: vec![ProjectFolder {
                        id: "folder-a".to_string(),
                        name: "api".to_string(),
                        commands: vec![RunCommand {
                            id: "command-a".to_string(),
                            label: "dev".to_string(),
                            port: Some(3000),
                            ..Default::default()
                        }],
                        ..Default::default()
                    }],
                    ..Default::default()
                },
                Project {
                    id: "project-b".to_string(),
                    name: "Project B".to_string(),
                    folders: vec![ProjectFolder {
                        id: "folder-b".to_string(),
                        name: "web".to_string(),
                        commands: vec![
                            RunCommand {
                                id: "command-b".to_string(),
                                label: "serve".to_string(),
                                port: Some(3000),
                                ..Default::default()
                            },
                            RunCommand {
                                id: "command-c".to_string(),
                                label: "admin".to_string(),
                                port: Some(4100),
                                ..Default::default()
                            },
                        ],
                        ..Default::default()
                    }],
                    ..Default::default()
                },
            ],
            ..Default::default()
        };

        let conflicts = get_port_conflicts(&config);

        assert_eq!(conflicts.len(), 1);
        assert_eq!(conflicts[0].port, 3000);
        assert_eq!(conflicts[0].commands.len(), 2);
    }
}
