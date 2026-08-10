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
use serde::de;
use serde::ser::SerializeStruct;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::collections::{BTreeMap, HashMap};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::Notify;

const TASK_4_1_ADAPTER_REVISION: AdapterRevision = AdapterRevision::new(1);
const TASK_4_1_SEMANTIC_SCHEMA_VERSION: SemanticSchemaVersion = SemanticSchemaVersion::new(1);
const PROVIDER_CAPABILITY_CACHE_TTL: Duration = Duration::from_secs(5 * 60);
const MAX_PROVIDER_IN_FLIGHT_ENTRIES: usize = 64;
const PROVIDER_IN_FLIGHT_TTL: Duration = Duration::from_secs(30);

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
struct ProbeIdentityKey {
    kind: ProviderKind,
    executable: ProviderExecutable,
    launch_handle: ProviderExecutableHandle,
    adapter_revision: AdapterRevision,
    semantic_schema_version: SemanticSchemaVersion,
}

struct ProbeFlight {
    result: Mutex<
        Option<
            Result<
                (
                    ProviderCapabilities,
                    ProviderExecutable,
                    ProviderExecutableHandle,
                ),
                ProviderError,
            >,
        >,
    >,
    completed: Notify,
    started_at: Instant,
}

struct ProbeRun {
    capabilities: ProviderCapabilities,
    executable: ProviderExecutable,
    executable_handle: ProviderExecutableHandle,
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
    cache: Mutex<CapabilityCache>,
    in_flight: Mutex<HashMap<ProbeIdentityKey, Arc<ProbeFlight>>>,
    executable_inspector: Arc<dyn ExecutableInspector>,
    auth_evidence: Mutex<ProviderAuthEvidenceRegistry>,
}

impl ProviderRegistry {
    pub fn new() -> Self {
        Self::with_executable_inspector(Arc::new(FileSystemExecutableInspector))
    }

