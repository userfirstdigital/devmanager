use crate::providers::adapter::{
    ProviderAdapter, ProviderError, ProviderProbeRequest, ProviderProbeResult,
};
use crate::providers::capabilities::{
    AdapterRevision, ProviderAuthEvidenceError, ProviderAuthEvidenceReceipt,
    ProviderAuthEvidenceRegistry, ProviderAuthProbeInvocation, ProviderAuthProbeResult,
    ProviderCapabilities, ProviderDiscoveryCandidateInput, ProviderDiscoveryContract,
    ProviderDiscoveryError, ProviderExecutable, ProviderExecutableError, ProviderExecutableHandle,
    ProviderKind, ProviderVersion, SemanticSchemaVersion, MAX_PROVIDER_CAPABILITY_CACHE_ENTRIES,
};
use async_trait::async_trait;
use futures_util::FutureExt;
use serde::de;
use serde::ser::SerializeStruct;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::collections::{BTreeMap, HashMap};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, Notify};
use tokio::task::{JoinHandle, JoinSet};

const TASK_4_1_ADAPTER_REVISION: AdapterRevision = AdapterRevision::new(1);
const TASK_4_1_SEMANTIC_SCHEMA_VERSION: SemanticSchemaVersion = SemanticSchemaVersion::new(1);
const PROVIDER_CAPABILITY_CACHE_TTL: Duration = Duration::from_secs(5 * 60);
const MAX_PROVIDER_IN_FLIGHT_ENTRIES: usize = 64;
const PROVIDER_IN_FLIGHT_TTL: Duration = Duration::from_secs(30);
const PROVIDER_WORKER_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone, Default)]
pub struct ProviderDiscoveryConfig {
    pub executable_override: Option<PathBuf>,
    pub path: Option<OsString>,
}

impl std::fmt::Debug for ProviderDiscoveryConfig {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ProviderDiscoveryConfig")
            .field(
                "executable_override_bound",
                &self.executable_override.is_some(),
            )
            .field("path_bound", &self.path.is_some())
            .finish()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CacheStatus {
    Hit,
    Miss,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CapabilityCacheKey {
    pub kind: ProviderKind,
    pub executable: ProviderExecutable,
    pub version: ProviderVersion,
    pub adapter_revision: AdapterRevision,
    pub semantic_schema_version: SemanticSchemaVersion,
}

impl Serialize for CapabilityCacheKey {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut state = serializer.serialize_struct("CapabilityCacheKey", 6)?;
        state.serialize_field(
            "schema_version",
            &crate::providers::capabilities::PROVIDER_CACHE_KEY_SCHEMA_VERSION,
        )?;
        state.serialize_field("kind", &self.kind)?;
        state.serialize_field("executable", &self.executable)?;
        state.serialize_field("version", &self.version)?;
        state.serialize_field("adapter_revision", &self.adapter_revision)?;
        state.serialize_field("semantic_schema_version", &self.semantic_schema_version)?;
        state.end()
    }
}

impl<'de> Deserialize<'de> for CapabilityCacheKey {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            schema_version: u16,
            kind: ProviderKind,
            executable: ProviderExecutable,
            version: ProviderVersion,
            adapter_revision: AdapterRevision,
            semantic_schema_version: SemanticSchemaVersion,
        }

        let wire = Wire::deserialize(deserializer)?;
        if wire.schema_version != crate::providers::capabilities::PROVIDER_CACHE_KEY_SCHEMA_VERSION
        {
            return Err(de::Error::custom(format!(
                "unsupported provider cache-key schema version {}",
                wire.schema_version
            )));
        }
        Ok(Self::new(
            wire.kind,
            wire.executable,
            wire.version,
            wire.adapter_revision,
            wire.semantic_schema_version,
        ))
    }
}

