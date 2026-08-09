use crate::providers::adapter::{ProviderAdapter, ProviderError};
use crate::providers::capabilities::{
    AdapterRevision, ProviderCapabilities, ProviderExecutable, ProviderExecutableError,
    ProviderExecutablePolicy, ProviderKind, ProviderVersion, SemanticSchemaVersion,
};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tokio::sync::Notify;

const TASK_4_1_ADAPTER_REVISION: AdapterRevision = AdapterRevision::new(1);
const TASK_4_1_SEMANTIC_SCHEMA_VERSION: SemanticSchemaVersion = SemanticSchemaVersion::new(1);

#[derive(Debug, Clone, Default)]
pub struct ProviderDiscoveryConfig {
    pub executable_override: Option<PathBuf>,
    pub path: Option<OsString>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CacheStatus {
    Hit,
    Miss,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct CapabilityCacheKey {
    pub kind: ProviderKind,
    pub executable: ProviderExecutable,
    pub version: ProviderVersion,
    pub adapter_revision: AdapterRevision,
    pub semantic_schema_version: SemanticSchemaVersion,
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderObservation {
    pub kind: ProviderKind,
    pub executable: ProviderExecutable,
    pub version: ProviderVersion,
    pub adapter_revision: AdapterRevision,
    pub semantic_schema_version: SemanticSchemaVersion,
    pub capabilities: ProviderCapabilities,
    pub cache_status: CacheStatus,
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
        tokio::task::spawn_blocking(move || ProviderExecutable::inspect_blocking(&path))
            .await
            .map_err(|_| ProviderExecutableError::BackgroundTask)?
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ProbeIdentityKey {
    kind: ProviderKind,
    executable: ProviderExecutable,
    adapter_revision: AdapterRevision,
    semantic_schema_version: SemanticSchemaVersion,
}

struct ProbeFlight {
    result: Mutex<Option<Result<(ProviderCapabilities, ProviderExecutable), ProviderError>>>,
    completed: Notify,
}

struct ProbeRun {
    capabilities: ProviderCapabilities,
    executable: ProviderExecutable,
    leader: bool,
}

pub struct ProviderRegistry {
    adapters: BTreeMap<ProviderKind, Arc<dyn ProviderAdapter>>,
    cache: Mutex<HashMap<CapabilityCacheKey, ProviderCapabilities>>,
    in_flight: Mutex<HashMap<ProbeIdentityKey, Arc<ProbeFlight>>>,
    executable_inspector: Arc<dyn ExecutableInspector>,
}

impl ProviderRegistry {
    pub fn new() -> Self {
        Self::with_executable_inspector(Arc::new(FileSystemExecutableInspector))
    }

    pub fn with_executable_inspector(inspector: Arc<dyn ExecutableInspector>) -> Self {
        Self {
            adapters: BTreeMap::new(),
            cache: Mutex::new(HashMap::new()),
            in_flight: Mutex::new(HashMap::new()),
            executable_inspector: inspector,
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
        let (_, identity) = self.select_executable(kind, config).await?;
        Ok(identity)
    }

    pub async fn observe(
        &self,
        kind: ProviderKind,
        config: &ProviderDiscoveryConfig,
    ) -> Result<ProviderObservation, ProviderError> {
        let adapter = self
            .adapters
            .get(&kind)
            .cloned()
            .ok_or(ProviderError::ProviderNotRegistered(kind))?;
        let (requested_path, before) = self.select_executable(kind, config).await?;
        let identity_key = ProbeIdentityKey {
            kind,
            executable: before.clone(),
            adapter_revision: TASK_4_1_ADAPTER_REVISION,
            semantic_schema_version: TASK_4_1_SEMANTIC_SCHEMA_VERSION,
        };
        let cached_before = self.cached_identity_entries(&identity_key);
        let probe = self
            .probe_once(
                Arc::clone(&adapter),
                requested_path.clone(),
                before.clone(),
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
                self.cache.lock().unwrap().get(&key).cloned()
            } else {
                None
            }
        });
        let capabilities = match stable {
            Some(stable) => stable.with_fresh_auth_status(&probe.capabilities)?,
            None => probe.capabilities.clone(),
        };
        let cache_status = if had_matching_cached || (!probe.leader && cached_before.is_empty()) {
            CacheStatus::Hit
        } else {
            CacheStatus::Miss
        };

        Ok(ProviderObservation {
            kind,
            executable: probe.executable,
            version,
            adapter_revision: TASK_4_1_ADAPTER_REVISION,
            semantic_schema_version: TASK_4_1_SEMANTIC_SCHEMA_VERSION,
            capabilities,
            cache_status,
        })
    }

    async fn probe_once(
        &self,
        adapter: Arc<dyn ProviderAdapter>,
        requested_path: PathBuf,
        before: ProviderExecutable,
        key: ProbeIdentityKey,
    ) -> Result<ProbeRun, ProviderError> {
        let (flight, leader) = {
            let mut in_flight = self.in_flight.lock().unwrap();
            if let Some(flight) = in_flight.get(&key) {
                (Arc::clone(flight), false)
            } else {
                let flight = Arc::new(ProbeFlight {
                    result: Mutex::new(None),
                    completed: Notify::new(),
                });
                in_flight.insert(key.clone(), Arc::clone(&flight));
                (flight, true)
            }
        };

        if leader {
            let result = self
                .perform_probe(&adapter, &requested_path, &before, &key)
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
            result.map(|(capabilities, executable)| ProbeRun {
                capabilities,
                executable,
                leader: true,
            })
        } else {
            loop {
                if let Some(result) = flight.result.lock().unwrap().clone() {
                    return result.map(|(capabilities, executable)| ProbeRun {
                        capabilities,
                        executable,
                        leader: false,
                    });
                }
                flight.completed.notified().await;
            }
        }
    }

    async fn perform_probe(
        &self,
        adapter: &Arc<dyn ProviderAdapter>,
        requested_path: &Path,
        before: &ProviderExecutable,
        identity_key: &ProbeIdentityKey,
    ) -> Result<(ProviderCapabilities, ProviderExecutable), ProviderError> {
        let capabilities = adapter.probe(before.canonical_path()).await?;
        if capabilities.kind != identity_key.kind {
            return Err(ProviderError::CapabilityKindMismatch {
                expected: identity_key.kind,
                actual: capabilities.kind,
            });
        }
        capabilities.validate()?;

        // AuthStatusProbe is part of the adapter's probe contract. This
        // inspection happens after every version/capability/auth probe and
        // before any cache write.
        let after = self
            .executable_inspector
            .inspect(requested_path)
            .await
            .map_err(|error| {
                map_executable_error(identity_key.kind, Some(requested_path), error)
            })?;
        validate_policy(identity_key.kind, &after)?;
        if before != &after {
            return Err(ProviderError::ExecutableChanged {
                before: before.clone(),
                after,
            });
        }

        let cache_key = CapabilityCacheKey::new(
            identity_key.kind,
            after.clone(),
            capabilities.version.clone(),
            identity_key.adapter_revision,
            identity_key.semantic_schema_version,
        );
        self.cache
            .lock()
            .unwrap()
            .insert(cache_key, capabilities.without_auth_status());
        Ok((capabilities, after))
    }

    async fn select_executable(
        &self,
        kind: ProviderKind,
        config: &ProviderDiscoveryConfig,
    ) -> Result<(PathBuf, ProviderExecutable), ProviderError> {
        let policy = policy_for_kind(kind)?;

        if let Some(override_path) = &config.executable_override {
            let identity = self
                .executable_inspector
                .inspect(override_path)
                .await
                .map_err(|error| map_executable_error(kind, Some(override_path), error))?;
            validate_policy(kind, &identity)?;
            return Ok((override_path.clone(), identity));
        }

        let path_value = config.path.clone().or_else(|| std::env::var_os("PATH"));
        let Some(path_value) = path_value else {
            return Err(ProviderError::MissingCli {
                kind,
                requested: None,
            });
        };

        for directory in std::env::split_paths(&path_value) {
            for name in policy.entrypoints() {
                let candidate = directory.join(name);
                let identity = match self.executable_inspector.inspect(&candidate).await {
                    Ok(identity) => identity,
                    Err(
                        ProviderExecutableError::Missing(_) | ProviderExecutableError::NotAFile(_),
                    ) => continue,
                    Err(error) => {
                        return Err(map_executable_error(kind, Some(&candidate), error));
                    }
                };
                validate_policy(kind, &identity)?;
                return Ok((candidate, identity));
            }
        }

        Err(ProviderError::MissingCli {
            kind,
            requested: None,
        })
    }

    fn cached_identity_entries(
        &self,
        identity: &ProbeIdentityKey,
    ) -> Vec<(CapabilityCacheKey, ProviderCapabilities)> {
        self.cache
            .lock()
            .unwrap()
            .iter()
            .filter(|(key, _)| {
                key.kind == identity.kind
                    && key.executable == identity.executable
                    && key.adapter_revision == identity.adapter_revision
                    && key.semantic_schema_version == identity.semantic_schema_version
            })
            .map(|(key, capabilities)| (key.clone(), capabilities.clone()))
            .collect()
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

fn policy_for_kind(kind: ProviderKind) -> Result<ProviderExecutablePolicy, ProviderError> {
    let entrypoints = match kind {
        ProviderKind::ClaudeCode => ["claude"],
        ProviderKind::Codex => ["codex"],
        ProviderKind::Cursor => ["cursor-agent"],
    };
    ProviderExecutablePolicy::new(entrypoints).map_err(ProviderError::InvalidExecutablePolicy)
}

fn validate_policy(kind: ProviderKind, identity: &ProviderExecutable) -> Result<(), ProviderError> {
    let policy = policy_for_kind(kind)?;
    match policy.validate_canonical_path(identity.canonical_path()) {
        Ok(()) => Ok(()),
        Err(crate::providers::capabilities::ProviderExecutablePolicyViolation::ForbiddenRunner) => {
            Err(ProviderError::WrapperCommandNotAllowed {
                path: identity.canonical_path().to_path_buf(),
            })
        }
        Err(crate::providers::capabilities::ProviderExecutablePolicyViolation::NotDeclared) => {
            Err(ProviderError::ExecutableNotAllowed {
                kind,
                path: identity.canonical_path().to_path_buf(),
            })
        }
    }
}

fn map_executable_error(
    kind: ProviderKind,
    requested: Option<&Path>,
    error: ProviderExecutableError,
) -> ProviderError {
    match error {
        ProviderExecutableError::Missing(_) | ProviderExecutableError::NotAFile(_) => {
            ProviderError::MissingCli {
                kind,
                requested: requested.map(Path::to_path_buf),
            }
        }
        other => ProviderError::Executable(other),
    }
}