    pub fn with_executable_inspector(inspector: Arc<dyn ExecutableInspector>) -> Self {
        Self {
            adapters: BTreeMap::new(),
            cache: Mutex::new(CapabilityCache::default()),
            in_flight: Mutex::new(HashMap::new()),
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

    pub(crate) fn accept_auth_probe_observation(
        &self,
        invocation: ProviderAuthProbeInvocation,
        request: &ProviderProbeRequest,
        result: &ProviderProbeResult,
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
        let (requested_path, before, before_handle) = self.select_executable(kind, config).await?;
        let identity_key = ProbeIdentityKey {
            kind,
            executable: before.clone(),
            launch_handle: before_handle.clone(),
            adapter_revision: TASK_4_1_ADAPTER_REVISION,
            semantic_schema_version: TASK_4_1_SEMANTIC_SCHEMA_VERSION,
        };
        let cached_before = self.cached_identity_entries(&identity_key, &before_handle);
        let probe = self
            .probe_once(
                Arc::clone(&adapter),
                requested_path.clone(),
                before.clone(),
                before_handle,
                identity_key,
            )
            .await?;

        // `probe_once` performs this recheck before inserting the stable
        // projection. The caller also uses the returned identity, so a
        // follower cannot accidentally report the pre-probe executable.
        if before != probe.executable {
            return Err(ProviderError::ExecutableChanged {
                before,
                after: probe.executable,
            });
        }

        let version = probe.capabilities.version.clone();
        let key = CapabilityCacheKey::new(
            kind,
            probe.executable.clone(),
            version.clone(),
            TASK_4_1_ADAPTER_REVISION,
            TASK_4_1_SEMANTIC_SCHEMA_VERSION,
        );
        let matching_cached = cached_before
            .iter()
            .find(|(cached_key, _)| cached_key == &key)
            .map(|(_, capabilities)| capabilities.clone());
        let had_matching_cached = matching_cached.is_some();
        let stable = matching_cached.or_else(|| {
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
        let cache_status = if had_matching_cached || !probe.leader {
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
        requested_path: PathBuf,
        before: ProviderExecutable,
        before_handle: ProviderExecutableHandle,
        key: ProbeIdentityKey,
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
            for (expired_key, expired_flight) in expired {
                *expired_flight.result.lock().unwrap() = Some(Err(ProviderError::Probe(
                    crate::providers::adapter::ProviderProbeError::TimedOut,
                )));
                expired_flight.completed.notify_waiters();
                in_flight.remove(&expired_key);
            }
            let replaced: Vec<_> = in_flight
                .iter()
                .filter(|(existing_key, _)| {
                    existing_key.kind == key.kind
                        && existing_key.adapter_revision == key.adapter_revision
                        && existing_key.semantic_schema_version == key.semantic_schema_version
                        && existing_key.executable.canonical_path()
                            == key.executable.canonical_path()
                        && existing_key.launch_handle != key.launch_handle
                })
                .map(|(existing_key, flight)| (existing_key.clone(), Arc::clone(flight)))
                .collect();
            for (replaced_key, replaced_flight) in replaced {
                *replaced_flight.result.lock().unwrap() = Some(Err(ProviderError::Probe(
                    crate::providers::adapter::ProviderProbeError::TimedOut,
                )));
                replaced_flight.completed.notify_waiters();
                in_flight.remove(&replaced_key);
            }
            if let Some(flight) = in_flight.get(&key) {
                (Arc::clone(flight), false)
            } else {
                while in_flight.len() >= MAX_PROVIDER_IN_FLIGHT_ENTRIES {
                    let Some((oldest_key, oldest_flight)) = in_flight
                        .iter()
                        .min_by_key(|(_, flight)| flight.started_at)
                        .map(|(key, flight)| (key.clone(), Arc::clone(flight)))
                    else {
                        break;
                    };
                    *oldest_flight.result.lock().unwrap() = Some(Err(ProviderError::Probe(
                        crate::providers::adapter::ProviderProbeError::TimedOut,
                    )));
                    oldest_flight.completed.notify_waiters();
                    in_flight.remove(&oldest_key);
                }
                let flight = Arc::new(ProbeFlight {
                    result: Mutex::new(None),
                    completed: Notify::new(),
                    started_at: now,
                });
                in_flight.insert(key.clone(), Arc::clone(&flight));
                (flight, true)
            }
        };

        if leader {
            let result = self
                .perform_probe(&adapter, &requested_path, &before, &before_handle, &key)
                .await;
            *flight.result.lock().unwrap() = Some(result.clone());
            flight.completed.notify_waiters();
            let mut in_flight = self.in_flight.lock().unwrap();
            if in_flight
                .get(&key)
                .is_some_and(|current| Arc::ptr_eq(current, &flight))
            {
                in_flight.remove(&key);
            }
            result.map(|(capabilities, executable, executable_handle)| ProbeRun {
                capabilities,
                executable,
                executable_handle,
                leader: true,
            })
        } else {
            loop {
                if let Some(result) = flight.result.lock().unwrap().clone() {
                    return result.map(|(capabilities, executable, executable_handle)| ProbeRun {
                        capabilities,
                        executable,
                        executable_handle,
                        leader: false,
                    });
                }
                tokio::time::timeout(PROVIDER_IN_FLIGHT_TTL, flight.completed.notified())
                    .await
                    .map_err(|_| {
                        ProviderError::Probe(
                            crate::providers::adapter::ProviderProbeError::TimedOut,
                        )
                    })?;
            }
        }
    }

    async fn perform_probe(
        &self,
        adapter: &Arc<dyn ProviderAdapter>,
        requested_path: &Path,
        before: &ProviderExecutable,
        before_handle: &ProviderExecutableHandle,
        identity_key: &ProbeIdentityKey,
    ) -> Result<
        (
            ProviderCapabilities,
            ProviderExecutable,
            ProviderExecutableHandle,
        ),
        ProviderError,
    > {
        let capabilities = adapter.probe(before).await?;
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
        let after = self
            .executable_inspector
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
        self.cache.lock().unwrap().insert(
            cache_key,
            capabilities.stable_projection(),
            before_handle.clone(),
        );
        Ok((capabilities, after, before_handle.clone()))
    }

    async fn select_executable(
        &self,
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
            let identity =
                self.executable_inspector
                    .inspect(override_path)
                    .await
                    .map_err(|error| match error {
                        ProviderExecutableError::Missing(_)
                        | ProviderExecutableError::NotAFile(_) => ProviderError::MissingCli {
                            kind,
                            requested: Some(override_path.clone()),
                        },
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

    fn cached_identity_entries(
        &self,
        identity: &ProbeIdentityKey,
        launch_handle: &ProviderExecutableHandle,
    ) -> Vec<(CapabilityCacheKey, ProviderCapabilities)> {
        self.cache
            .lock()
            .unwrap()
            .matching_entries(identity, launch_handle)
    }

    pub fn cache_len(&self) -> usize {
        self.cache.lock().unwrap().len()
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