impl CapabilityCacheKey {
    pub fn new(
        kind: ProviderKind,
        executable: ProviderExecutable,
        version: ProviderVersion,
        adapter_revision: AdapterRevision,
        semantic_schema_version: SemanticSchemaVersion,
    ) -> Self {
        Self {
            kind,
            executable,
            version,
            adapter_revision,
            semantic_schema_version,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderObservation {
    pub kind: ProviderKind,
    pub executable: ProviderExecutable,
    pub version: ProviderVersion,
    pub adapter_revision: AdapterRevision,
    pub semantic_schema_version: SemanticSchemaVersion,
    pub capabilities: ProviderCapabilities,
    pub cache_status: CacheStatus,
}

impl ProviderObservation {
    pub fn validate(&self) -> Result<(), ProviderError> {
        if self.capabilities.kind != self.kind {
            return Err(ProviderError::CapabilityKindMismatch {
                expected: self.kind,
                actual: self.capabilities.kind,
            });
        }
        if self.capabilities.version != self.version {
            return Err(ProviderError::CapabilityVersionMismatch {
                expected: self.version.clone(),
                actual: self.capabilities.version.clone(),
            });
        }
        self.capabilities.validate()?;
        self.executable
            .validate_current()
            .map_err(ProviderError::Executable)?;
        Ok(())
    }
}

impl Serialize for ProviderObservation {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.validate().map_err(serde::ser::Error::custom)?;
        let mut state = serializer.serialize_struct("ProviderObservation", 8)?;
        state.serialize_field(
            "schema_version",
            &crate::providers::capabilities::PROVIDER_OBSERVATION_SCHEMA_VERSION,
        )?;
        state.serialize_field("kind", &self.kind)?;
        state.serialize_field("executable", &self.executable)?;
        state.serialize_field("version", &self.version)?;
        state.serialize_field("adapter_revision", &self.adapter_revision)?;
        state.serialize_field("semantic_schema_version", &self.semantic_schema_version)?;
        state.serialize_field("capabilities", &self.capabilities)?;
        state.serialize_field("cache_status", &self.cache_status)?;
        state.end()
    }
}

impl<'de> Deserialize<'de> for ProviderObservation {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            schema_version: u16,
            kind: ProviderKind,
            executable: ProviderExecutable,
            version: ProviderVersion,
            adapter_revision: AdapterRevision,
            semantic_schema_version: SemanticSchemaVersion,
            capabilities: ProviderCapabilities,
            cache_status: CacheStatus,
        }

        let wire = Wire::deserialize(deserializer)?;
        if wire.schema_version
            != crate::providers::capabilities::PROVIDER_OBSERVATION_SCHEMA_VERSION
        {
            return Err(de::Error::custom(format!(
                "unsupported provider observation schema version {}",
                wire.schema_version
            )));
        }
        let observation = Self {
            kind: wire.kind,
            executable: wire.executable,
            version: wire.version,
            adapter_revision: wire.adapter_revision,
            semantic_schema_version: wire.semantic_schema_version,
            capabilities: wire.capabilities,
            cache_status: wire.cache_status,
        };
        observation.validate().map_err(de::Error::custom)?;
        Ok(observation)
    }
}

#[async_trait]
pub trait ExecutableInspector: Send + Sync {
    async fn inspect(&self, path: &Path) -> Result<ProviderExecutable, ProviderExecutableError>;
}

#[derive(Debug, Default)]
pub struct FileSystemExecutableInspector;

#[async_trait]
impl ExecutableInspector for FileSystemExecutableInspector {
    async fn inspect(&self, path: &Path) -> Result<ProviderExecutable, ProviderExecutableError> {
        let path = path.to_path_buf();
        tokio::task::spawn_blocking(move || {
            #[cfg(target_os = "windows")]
            if path.extension().is_some_and(|extension| {
                let extension = extension.to_string_lossy();
                extension.eq_ignore_ascii_case("cmd")
                    || extension.eq_ignore_ascii_case("ps1")
                    || extension.eq_ignore_ascii_case("js")
            }) {
                return ProviderExecutable::inspect_non_native_blocking(&path);
            }
            ProviderExecutable::inspect_blocking(&path)
        })
        .await
        .map_err(|_| ProviderExecutableError::BackgroundTask)?
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ProbeReservationKey {
    kind: ProviderKind,
    executable_override: Option<PathBuf>,
    path: Option<OsString>,
}

impl ProbeReservationKey {
    fn new(kind: ProviderKind, config: &ProviderDiscoveryConfig) -> Self {
        Self {
            kind,
            executable_override: config.executable_override.clone(),
            path: config.path.clone().or_else(|| std::env::var_os("PATH")),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ProbeIdentityKey {
    kind: ProviderKind,
    executable: ProviderExecutable,
    launch_handle: ProviderExecutableHandle,
    adapter_revision: AdapterRevision,
    semantic_schema_version: SemanticSchemaVersion,
}

#[derive(Clone)]
struct ProbeWorkResult {
    capabilities: ProviderCapabilities,
    executable: ProviderExecutable,
    executable_handle: ProviderExecutableHandle,
    had_matching_cached: bool,
}

struct ProbeFlight {
    result: Mutex<Option<Result<ProbeWorkResult, ProviderError>>>,
    completed: Notify,
    started_at: Instant,
    deadline: Instant,
    cancelled: AtomicBool,
    cancellation: Notify,
}

impl ProbeFlight {
    fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
        let mut result = self.result.lock().unwrap();
        if result.is_none() {
            *result = Some(Err(ProviderError::Probe(
                crate::providers::adapter::ProviderProbeError::TimedOut,
            )));
        }
        self.cancellation.notify_waiters();
        self.completed.notify_waiters();
    }
}

struct ProbeLeaderCleanup {
    flight: Arc<ProbeFlight>,
    armed: bool,
}

impl ProbeLeaderCleanup {
    fn new(flight: Arc<ProbeFlight>) -> Self {
        Self {
            flight,
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for ProbeLeaderCleanup {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        self.flight.cancel();
    }
}

struct ProbeWorkerRequest {
    flight: Arc<ProbeFlight>,
    key: ProbeReservationKey,
    adapter: Arc<dyn ProviderAdapter>,
    kind: ProviderKind,
    config: ProviderDiscoveryConfig,
    cache: Arc<Mutex<CapabilityCache>>,
    executable_inspector: Arc<dyn ExecutableInspector>,
    in_flight: Arc<Mutex<HashMap<ProbeReservationKey, Arc<ProbeFlight>>>>,
    supervisor: Arc<ProbeWorkerSupervisorState>,
}

struct ProbeWorkerCompletion {
    flight: Arc<ProbeFlight>,
    key: ProbeReservationKey,
    in_flight: Arc<Mutex<HashMap<ProbeReservationKey, Arc<ProbeFlight>>>>,
    result: Result<ProbeWorkResult, ProviderError>,
}

struct ProbeWorkerSupervisorState {
    sender: mpsc::Sender<ProbeWorkerRequest>,
    shutdown: Notify,
    task: Mutex<Option<JoinHandle<()>>>,
}

struct ProbeWorkerSupervisor {
    state: Mutex<Option<Arc<ProbeWorkerSupervisorState>>>,
}

struct ProbeWorkerReaper {
    task: JoinHandle<()>,
    worker_deadline: tokio::time::Instant,
}

impl ProbeWorkerReaper {
    async fn join_until_deadline(mut self) {
        let joined = tokio::time::timeout_at(self.worker_deadline, &mut self.task).await;
        if joined.is_err() {
            // The supervisor owns a JoinSet, so aborting it at the shared
            // deadline also aborts every async worker still in that set.
            // Native spawn_blocking work remains owned by its own runtime
            // task and is independently bounded by the flight deadline.
            self.task.abort();
            let _ = self.task.await;
        }
    }
}

impl ProbeWorkerSupervisor {
    fn new() -> Self {
        Self {
            state: Mutex::new(None),
        }
    }

    fn submit(&self, request: ProbeWorkerRequest) -> Result<(), ProviderError> {
        let state = request.supervisor.clone();
        match state.sender.try_send(request) {
            Ok(()) => Ok(()),
            Err(mpsc::error::TrySendError::Full(_)) | Err(mpsc::error::TrySendError::Closed(_)) => {
                Err(ProviderError::Probe(
                    crate::providers::adapter::ProviderProbeError::Io(
                        crate::providers::adapter::ProviderProbeIoError::WaitFailed,
                    ),
                ))
            }
        }
    }

    fn ensure(&self) -> Arc<ProbeWorkerSupervisorState> {
        let mut state_slot = self.state.lock().unwrap();
        if let Some(state) = state_slot.as_ref() {
            // Keep the completed/failed supervisor handle owned.  Replacing
            // this state would drop a JoinHandle without joining it and could
            // strand the bounded in-flight residue it was responsible for.
            return Arc::clone(state);
        }

        let (sender, receiver) = mpsc::channel(MAX_PROVIDER_IN_FLIGHT_ENTRIES);
        let state = Arc::new(ProbeWorkerSupervisorState {
            sender,
            shutdown: Notify::new(),
            task: Mutex::new(None),
        });
        let task_state = Arc::clone(&state);
        let task = tokio::spawn(run_probe_worker_supervisor(receiver, task_state));
        *state.task.lock().unwrap() = Some(task);
        *state_slot = Some(Arc::clone(&state));
        state
    }

    fn shutdown(&self) {
        let state = self.state.lock().unwrap().take();
        if let Some(state) = state {
            let worker_deadline = Instant::now() + PROVIDER_WORKER_SHUTDOWN_TIMEOUT;
            state.shutdown.notify_one();
            let task = state.task.lock().unwrap().take();
            if let Some(task) = task {
                let reaper = ProbeWorkerReaper {
                    task,
                    worker_deadline: tokio::time::Instant::from_std(worker_deadline),
                };
                if let Ok(handle) = tokio::runtime::Handle::try_current() {
                    handle.spawn(reaper.join_until_deadline());
                } else {
                    reaper.task.abort();
                }
            }
        }
    }
}

async fn run_probe_worker_supervisor(
    mut receiver: mpsc::Receiver<ProbeWorkerRequest>,
    state: Arc<ProbeWorkerSupervisorState>,
) {
    let mut workers = JoinSet::new();
    let mut accepting = true;

    loop {
        if !accepting && workers.is_empty() {
            break;
        }

        tokio::select! {
            _ = state.shutdown.notified(), if accepting => {
                accepting = false;
                receiver.close();
            }
            request = receiver.recv(), if accepting => {
                match request {
                    Some(request) => {
                        workers.spawn(async move {
                            let flight = Arc::clone(&request.flight);
                            let key = request.key.clone();
                            let in_flight = Arc::clone(&request.in_flight);
                            match std::panic::AssertUnwindSafe(run_probe_worker(request))
                                .catch_unwind()
                                .await
                            {
                                Ok(completion) => completion,
                                Err(_) => ProbeWorkerCompletion {
                                    flight,
                                    key,
                                    in_flight,
                                    result: Err(ProviderError::Probe(
                                        crate::providers::adapter::ProviderProbeError::Io(
                                            crate::providers::adapter::ProviderProbeIoError::WaitFailed,
                                        ),
                                    )),
                                },
                            }
                        });
                    }
                    None => accepting = false,
                }
            }
            completion = workers.join_next(), if !workers.is_empty() => {
                if let Some(Ok(completion)) = completion {
                    publish_probe_completion(completion);
                }
            }
        }
    }

    while let Some(joined) = workers.join_next().await {
        if let Ok(completion) = joined {
            publish_probe_completion(completion);
        }
    }

    while let Ok(request) = receiver.try_recv() {
        request.flight.cancel();
        let completion = ProbeWorkerCompletion {
            flight: request.flight,
            key: request.key,
            in_flight: request.in_flight,
            result: Err(ProviderError::Probe(
                crate::providers::adapter::ProviderProbeError::TimedOut,
            )),
        };
        publish_probe_completion(completion);
    }
}

async fn run_probe_worker(request: ProbeWorkerRequest) -> ProbeWorkerCompletion {
    let ProbeWorkerRequest {
        flight,
        key,
        adapter,
        kind,
        config,
        cache,
        executable_inspector,
        in_flight,
        supervisor: _supervisor,
    } = request;

    // The timer only publishes cancellation. It never owns the
    // inspector/adapter future, so cancellation cannot drop blocking
    // selection or native work.
    let timer_flight: Weak<ProbeFlight> = Arc::downgrade(&flight);
    let timer = tokio::spawn(async move {
        let Some(flight) = timer_flight.upgrade() else {
            return;
        };
        tokio::time::sleep_until(tokio::time::Instant::from_std(flight.deadline)).await;
        flight.cancel();
    });

    // Selection is part of the retained worker. A caller timeout during
    // executable inspection therefore leaves the same bounded admission
    // charged until selection and the exact native leader have completed.
    let result = async {
        if flight.cancelled.load(Ordering::Acquire) {
            return Err(ProviderError::Probe(
                crate::providers::adapter::ProviderProbeError::TimedOut,
            ));
        }
        let (requested_path, before, before_handle) =
            ProviderRegistry::select_executable_with_inspector(
                &executable_inspector,
                kind,
                &config,
            )
            .await?;
        let identity_key = ProbeIdentityKey {
            kind,
            executable: before.clone(),
            launch_handle: before_handle.clone(),
            adapter_revision: TASK_4_1_ADAPTER_REVISION,
            semantic_schema_version: TASK_4_1_SEMANTIC_SCHEMA_VERSION,
        };
        ProviderRegistry::perform_probe(
            &cache,
            &executable_inspector,
            &adapter,
            &requested_path,
            &before,
            &before_handle,
            &identity_key,
        )
        .await
    }
    .await;
    timer.abort();

    ProbeWorkerCompletion {
        flight,
        key,
        in_flight,
        result,
    }
}

fn publish_probe_completion(completion: ProbeWorkerCompletion) {
    {
        let mut slot = completion.flight.result.lock().unwrap();
        if slot.is_none() {
            *slot = Some(completion.result);
        }
    }
    completion.flight.completed.notify_waiters();

    let mut in_flight = completion.in_flight.lock().unwrap();
    if in_flight
        .get(&completion.key)
        .is_some_and(|current| Arc::ptr_eq(current, &completion.flight))
    {
        in_flight.remove(&completion.key);
    }
}

struct ProbeRun {
    capabilities: ProviderCapabilities,
    executable: ProviderExecutable,
    executable_handle: ProviderExecutableHandle,
    had_matching_cached: bool,
    leader: bool,
}

struct CapabilityCacheEntry {
    capabilities: ProviderCapabilities,
    launch_handle: ProviderExecutableHandle,
    sequence: u64,
    inserted_at: Instant,
}

#[derive(Default)]
struct CapabilityCache {
    entries: HashMap<CapabilityCacheKey, CapabilityCacheEntry>,
    next_sequence: u64,
}

impl CapabilityCache {
    fn get(
        &mut self,
        key: &CapabilityCacheKey,
        launch_handle: &ProviderExecutableHandle,
    ) -> Option<ProviderCapabilities> {
        self.evict_expired(Instant::now());
        let entry = self.entries.get_mut(key)?;
        if entry.launch_handle != *launch_handle {
            return None;
        }
        self.next_sequence = self.next_sequence.saturating_add(1);
        entry.sequence = self.next_sequence;
        Some(entry.capabilities.clone())
    }

    fn insert(
        &mut self,
        key: CapabilityCacheKey,
        capabilities: ProviderCapabilities,
        launch_handle: ProviderExecutableHandle,
    ) {
        let now = Instant::now();
        self.evict_expired(now);
        self.entries.retain(|existing_key, entry| {
            !(existing_key.kind == key.kind
                && existing_key.adapter_revision == key.adapter_revision
                && existing_key.semantic_schema_version == key.semantic_schema_version
                && existing_key.executable.canonical_path() == key.executable.canonical_path()
                && (existing_key.executable != key.executable
                    || existing_key.version != key.version
                    || entry.launch_handle != launch_handle))
        });
        self.next_sequence = self.next_sequence.saturating_add(1);
        if self
            .entries
            .get(&key)
            .is_some_and(|entry| entry.launch_handle == launch_handle)
        {
            let entry = self.entries.get_mut(&key).expect("cache entry exists");
            entry.capabilities = capabilities;
            entry.sequence = self.next_sequence;
            entry.inserted_at = now;
            return;
        }
        self.entries.remove(&key);
        while self.entries.len() >= MAX_PROVIDER_CAPABILITY_CACHE_ENTRIES {
            let Some(oldest_key) = self
                .entries
                .iter()
                .min_by_key(|(_, entry)| entry.sequence)
                .map(|(key, _)| key.clone())
            else {
                break;
            };
            self.entries.remove(&oldest_key);
        }
        self.entries.insert(
            key,
            CapabilityCacheEntry {
                capabilities,
                launch_handle,
                sequence: self.next_sequence,
                inserted_at: now,
            },
        );
    }

    fn matching_entries(
        &mut self,
        identity: &ProbeIdentityKey,
        launch_handle: &ProviderExecutableHandle,
    ) -> Vec<(CapabilityCacheKey, ProviderCapabilities)> {
        self.evict_expired(Instant::now());
        self.entries
            .iter()
            .filter(|(key, _)| {
                key.kind == identity.kind
                    && key.executable == identity.executable
                    && key.adapter_revision == identity.adapter_revision
                    && key.semantic_schema_version == identity.semantic_schema_version
            })
            .filter(|(_, entry)| entry.launch_handle == *launch_handle)
            .map(|(key, entry)| (key.clone(), entry.capabilities.clone()))
            .collect()
    }

    fn len(&mut self) -> usize {
        self.evict_expired(Instant::now());
        self.entries.len()
    }

    fn clear(&mut self) {
        self.entries.clear();
    }

    fn evict_expired(&mut self, now: Instant) {
        self.entries.retain(|_, entry| {
            now.saturating_duration_since(entry.inserted_at) <= PROVIDER_CAPABILITY_CACHE_TTL
        });
    }
}

pub struct ProviderRegistry {
    adapters: BTreeMap<ProviderKind, Arc<dyn ProviderAdapter>>,
    cache: Arc<Mutex<CapabilityCache>>,
    in_flight: Arc<Mutex<HashMap<ProbeReservationKey, Arc<ProbeFlight>>>>,
    worker_supervisor: ProbeWorkerSupervisor,
    executable_inspector: Arc<dyn ExecutableInspector>,
    auth_evidence: Mutex<ProviderAuthEvidenceRegistry>,
}

impl Drop for ProviderRegistry {
    fn drop(&mut self) {
        if let Ok(in_flight) = self.in_flight.lock() {
            for flight in in_flight.values() {
                flight.cancel();
            }
        }
        self.worker_supervisor.shutdown();
    }
}

impl ProviderRegistry {
    pub fn new() -> Self {
        Self::with_executable_inspector(Arc::new(FileSystemExecutableInspector))
    }

    pub fn with_executable_inspector(inspector: Arc<dyn ExecutableInspector>) -> Self {
        Self {
            adapters: BTreeMap::new(),
            cache: Arc::new(Mutex::new(CapabilityCache::default())),
            in_flight: Arc::new(Mutex::new(HashMap::new())),
            worker_supervisor: ProbeWorkerSupervisor::new(),
            executable_inspector: inspector,
            auth_evidence: Mutex::new(ProviderAuthEvidenceRegistry::new()),
        }
    }

    pub fn register(&mut self, adapter: Arc<dyn ProviderAdapter>) -> Result<(), ProviderError> {
        let kind = adapter.kind();
        if self.adapters.contains_key(&kind) {
            return Err(ProviderError::DuplicateProviderKind(kind));
        }
        self.adapters.insert(kind, adapter);
        Ok(())
    }

    pub async fn resolve_executable(
        &self,
        kind: ProviderKind,
        config: &ProviderDiscoveryConfig,
    ) -> Result<ProviderExecutable, ProviderError> {
        let (_, identity, _) = self.select_executable(kind, config).await?;
        Ok(identity)
    }

    pub async fn observe(
        &self,
        kind: ProviderKind,
        config: &ProviderDiscoveryConfig,
    ) -> Result<ProviderObservation, ProviderError> {
        self.observe_internal(kind, config, None)
            .await
            .map(|(observation, _)| observation)
    }

    pub async fn observe_with_auth_receipt(
        &self,
        kind: ProviderKind,
        config: &ProviderDiscoveryConfig,
        receipt: ProviderAuthEvidenceReceipt,
    ) -> Result<ProviderObservation, ProviderError> {
        self.observe_internal(kind, config, Some(receipt))
            .await
            .map(|(observation, _)| observation)
    }

    pub async fn begin_auth_probe(
        &self,
        kind: ProviderKind,
        config: &ProviderDiscoveryConfig,
        ttl: Duration,
    ) -> Result<ProviderAuthProbeInvocation, ProviderError> {
        if !self.adapters.contains_key(&kind) {
            return Err(ProviderError::ProviderNotRegistered(kind));
        }
        let (observation, executable_handle) = self.observe_internal(kind, config, None).await?;
        self.auth_evidence
            .lock()
            .unwrap()
            .begin_with_handle_and_version(
                kind,
                crate::providers::capabilities::ProviderAuthEvidenceSource::for_kind(kind),
                executable_handle,
                observation.version,
                ttl,
            )
            .map_err(ProviderError::AuthEvidence)
    }

    pub fn accept_auth_probe(
        &self,
        expected_kind: ProviderKind,
        expected_executable: &ProviderExecutable,
        invocation: ProviderAuthProbeInvocation,
        result: ProviderAuthProbeResult,
        observed_at: std::time::Instant,
    ) -> Result<ProviderAuthEvidenceReceipt, ProviderError> {
        self.auth_evidence
            .lock()
            .unwrap()
            .accept_at_for(
                expected_kind,
                expected_executable,
                invocation,
                result,
                observed_at,
            )
            .map_err(ProviderError::AuthEvidence)
    }

    /// Accept only a result carrying the private proof emitted by the
    /// crate-owned bounded probe runner.  Public callers may submit the
    /// runner's opaque result, but cannot choose an auth state or timestamp.
    pub fn accept_auth_probe_result(
        &self,
        invocation: ProviderAuthProbeInvocation,
        request: ProviderProbeRequest,
        result: ProviderProbeResult,
    ) -> Result<ProviderAuthEvidenceReceipt, ProviderError> {
        if request.executable() != invocation.executable_handle()
            || !request.auth_binding_matches(&invocation)
        {
            return Err(ProviderError::AuthEvidence(
                ProviderAuthEvidenceError::RequestBindingMismatch,
            ));
        }
        let observation = result
            .into_auth_observation(&invocation, &request)
            .map_err(ProviderError::AuthEvidence)?;
        self.auth_evidence
            .lock()
            .unwrap()
            .accept_observation(invocation, observation)
            .map_err(ProviderError::AuthEvidence)
    }

    pub(crate) fn accept_auth_probe_observation(
        &self,
        invocation: ProviderAuthProbeInvocation,
        request: &ProviderProbeRequest,
        result: ProviderProbeResult,
    ) -> Result<ProviderAuthEvidenceReceipt, ProviderError> {
        if request.executable() != invocation.executable_handle()
            || !request.auth_binding_matches(&invocation)
        {
            return Err(ProviderError::AuthEvidence(
                ProviderAuthEvidenceError::RequestBindingMismatch,
            ));
        }
        let observation = result
            .into_auth_observation(&invocation, request)
            .map_err(ProviderError::AuthEvidence)?;
        self.auth_evidence
            .lock()
            .unwrap()
            .accept_observation(invocation, observation)
            .map_err(ProviderError::AuthEvidence)
    }

    pub fn auth_pending_len(&self) -> usize {
        self.auth_evidence.lock().unwrap().pending_len()
    }

    pub fn auth_accepted_len(&self) -> usize {
        self.auth_evidence.lock().unwrap().accepted_len()
    }

    async fn observe_internal(
        &self,
        kind: ProviderKind,
        config: &ProviderDiscoveryConfig,
        receipt: Option<ProviderAuthEvidenceReceipt>,
    ) -> Result<(ProviderObservation, ProviderExecutableHandle), ProviderError> {
        let adapter = self
            .adapters
            .get(&kind)
            .cloned()
            .ok_or(ProviderError::ProviderNotRegistered(kind))?;
        // Admission is keyed by the complete discovery request because the
        // executable identity is not available until selection finishes. The
        // retained flight owns selection as well as probing, so cancellation
        // cannot drop a potentially blocking inspector task before a bounded
        // slot has been charged.
        let reservation_key = ProbeReservationKey::new(kind, config);
        let mut worker_config = config.clone();
        if worker_config.path.is_none() {
            worker_config.path = reservation_key.path.clone();
        }
        let probe = self
            .probe_once(Arc::clone(&adapter), kind, worker_config, reservation_key)
            .await?;

        let version = probe.capabilities.version.clone();
        let key = CapabilityCacheKey::new(
            kind,
            probe.executable.clone(),
            version.clone(),
            TASK_4_1_ADAPTER_REVISION,
            TASK_4_1_SEMANTIC_SCHEMA_VERSION,
        );
        let stable = if probe.had_matching_cached {
            self.cache
                .lock()
                .unwrap()
                .get(&key, &probe.executable_handle)
        } else {
            None
        }
        .or_else(|| {
            if !probe.leader {
                self.cache
                    .lock()
                    .unwrap()
                    .get(&key, &probe.executable_handle)
            } else {
                None
            }
        });
        let stable = stable.unwrap_or_else(|| probe.capabilities.stable_projection());
        let capabilities = match receipt {
            Some(receipt) => {
                let consumed = self
                    .auth_evidence
                    .lock()
                    .unwrap()
                    .consume_at_for_handle(
                        kind,
                        &probe.executable_handle,
                        &probe.capabilities.version,
                        receipt,
                    )
                    .map_err(ProviderError::AuthEvidence)?;
                stable.with_auth_receipt(&consumed)?
            }
            None => stable,
        };
        let cache_status = if probe.had_matching_cached || !probe.leader {
            CacheStatus::Hit
        } else {
            CacheStatus::Miss
        };

        let observation = ProviderObservation {
            kind,
            executable: probe.executable,
            version,
            adapter_revision: TASK_4_1_ADAPTER_REVISION,
            semantic_schema_version: TASK_4_1_SEMANTIC_SCHEMA_VERSION,
            capabilities,
            cache_status,
        };
        observation.validate()?;
        Ok((observation, probe.executable_handle))
    }

    async fn probe_once(
        &self,
        adapter: Arc<dyn ProviderAdapter>,
        kind: ProviderKind,
        config: ProviderDiscoveryConfig,
        key: ProbeReservationKey,
    ) -> Result<ProbeRun, ProviderError> {
        let (flight, leader) = {
            let mut in_flight = self.in_flight.lock().unwrap();
            let now = Instant::now();
            let expired: Vec<_> = in_flight
                .iter()
                .filter(|(_, flight)| {
                    now.saturating_duration_since(flight.started_at) > PROVIDER_IN_FLIGHT_TTL
                })
                .map(|(key, flight)| (key.clone(), Arc::clone(flight)))
                .collect();
            for (_expired_key, expired_flight) in expired {
                expired_flight.cancel();
            }
            if let Some(flight) = in_flight.get(&key) {
                (Arc::clone(flight), false)
            } else {
                if in_flight.len() >= MAX_PROVIDER_IN_FLIGHT_ENTRIES {
                    // Cancellation is cooperative for the async layer, but
                    // the adapter may own an uncancellable native leader.
                    // Keep that flight in the map until its worker joins so
                    // the capacity charge cannot be evicted and immediately
                    // replaced by unbounded native work.
                    if let Some((_, oldest_flight)) =
                        in_flight.iter().min_by_key(|(_, flight)| flight.started_at)
                    {
                        oldest_flight.cancel();
                    }
                    return Err(ProviderError::Probe(
                        crate::providers::adapter::ProviderProbeError::TimedOut,
                    ));
                }
                let flight = Arc::new(ProbeFlight {
                    result: Mutex::new(None),
                    completed: Notify::new(),
                    started_at: now,
                    deadline: now + PROVIDER_IN_FLIGHT_TTL,
                    cancelled: AtomicBool::new(false),
                    cancellation: Notify::new(),
                });
                in_flight.insert(key.clone(), Arc::clone(&flight));
                (flight, true)
            }
        };

        if leader {
            let mut leader_cleanup = ProbeLeaderCleanup::new(Arc::clone(&flight));
            if let Err(error) =
                self.spawn_probe_worker(Arc::clone(&flight), key.clone(), adapter, kind, config)
            {
                self.remove_probe_flight(&key, &flight);
                return Err(error);
            }
            let published = Self::await_probe_flight(Arc::clone(&flight)).await;
            leader_cleanup.disarm();
            published.map(|probe| ProbeRun {
                capabilities: probe.capabilities,
                executable: probe.executable,
                executable_handle: probe.executable_handle,
                had_matching_cached: probe.had_matching_cached,
                leader: true,
            })
        } else {
            Self::await_probe_flight(Arc::clone(&flight))
                .await
                .map(|probe| ProbeRun {
                    capabilities: probe.capabilities,
                    executable: probe.executable,
                    executable_handle: probe.executable_handle,
                    had_matching_cached: probe.had_matching_cached,
                    leader: false,
                })
        }
    }

    fn spawn_probe_worker(
        &self,
        flight: Arc<ProbeFlight>,
        key: ProbeReservationKey,
        adapter: Arc<dyn ProviderAdapter>,
        kind: ProviderKind,
        config: ProviderDiscoveryConfig,
    ) -> Result<(), ProviderError> {
        let in_flight = Arc::clone(&self.in_flight);
        let supervisor = self.worker_supervisor.ensure();
        self.worker_supervisor.submit(ProbeWorkerRequest {
            flight,
            key,
            adapter,
            kind,
            config,
            cache: Arc::clone(&self.cache),
            executable_inspector: Arc::clone(&self.executable_inspector),
            in_flight,
            supervisor,
        })
    }

    fn remove_probe_flight(&self, key: &ProbeReservationKey, flight: &Arc<ProbeFlight>) {
        let mut in_flight = self.in_flight.lock().unwrap();
        if in_flight
            .get(key)
            .is_some_and(|current| Arc::ptr_eq(current, flight))
        {
            in_flight.remove(key);
        }
    }

    async fn await_probe_flight(
        flight: Arc<ProbeFlight>,
    ) -> Result<ProbeWorkResult, ProviderError> {
        loop {
            if let Some(result) = flight.result.lock().unwrap().clone() {
                return result;
            }

            let mut completed = std::pin::pin!(flight.completed.notified());
            let mut cancellation = std::pin::pin!(flight.cancellation.notified());
            completed.as_mut().enable();
            cancellation.as_mut().enable();
            if let Some(result) = flight.result.lock().unwrap().clone() {
                return result;
            }
            tokio::select! {
                _ = &mut completed => {}
                _ = &mut cancellation => {}
            }
        }
    }

    async fn perform_probe(
        cache: &Arc<Mutex<CapabilityCache>>,
        executable_inspector: &Arc<dyn ExecutableInspector>,
        adapter: &Arc<dyn ProviderAdapter>,
        requested_path: &Path,
        before: &ProviderExecutable,
        before_handle: &ProviderExecutableHandle,
        identity_key: &ProbeIdentityKey,
    ) -> Result<ProbeWorkResult, ProviderError> {
        let capabilities = adapter.probe(before_handle).await?;
        if capabilities.kind != identity_key.kind {
            return Err(ProviderError::CapabilityKindMismatch {
                expected: identity_key.kind,
                actual: capabilities.kind,
            });
        }
        if capabilities.auth_state() != crate::providers::capabilities::ProviderAuthState::Unknown
            || capabilities.evidence().iter().any(|evidence| {
                evidence.source()
                    == crate::providers::capabilities::EvidenceSourceId::AuthStatusProbe
                    || evidence.auth_source().is_some()
                    || matches!(
                        evidence.status(),
                        crate::providers::capabilities::EvidenceStatus::Authenticated
                            | crate::providers::capabilities::EvidenceStatus::AuthRequired
                    )
            })
        {
            return Err(ProviderError::UntrustedAuthenticationEvidence);
        }
        capabilities.validate()?;

        // Reinspect after every capability probe and before any cache write.
        let after =
            executable_inspector
                .inspect(requested_path)
                .await
                .map_err(|error| match error {
                    ProviderExecutableError::Missing(_) | ProviderExecutableError::NotAFile(_) => {
                        ProviderError::MissingCli {
                            kind: identity_key.kind,
                            requested: Some(requested_path.to_path_buf()),
                        }
                    }
                    other => ProviderError::Executable(other),
                })?;
        let contract = ProviderDiscoveryContract::for_kind(identity_key.kind);
        if after.is_native() {
            contract
                .validate_executable(&after)
                .map_err(ProviderError::Discovery)?;
        }
        if before != &after {
            return Err(ProviderError::ExecutableChanged {
                before: before.clone(),
                after,
            });
        }

        before_handle
            .revalidate()
            .map_err(ProviderError::Executable)?;

        let cache_key = CapabilityCacheKey::new(
            identity_key.kind,
            after.clone(),
            capabilities.version.clone(),
            identity_key.adapter_revision,
            identity_key.semantic_schema_version,
        );
        let had_matching_cached = cache
            .lock()
            .unwrap()
            .matching_entries(identity_key, before_handle)
            .iter()
            .any(|(existing_key, _)| existing_key == &cache_key);
        cache.lock().unwrap().insert(
            cache_key,
            capabilities.stable_projection(),
            before_handle.clone(),
        );
        Ok(ProbeWorkResult {
            capabilities,
            executable: after,
            executable_handle: before_handle.clone(),
            had_matching_cached,
        })
    }

    async fn select_executable(
        &self,
        kind: ProviderKind,
        config: &ProviderDiscoveryConfig,
    ) -> Result<(PathBuf, ProviderExecutable, ProviderExecutableHandle), ProviderError> {
        Self::select_executable_with_inspector(&self.executable_inspector, kind, config).await
    }

    async fn select_executable_with_inspector(
        executable_inspector: &Arc<dyn ExecutableInspector>,
        kind: ProviderKind,
        config: &ProviderDiscoveryConfig,
    ) -> Result<(PathBuf, ProviderExecutable, ProviderExecutableHandle), ProviderError> {
        let contract = ProviderDiscoveryContract::for_kind(kind);

        if let Some(override_path) = &config.executable_override {
            let candidate = contract
                .validate(ProviderDiscoveryCandidateInput::configured_override(
                    override_path.clone(),
                ))
                .map_err(|error| map_discovery_error(kind, Some(override_path), error))?;
            let identity = executable_inspector
                .inspect(override_path)
                .await
                .map_err(|error| match error {
                    ProviderExecutableError::Missing(_) | ProviderExecutableError::NotAFile(_) => {
                        ProviderError::MissingCli {
                            kind,
                            requested: Some(override_path.clone()),
                        }
                    }
                    other => ProviderError::Executable(other),
                })?;
            if candidate.executable() != &identity {
                return Err(ProviderError::ExecutableChanged {
                    before: candidate.executable().clone(),
                    after: identity,
                });
            }
            let handle = candidate
                .open_for_launch()
                .map_err(ProviderError::Executable)?;
            return Ok((candidate.requested_path().to_path_buf(), identity, handle));
        }

        let path_value = config.path.clone().or_else(|| std::env::var_os("PATH"));
        let Some(path_value) = path_value else {
            return Err(ProviderError::MissingCli {
                kind,
                requested: None,
            });
        };

        let snapshot = crate::providers::capabilities::ProviderPathSnapshot::capture(path_value)
            .map_err(ProviderError::Discovery)?;
        let candidate = contract
            .resolve_from_path_snapshot(&snapshot)
            .map_err(|error| map_discovery_error(kind, None, error))?;
        let handle = candidate
            .open_for_launch()
            .map_err(ProviderError::Executable)?;
        Ok((
            candidate.requested_path().to_path_buf(),
            candidate.executable().clone(),
            handle,
        ))
    }

    pub fn cache_len(&self) -> usize {
        self.cache.lock().unwrap().len()
    }

    pub fn in_flight_len(&self) -> usize {
        self.in_flight.lock().unwrap().len()
    }

    pub fn clear_cache(&self) {
        self.cache.lock().unwrap().clear();
    }
}

impl Default for ProviderRegistry {
    fn default() -> Self {
        Self::new()
    }
}

fn map_discovery_error(
    kind: ProviderKind,
    requested: Option<&Path>,
    error: ProviderDiscoveryError,
) -> ProviderError {
    match error {
        ProviderDiscoveryError::NoCandidate(_) => ProviderError::MissingCli {
            kind,
            requested: requested.map(Path::to_path_buf),
        },
        ProviderDiscoveryError::ForbiddenRunner(path) => {
            ProviderError::WrapperCommandNotAllowed { path }
        }
        ProviderDiscoveryError::WrongEntrypoint(path)
        | ProviderDiscoveryError::WrongFileType(path) => {
            ProviderError::ExecutableNotAllowed { kind, path }
        }
        ProviderDiscoveryError::Executable(
            ProviderExecutableError::Missing(_) | ProviderExecutableError::NotAFile(_),
        ) => ProviderError::MissingCli {
            kind,
            requested: requested.map(Path::to_path_buf),
        },
        ProviderDiscoveryError::Executable(error) => ProviderError::Executable(error),
        other => ProviderError::Discovery(other),
    }
}
