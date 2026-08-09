use async_trait::async_trait;
use devmanager::domain::{
    AgentRole, AgentSessionFacts, AgentSessionId, ProviderSessionId, TaskId,
    MAX_PROVIDER_SESSION_ID_BYTES,
};
use devmanager::providers::adapter::{
    JournalEvent, LaunchProviderRequest, ProviderAdapter, ProviderError, ProviderLaunchSpec,
    ProviderProbeError, ProviderProbeRequest, ProviderProbeResult, ProviderProbeRunner,
    ProviderRuntime, ProviderSignal, QuotaObservation, StopStrategy,
};
use devmanager::providers::capabilities::{
    AdapterRevision, CapabilityEvidence, CapabilityEvidenceError, CapabilitySupport,
    EvidenceDiagnostic, EvidenceDiagnosticCode, EvidenceSourceId, EvidenceStatus,
    ProviderAuthState, ProviderCapabilities, ProviderCapability, ProviderExecutable,
    ProviderExecutablePolicy, ProviderKind, ProviderVersion, ProviderVersionError,
    SemanticSchemaVersion, MAX_CAPABILITY_EVIDENCE_ITEMS,
};
use devmanager::providers::registry::{
    CacheStatus, CapabilityCacheKey, ExecutableInspector, FileSystemExecutableInspector,
    ProviderDiscoveryConfig, ProviderRegistry,
};
use serde_json::Value;
use std::ffi::OsString;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tempfile::tempdir;

#[cfg(windows)]
fn copied_probe_fixture(temp: &tempfile::TempDir, stem: &str) -> PathBuf {
    let path = temp.path().join(format!("{stem}.exe"));
    std::fs::copy(
        env!("CARGO_BIN_EXE_devmanager-provider-probe-fixture"),
        &path,
    )
    .expect("copy harmless provider probe fixture");
    path
}

#[cfg(windows)]
fn probe_runner(path: &Path) -> devmanager::providers::WindowsProviderProbeRunner {
    let file_name = path.file_name().unwrap().to_string_lossy().into_owned();
    devmanager::providers::WindowsProviderProbeRunner::new(
        ProviderExecutablePolicy::new([file_name]).expect("fixture allowlist"),
    )
}

struct FakeAdapter {
    kind: ProviderKind,
    version: Mutex<ProviderVersion>,
    capabilities: Mutex<ProviderCapabilities>,
    capability_probes: AtomicUsize,
    probe_delay: Mutex<Option<Duration>>,
    path_delay: Mutex<Option<(String, Duration)>>,
}

impl FakeAdapter {
    fn new(capabilities: ProviderCapabilities) -> Arc<Self> {
        Arc::new(Self {
            kind: capabilities.kind,
            version: Mutex::new(capabilities.version.clone()),
            capabilities: Mutex::new(capabilities),
            capability_probes: AtomicUsize::new(0),
            probe_delay: Mutex::new(None),
            path_delay: Mutex::new(None),
        })
    }

    fn set_version(&self, version: ProviderVersion) {
        *self.version.lock().unwrap() = version.clone();
        self.capabilities.lock().unwrap().version = version;
    }

    fn set_auth_state(&self, auth_state: ProviderAuthState) {
        let mut capabilities = self.capabilities.lock().unwrap();
        capabilities.auth_state = auth_state;
        capabilities.evidence = auth_evidence(auth_state, 1_700_000_000_100);
    }

    fn set_probe_delay(&self, delay: Duration) {
        *self.probe_delay.lock().unwrap() = Some(delay);
    }

    fn set_path_delay(&self, marker: impl Into<String>, delay: Duration) {
        *self.path_delay.lock().unwrap() = Some((marker.into(), delay));
    }
}

#[async_trait]
impl ProviderAdapter for FakeAdapter {
    fn kind(&self) -> ProviderKind {
        self.kind
    }

    async fn probe(&self, executable: &Path) -> Result<ProviderCapabilities, ProviderError> {
        self.capability_probes.fetch_add(1, Ordering::Relaxed);
        let delay = *self.probe_delay.lock().unwrap();
        if let Some(delay) = delay {
            tokio::time::sleep(delay).await;
        }
        let path_delay = self.path_delay.lock().unwrap().clone();
        if let Some((marker, delay)) = path_delay {
            if executable.to_string_lossy().contains(&marker) {
                tokio::time::sleep(delay).await;
            }
        }
        Ok(self.capabilities.lock().unwrap().clone())
    }

    fn build_launch(
        &self,
        _request: LaunchProviderRequest,
    ) -> Result<ProviderLaunchSpec, ProviderError> {
        Err(ProviderError::UnsupportedCapability(
            ProviderCapability::BuildLaunch,
        ))
    }

    fn parse_signal(&self, _signal: ProviderSignal) -> Vec<JournalEvent> {
        Vec::new()
    }

    fn cooperative_stop(&self, _session: &ProviderRuntime) -> StopStrategy {
        StopStrategy::Unsupported
    }

    async fn observe_quota(
        &self,
        _executable: &Path,
    ) -> Result<Option<QuotaObservation>, ProviderError> {
        Ok(None)
    }
}

struct ProbeOnlyAdapter {
    capabilities: ProviderCapabilities,
    probes: AtomicUsize,
}

#[async_trait]
impl ProviderAdapter for ProbeOnlyAdapter {
    fn kind(&self) -> ProviderKind {
        self.capabilities.kind
    }

    async fn probe(&self, _executable: &Path) -> Result<ProviderCapabilities, ProviderError> {
        self.probes.fetch_add(1, Ordering::Relaxed);
        Ok(self.capabilities.clone())
    }

    fn build_launch(
        &self,
        _request: LaunchProviderRequest,
    ) -> Result<ProviderLaunchSpec, ProviderError> {
        Err(ProviderError::UnsupportedCapability(
            ProviderCapability::BuildLaunch,
        ))
    }

    fn parse_signal(&self, _signal: ProviderSignal) -> Vec<JournalEvent> {
        Vec::new()
    }

    fn cooperative_stop(&self, _session: &ProviderRuntime) -> StopStrategy {
        StopStrategy::Unsupported
    }

    async fn observe_quota(
        &self,
        _executable: &Path,
    ) -> Result<Option<QuotaObservation>, ProviderError> {
        Ok(None)
    }
}

fn capabilities(
    kind: ProviderKind,
    version: &str,
    auth_state: ProviderAuthState,
    exact_resume: CapabilitySupport,
    semantic_events: CapabilitySupport,
    provider_session_id: CapabilitySupport,
) -> ProviderCapabilities {
    ProviderCapabilities {
        kind,
        version: ProviderVersion::new(version).unwrap(),
        auth_state,
        exact_resume,
        semantic_events,
        provider_session_id,
        build_launch: CapabilitySupport::Unknown,
        parse_signal: CapabilitySupport::Unknown,
        cooperative_stop: CapabilitySupport::Unknown,
        observe_quota: CapabilitySupport::Unknown,
        evidence: auth_evidence(auth_state, 1_700_000_000_001),
    }
}

fn auth_evidence(auth_state: ProviderAuthState, observed_at: u64) -> Vec<CapabilityEvidence> {
    let status = match auth_state {
        ProviderAuthState::AuthenticatedSubscription => EvidenceStatus::Authenticated,
        ProviderAuthState::AuthRequired => EvidenceStatus::AuthRequired,
        ProviderAuthState::Unknown => EvidenceStatus::Unknown,
    };
    vec![
        CapabilityEvidence::new(EvidenceSourceId::AuthStatusProbe, observed_at, status, None)
            .unwrap(),
    ]
}

fn executable_file(root: &Path, name: &str, bytes: &[u8]) -> PathBuf {
    let path = root.join(name);
    std::fs::copy(
        std::env::current_exe().expect("current test executable"),
        &path,
    )
    .expect("copy controlled native test executable");
    if !bytes.is_empty() {
        std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .expect("open controlled native test executable")
            .write_all(bytes)
            .expect("append controlled fixture marker");
    }
    path
}

fn discovery(override_path: Option<PathBuf>, path_root: Option<&Path>) -> ProviderDiscoveryConfig {
    ProviderDiscoveryConfig {
        executable_override: override_path,
        path: path_root.map(|root| OsString::from(root.as_os_str())),
    }
}

#[tokio::test]
async fn registry_rejects_duplicate_provider_kinds() {
    let mut registry = ProviderRegistry::new();
    registry
        .register(FakeAdapter::new(capabilities(
            ProviderKind::ClaudeCode,
            "fixture-1",
            ProviderAuthState::Unknown,
            CapabilitySupport::Unknown,
            CapabilitySupport::Unknown,
            CapabilitySupport::Unknown,
        )))
        .unwrap();

    let duplicate = registry.register(FakeAdapter::new(capabilities(
        ProviderKind::ClaudeCode,
        "fixture-2",
        ProviderAuthState::Unknown,
        CapabilitySupport::Unknown,
        CapabilitySupport::Unknown,
        CapabilitySupport::Unknown,
    )));

    assert!(matches!(
        duplicate,
        Err(ProviderError::DuplicateProviderKind(
            ProviderKind::ClaudeCode
        ))
    ));
}

#[tokio::test]
async fn configured_direct_executable_wins_over_path_discovery() {
    let temp = tempdir().unwrap();
    let path_root = temp.path().join("path");
    std::fs::create_dir_all(&path_root).unwrap();
    let path_executable = executable_file(&path_root, "claude", b"path-binary");
    let override_root = temp.path().join("override");
    std::fs::create_dir_all(&override_root).unwrap();
    let override_executable = executable_file(&override_root, "claude", b"override-binary");
    let adapter = FakeAdapter::new(capabilities(
        ProviderKind::ClaudeCode,
        "fixture-1",
        ProviderAuthState::Unknown,
        CapabilitySupport::Unknown,
        CapabilitySupport::Unknown,
        CapabilitySupport::Unknown,
    ));
    let mut registry = ProviderRegistry::new();
    registry.register(adapter).unwrap();

    let observation = registry
        .observe(
            ProviderKind::ClaudeCode,
            &discovery(Some(override_executable.clone()), Some(&path_root)),
        )
        .await
        .unwrap();

    assert_eq!(
        observation.executable.canonical_path(),
        std::fs::canonicalize(override_executable).unwrap()
    );
    assert_ne!(
        observation.executable.canonical_path(),
        std::fs::canonicalize(path_executable).unwrap()
    );
}

#[tokio::test]
async fn canonical_aliases_have_one_executable_identity() {
    let temp = tempdir().unwrap();
    let bin = temp.path().join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let executable = executable_file(&bin, "codex", b"same-binary");
    let alias = temp.path().join("bin").join("..").join("bin").join("codex");

    let inspector = FileSystemExecutableInspector;
    let canonical = inspector.inspect(&executable).await.unwrap();
    let aliased = inspector.inspect(&alias).await.unwrap();

    assert_eq!(canonical, aliased);
    assert_eq!(canonical.sha256(), aliased.sha256());
}

#[test]
fn capability_cache_key_contains_every_identity_dimension() {
    let temp = tempdir().unwrap();
    let executable = executable_file(temp.path(), "cursor-agent", b"cursor-a");
    let replacement = executable_file(temp.path(), "cursor-agent-replacement", b"cursor-b");
    let identity = ProviderExecutable::from_path(&executable).unwrap();
    let replacement_identity = ProviderExecutable::from_path(&replacement).unwrap();
    let version = ProviderVersion::new("fixture-1").unwrap();
    let base = CapabilityCacheKey::new(
        ProviderKind::ClaudeCode,
        identity.clone(),
        version.clone(),
        AdapterRevision::new(1),
        SemanticSchemaVersion::new(1),
    );

    assert_ne!(
        base,
        CapabilityCacheKey::new(
            ProviderKind::Codex,
            identity.clone(),
            version.clone(),
            AdapterRevision::new(1),
            SemanticSchemaVersion::new(1),
        )
    );
    assert_ne!(
        base,
        CapabilityCacheKey::new(
            ProviderKind::ClaudeCode,
            replacement_identity,
            version.clone(),
            AdapterRevision::new(1),
            SemanticSchemaVersion::new(1),
        )
    );
    assert_ne!(
        base,
        CapabilityCacheKey::new(
            ProviderKind::ClaudeCode,
            identity.clone(),
            ProviderVersion::new("fixture-2").unwrap(),
            AdapterRevision::new(1),
            SemanticSchemaVersion::new(1),
        )
    );
    assert_ne!(
        base,
        CapabilityCacheKey::new(
            ProviderKind::ClaudeCode,
            identity.clone(),
            version.clone(),
            AdapterRevision::new(2),
            SemanticSchemaVersion::new(1),
        )
    );
    assert_ne!(
        base,
        CapabilityCacheKey::new(
            ProviderKind::ClaudeCode,
            identity,
            version,
            AdapterRevision::new(1),
            SemanticSchemaVersion::new(2),
        )
    );
}

#[tokio::test]
async fn capability_cache_hits_while_refreshing_auth_probe() {
    let temp = tempdir().unwrap();
    let executable = executable_file(temp.path(), "claude", b"binary-a");
    let adapter = FakeAdapter::new(capabilities(
        ProviderKind::ClaudeCode,
        "fixture-1",
        ProviderAuthState::Unknown,
        CapabilitySupport::Supported,
        CapabilitySupport::Unknown,
        CapabilitySupport::Supported,
    ));
    let mut registry = ProviderRegistry::new();
    registry.register(adapter.clone()).unwrap();
    let config = discovery(Some(executable), None);

    let first = registry
        .observe(ProviderKind::ClaudeCode, &config)
        .await
        .unwrap();
    let second = registry
        .observe(ProviderKind::ClaudeCode, &config)
        .await
        .unwrap();

    assert_eq!(first.cache_status, CacheStatus::Miss);
    assert_eq!(second.cache_status, CacheStatus::Hit);
    assert_eq!(adapter.capability_probes.load(Ordering::Relaxed), 2);
}

#[tokio::test]
async fn cache_hit_refreshes_auth_from_matching_fresh_probe_evidence() {
    let temp = tempdir().unwrap();
    let executable = executable_file(temp.path(), "claude", b"binary-a");
    let adapter = FakeAdapter::new(capabilities(
        ProviderKind::ClaudeCode,
        "fixture-1",
        ProviderAuthState::AuthenticatedSubscription,
        CapabilitySupport::Supported,
        CapabilitySupport::Unknown,
        CapabilitySupport::Supported,
    ));
    let mut registry = ProviderRegistry::new();
    registry.register(adapter.clone()).unwrap();
    let config = discovery(Some(executable), None);

    let first = registry
        .observe(ProviderKind::ClaudeCode, &config)
        .await
        .unwrap();
    adapter.set_auth_state(ProviderAuthState::AuthRequired);
    let second = registry
        .observe(ProviderKind::ClaudeCode, &config)
        .await
        .unwrap();

    assert_eq!(first.cache_status, CacheStatus::Miss);
    assert_eq!(
        first.capabilities.auth_state,
        ProviderAuthState::AuthenticatedSubscription
    );
    assert_eq!(second.cache_status, CacheStatus::Hit);
    assert_eq!(
        second.capabilities.auth_state,
        ProviderAuthState::AuthRequired
    );
    assert_eq!(
        second.capabilities.evidence[0].source(),
        EvidenceSourceId::AuthStatusProbe
    );
    assert_eq!(adapter.capability_probes.load(Ordering::Relaxed), 2);
}

#[tokio::test]
async fn same_identity_concurrent_misses_share_one_expensive_probe() {
    let temp = tempdir().unwrap();
    let executable = executable_file(temp.path(), "claude", b"binary-a");
    let adapter = FakeAdapter::new(capabilities(
        ProviderKind::ClaudeCode,
        "fixture-1",
        ProviderAuthState::Unknown,
        CapabilitySupport::Unknown,
        CapabilitySupport::Unknown,
        CapabilitySupport::Unknown,
    ));
    adapter.set_probe_delay(Duration::from_millis(100));
    let mut registry = ProviderRegistry::new();
    registry.register(adapter.clone()).unwrap();
    let config = discovery(Some(executable), None);

    let (first, second) = tokio::join!(
        registry.observe(ProviderKind::ClaudeCode, &config),
        registry.observe(ProviderKind::ClaudeCode, &config),
    );

    assert!(first.is_ok());
    assert!(second.is_ok());
    assert_eq!(adapter.capability_probes.load(Ordering::Relaxed), 1);
}

#[tokio::test]
async fn slower_completion_cannot_evict_a_newer_executable_identity() {
    let temp = tempdir().unwrap();
    let slow_root = temp.path().join("slow");
    let fast_root = temp.path().join("fast");
    std::fs::create_dir_all(&slow_root).unwrap();
    std::fs::create_dir_all(&fast_root).unwrap();
    let slow = executable_file(&slow_root, "claude", b"slow-binary");
    let fast = executable_file(&fast_root, "claude", b"fast-binary");
    let adapter = FakeAdapter::new(capabilities(
        ProviderKind::ClaudeCode,
        "fixture-1",
        ProviderAuthState::Unknown,
        CapabilitySupport::Unknown,
        CapabilitySupport::Unknown,
        CapabilitySupport::Unknown,
    ));
    adapter.set_path_delay("slow", Duration::from_millis(100));
    let mut registry = ProviderRegistry::new();
    registry.register(adapter).unwrap();
    let registry = Arc::new(registry);

    let slow_registry = Arc::clone(&registry);
    let slow_task = tokio::spawn(async move {
        slow_registry
            .observe(ProviderKind::ClaudeCode, &discovery(Some(slow), None))
            .await
    });
    tokio::time::sleep(Duration::from_millis(10)).await;
    let fast_result = registry
        .observe(ProviderKind::ClaudeCode, &discovery(Some(fast), None))
        .await;
    let slow_result = slow_task.await.unwrap();

    assert!(fast_result.is_ok());
    assert!(slow_result.is_ok());
    assert_eq!(registry.cache_len(), 2);
}

#[tokio::test]
async fn adapter_without_lightweight_version_probe_still_gets_a_cache_hit() {
    let temp = tempdir().unwrap();
    let executable = executable_file(temp.path(), "claude", b"binary-a");
    let adapter = Arc::new(ProbeOnlyAdapter {
        capabilities: capabilities(
            ProviderKind::ClaudeCode,
            "fixture-1",
            ProviderAuthState::Unknown,
            CapabilitySupport::Unknown,
            CapabilitySupport::Unknown,
            CapabilitySupport::Unknown,
        ),
        probes: AtomicUsize::new(0),
    });
    let mut registry = ProviderRegistry::new();
    registry.register(adapter.clone()).unwrap();
    let config = discovery(Some(executable), None);

    registry
        .observe(ProviderKind::ClaudeCode, &config)
        .await
        .unwrap();
    let second = registry
        .observe(ProviderKind::ClaudeCode, &config)
        .await
        .unwrap();

    assert_eq!(second.cache_status, CacheStatus::Hit);
    assert_eq!(adapter.probes.load(Ordering::Relaxed), 2);
}

#[tokio::test]
async fn binary_replacement_at_same_path_invalidates_capability_cache() {
    let temp = tempdir().unwrap();
    let executable = executable_file(temp.path(), "codex", b"binary-a");
    let adapter = FakeAdapter::new(capabilities(
        ProviderKind::Codex,
        "fixture-1",
        ProviderAuthState::Unknown,
        CapabilitySupport::Unknown,
        CapabilitySupport::Unknown,
        CapabilitySupport::Unknown,
    ));
    let mut registry = ProviderRegistry::new();
    registry.register(adapter.clone()).unwrap();
    let config = discovery(Some(executable.clone()), None);

    registry
        .observe(ProviderKind::Codex, &config)
        .await
        .unwrap();
    std::fs::OpenOptions::new()
        .append(true)
        .open(&executable)
        .unwrap()
        .write_all(b"binary-replaced")
        .unwrap();
    let second = registry
        .observe(ProviderKind::Codex, &config)
        .await
        .unwrap();

    assert_eq!(second.cache_status, CacheStatus::Miss);
    assert_eq!(adapter.capability_probes.load(Ordering::Relaxed), 2);
}

#[tokio::test]
async fn provider_version_change_invalidates_capability_cache() {
    let temp = tempdir().unwrap();
    let executable = executable_file(temp.path(), "cursor-agent", b"binary-a");
    let adapter = FakeAdapter::new(capabilities(
        ProviderKind::Cursor,
        "fixture-1",
        ProviderAuthState::Unknown,
        CapabilitySupport::Unknown,
        CapabilitySupport::Unknown,
        CapabilitySupport::Unknown,
    ));
    let mut registry = ProviderRegistry::new();
    registry.register(adapter.clone()).unwrap();
    let config = discovery(Some(executable), None);

    registry
        .observe(ProviderKind::Cursor, &config)
        .await
        .unwrap();
    adapter.set_version(ProviderVersion::new("fixture-2").unwrap());
    let second = registry
        .observe(ProviderKind::Cursor, &config)
        .await
        .unwrap();

    assert_eq!(second.cache_status, CacheStatus::Miss);
    assert_eq!(adapter.capability_probes.load(Ordering::Relaxed), 2);
}

#[test]
fn authenticated_subscription_fixture_is_explicit_provider_observation() {
    let raw = include_str!("fixtures/providers/registry/authenticated_subscription.json");
    let capabilities: ProviderCapabilities = serde_json::from_str(raw).unwrap();

    assert_eq!(
        capabilities.auth_state,
        ProviderAuthState::AuthenticatedSubscription
    );
    assert_eq!(capabilities.kind, ProviderKind::ClaudeCode);
    assert!(capabilities
        .evidence
        .iter()
        .all(|evidence| evidence.source() == EvidenceSourceId::AuthStatusProbe));
}

#[test]
fn authenticated_states_require_matching_auth_status_evidence() {
    let mut capabilities = capabilities(
        ProviderKind::ClaudeCode,
        "fixture-1",
        ProviderAuthState::AuthRequired,
        CapabilitySupport::Unknown,
        CapabilitySupport::Unknown,
        CapabilitySupport::Unknown,
    );
    capabilities.evidence = vec![CapabilityEvidence::new(
        EvidenceSourceId::AuthStatusProbe,
        1_700_000_000_001,
        EvidenceStatus::Authenticated,
        None,
    )
    .unwrap()];

    assert!(matches!(
        capabilities.validate(),
        Err(devmanager::providers::ProviderCapabilitiesError::MismatchedAuthStatusEvidence { .. })
    ));
}

#[test]
fn auth_required_observation_contains_no_credentials_or_raw_output() {
    let raw = include_str!("fixtures/providers/registry/auth_required.json");
    let capabilities: ProviderCapabilities = serde_json::from_str(raw).unwrap();
    let encoded = serde_json::to_string(&capabilities).unwrap();

    assert_eq!(capabilities.auth_state, ProviderAuthState::AuthRequired);
    assert!(!encoded.contains("sk-") && !encoded.contains("OPENAI_API_KEY"));
    assert!(!encoded.contains("raw provider output"));
}

#[test]
fn unknown_capability_is_distinct_from_unsupported() {
    assert_ne!(CapabilitySupport::Unknown, CapabilitySupport::Unsupported);
    assert_ne!(CapabilitySupport::Unknown, CapabilitySupport::Supported);
}

#[test]
fn unsupported_exact_resume_remains_an_explicit_capability_state() {
    let capabilities = capabilities(
        ProviderKind::Cursor,
        "fixture-1",
        ProviderAuthState::AuthenticatedSubscription,
        CapabilitySupport::Unsupported,
        CapabilitySupport::Supported,
        CapabilitySupport::Unknown,
    );

    assert_eq!(capabilities.exact_resume, CapabilitySupport::Unsupported);
    assert!(!capabilities.exact_resume.is_supported());
}

#[tokio::test]
async fn missing_cli_is_typed_and_does_not_fall_back_from_a_missing_override() {
    let temp = tempdir().unwrap();
    let path_root = temp.path().join("path");
    std::fs::create_dir_all(&path_root).unwrap();
    executable_file(&path_root, "claude", b"path-binary");
    let missing_override = temp.path().join("missing-claude");
    let adapter = FakeAdapter::new(capabilities(
        ProviderKind::ClaudeCode,
        "fixture-1",
        ProviderAuthState::Unknown,
        CapabilitySupport::Unknown,
        CapabilitySupport::Unknown,
        CapabilitySupport::Unknown,
    ));
    let mut registry = ProviderRegistry::new();
    registry.register(adapter).unwrap();

    let result = registry
        .resolve_executable(
            ProviderKind::ClaudeCode,
            &discovery(Some(missing_override), Some(&path_root)),
        )
        .await;

    assert!(matches!(result, Err(ProviderError::MissingCli { .. })));
}

#[tokio::test]
async fn wrapper_override_is_rejected_without_wrapper_or_path_fallback() {
    let temp = tempdir().unwrap();
    let path_root = temp.path().join("path");
    std::fs::create_dir_all(&path_root).unwrap();
    executable_file(&path_root, "claude", b"path-binary");
    let npx = executable_file(temp.path(), "npx", b"wrapper");
    let adapter = FakeAdapter::new(capabilities(
        ProviderKind::ClaudeCode,
        "fixture-1",
        ProviderAuthState::Unknown,
        CapabilitySupport::Unknown,
        CapabilitySupport::Unknown,
        CapabilitySupport::Unknown,
    ));
    let mut registry = ProviderRegistry::new();
    registry.register(adapter).unwrap();

    let result = registry
        .resolve_executable(
            ProviderKind::ClaudeCode,
            &discovery(Some(npx), Some(&path_root)),
        )
        .await;

    assert!(matches!(
        result,
        Err(ProviderError::WrapperCommandNotAllowed { .. })
    ));
}

#[test]
fn malformed_version_output_is_typed() {
    assert!(matches!(
        ProviderVersion::from_probe_output(b"\xff\xfe"),
        Err(ProviderVersionError::InvalidUtf8)
    ));
    assert!(matches!(
        ProviderVersion::from_probe_output(b"\n\n"),
        Err(ProviderVersionError::Empty)
    ));
    assert!(matches!(
        ProviderVersion::new("   "),
        Err(ProviderVersionError::Empty)
    ));
}

#[test]
fn capability_metadata_rejects_an_oversized_evidence_collection() {
    let evidence = (1..=MAX_CAPABILITY_EVIDENCE_ITEMS as u64 + 1)
        .map(|index| {
            serde_json::json!({
                "source": "registry",
                "observed_at": index,
                "status": "unknown",
                "diagnostic": null
            })
        })
        .collect::<Vec<_>>();
    let value = serde_json::json!({
        "kind": "codex",
        "version": "fixture-1",
        "auth_state": "unknown",
        "exact_resume": "unknown",
        "semantic_events": "unknown",
        "provider_session_id": "unknown",
        "build_launch": "unknown",
        "parse_signal": "unknown",
        "cooperative_stop": "unknown",
        "observe_quota": "unknown",
        "evidence": evidence
    });

    assert!(serde_json::from_value::<ProviderCapabilities>(value).is_err());
}

#[test]
fn capability_evidence_has_only_bounded_sanitized_metadata() {
    let evidence = CapabilityEvidence::new(
        EvidenceSourceId::AuthStatusProbe,
        1_700_000_000_000,
        EvidenceStatus::AuthRequired,
        Some(EvidenceDiagnostic::new(
            EvidenceDiagnosticCode::AuthenticationRequired,
            Some([0xabu8; 32]),
        )),
    )
    .unwrap();
    let encoded: Value = serde_json::to_value(&evidence).unwrap();
    let object = encoded.as_object().unwrap();

    assert_eq!(object.len(), 8);
    assert_eq!(object.get("schema_version"), Some(&Value::from(1)));
    assert!(object.contains_key("source"));
    assert!(object.contains_key("observed_at"));
    assert!(object.contains_key("expires_at"));
    assert!(object.contains_key("confidence"));
    assert!(object.contains_key("auth_source"));
    assert!(object.contains_key("status"));
    assert!(object.contains_key("diagnostic"));
    assert!(!object.contains_key("detail"));
    assert_eq!(evidence.source(), EvidenceSourceId::AuthStatusProbe);
    assert_eq!(evidence.status(), EvidenceStatus::AuthRequired);
}

#[test]
fn evidence_rejects_empty_or_oversized_sources() {
    assert!(matches!(
        CapabilityEvidence::new(EvidenceSourceId::Registry, 0, EvidenceStatus::Unknown, None,),
        Err(CapabilityEvidenceError::ObservedAtZero)
    ));
    assert!(
        serde_json::from_value::<CapabilityEvidence>(serde_json::json!({
            "source": "arbitrary-source",
            "observed_at": 1,
            "status": "unknown",
            "diagnostic": null
        }))
        .is_err()
    );
}

#[test]
fn opaque_provider_session_id_preserves_bytes_and_rejects_invalid_values() {
    let original = " opaque-\u{00e9}-bytes ".to_string();
    let session = ProviderSessionId::new(original.clone()).unwrap();
    assert_eq!(session.as_str(), original);
    assert_eq!(session.as_bytes(), original.as_bytes());
    assert_eq!(
        ProviderSessionId::try_from(original.as_str()).unwrap(),
        session
    );

    assert!(ProviderSessionId::new("").is_err());
    assert!(ProviderSessionId::new("has\ncontrol").is_err());
    assert!(ProviderSessionId::new("x".repeat(MAX_PROVIDER_SESSION_ID_BYTES + 1)).is_err());
}

struct RecordingProbeRunner {
    requests: Mutex<Vec<ProviderProbeRequest>>,
}

#[async_trait]
impl ProviderProbeRunner for RecordingProbeRunner {
    async fn run(
        &self,
        request: ProviderProbeRequest,
    ) -> Result<ProviderProbeResult, ProviderProbeError> {
        let result = ProviderProbeResult::completed(&request, 0, 10, 0)?;
        self.requests.lock().unwrap().push(request);
        Ok(result)
    }
}

#[tokio::test]
async fn probe_seam_uses_only_fixed_version_help_arguments_and_null_stdin() {
    let runner = RecordingProbeRunner {
        requests: Mutex::new(Vec::new()),
    };
    let executable = PathBuf::from("C:/fixtures/claude");
    runner
        .run(ProviderProbeRequest::version(executable.clone()).unwrap())
        .await
        .unwrap();
    runner
        .run(ProviderProbeRequest::help(executable).unwrap())
        .await
        .unwrap();

    let requests = runner.requests.lock().unwrap();
    assert_eq!(requests[0].arguments(), &["--version"]);
    assert_eq!(requests[1].arguments(), &["--help"]);
    assert!(requests.iter().all(ProviderProbeRequest::uses_null_stdin));
}

#[tokio::test]
async fn executable_presence_and_version_do_not_infer_subscription_auth() {
    let temp = tempdir().unwrap();
    let executable = executable_file(temp.path(), "codex", b"stock-looking-binary");
    let adapter = FakeAdapter::new(capabilities(
        ProviderKind::Codex,
        "fixture-1",
        ProviderAuthState::Unknown,
        CapabilitySupport::Unknown,
        CapabilitySupport::Unknown,
        CapabilitySupport::Unknown,
    ));
    let mut registry = ProviderRegistry::new();
    registry.register(adapter).unwrap();

    let observation = registry
        .observe(ProviderKind::Codex, &discovery(Some(executable), None))
        .await
        .unwrap();

    assert_eq!(
        observation.capabilities.auth_state,
        ProviderAuthState::Unknown
    );
}

#[tokio::test]
async fn observation_metadata_is_serializable_without_probe_payloads() {
    let temp = tempdir().unwrap();
    let executable = executable_file(temp.path(), "claude", b"binary-a");
    let adapter = FakeAdapter::new(capabilities(
        ProviderKind::ClaudeCode,
        "fixture-1",
        ProviderAuthState::AuthRequired,
        CapabilitySupport::Unsupported,
        CapabilitySupport::Unknown,
        CapabilitySupport::Unknown,
    ));
    let mut registry = ProviderRegistry::new();
    registry.register(adapter).unwrap();

    let observation = registry
        .observe(ProviderKind::ClaudeCode, &discovery(Some(executable), None))
        .await
        .unwrap();
    let encoded = serde_json::to_string(&observation).unwrap();

    assert!(encoded.contains("canonical_path"));
    assert!(encoded.contains("sha256"));
    assert!(!encoded.contains("stdout"));
    assert!(!encoded.contains("stderr"));
}

#[test]
fn agent_session_facts_deserialization_preserves_exact_opaque_provider_id() {
    let exact = "  opaque\u{00e9}-provider-id  ";
    let value = serde_json::json!({
        "id": AgentSessionId::new(),
        "task_id": TaskId::new(),
        "role": "primary",
        "provider_kind": "claude",
        "provider_session_id": exact,
        "lifecycle": "open",
        "runtime_generation": 0,
        "revision": 0
    });

    let facts: AgentSessionFacts = serde_json::from_value(value).unwrap();
    assert_eq!(
        facts
            .provider_session_id
            .as_ref()
            .map(ProviderSessionId::as_str),
        Some(exact)
    );
    let encoded = serde_json::to_value(&facts).unwrap();
    assert_eq!(encoded["provider_session_id"], exact);
}

#[test]
fn agent_session_facts_constructor_keeps_typed_provider_id_unchanged() {
    let exact = "  opaque\u{00e9}-constructor-id  ";
    let session = ProviderSessionId::new(exact).unwrap();
    let facts = AgentSessionFacts::new(
        TaskId::new(),
        AgentRole::Primary,
        "claude",
        Some(session.clone()),
    );

    let facts = facts.unwrap();
    assert_eq!(facts.provider_session_id, Some(session));
}

#[test]
fn capability_evidence_persists_only_typed_non_sensitive_metadata() {
    let evidence = CapabilityEvidence::new(
        EvidenceSourceId::AuthStatusProbe,
        1_700_000_000_000,
        EvidenceStatus::AuthRequired,
        Some(EvidenceDiagnostic::new(
            EvidenceDiagnosticCode::AuthenticationRequired,
            Some([0xabu8; 32]),
        )),
    )
    .unwrap();
    let encoded: Value = serde_json::to_value(&evidence).unwrap();
    let object = encoded.as_object().unwrap();

    assert_eq!(object["source"], "auth_status_probe");
    assert_eq!(object["status"], "auth_required");
    assert!(object.contains_key("observed_at"));
    assert!(object.contains_key("diagnostic"));
    assert!(!object.contains_key("detail"));
    assert!(!object.contains_key("command"));
    assert!(!object.contains_key("path"));
    assert!(
        serde_json::from_value::<CapabilityEvidence>(serde_json::json!({
            "source": "auth_status_probe",
            "observed_at": 1_700_000_000_000u64,
            "status": "auth_required",
            "detail": "raw stdout OPENAI_API_KEY=secret"
        }))
        .is_err()
    );
}

#[test]
fn probe_requests_include_typed_noninteractive_auth_status_contract() {
    let request = ProviderProbeRequest::auth_status(PathBuf::from("C:/bin/claude")).unwrap();

    assert_eq!(
        request.kind(),
        devmanager::providers::ProviderProbeKind::AuthStatus
    );
    assert_eq!(request.arguments(), &["auth", "status"]);
    assert!(request.uses_null_stdin());
    assert!(!request.uses_shell());
    assert!(request.strips_api_key_environment());
    assert!(request.kills_descendants_on_timeout());
    assert!(request.max_output_bytes() <= 64 * 1024);
}

struct BoundaryAdapter;

#[async_trait]
impl ProviderAdapter for BoundaryAdapter {
    fn kind(&self) -> ProviderKind {
        ProviderKind::ClaudeCode
    }

    async fn probe(&self, _executable: &Path) -> Result<ProviderCapabilities, ProviderError> {
        Ok(capabilities(
            ProviderKind::ClaudeCode,
            "fixture-1",
            ProviderAuthState::Unknown,
            CapabilitySupport::Unknown,
            CapabilitySupport::Unknown,
            CapabilitySupport::Unknown,
        ))
    }

    fn build_launch(
        &self,
        _request: LaunchProviderRequest,
    ) -> Result<ProviderLaunchSpec, ProviderError> {
        Err(ProviderError::UnsupportedCapability(
            ProviderCapability::BuildLaunch,
        ))
    }

    fn parse_signal(&self, _signal: ProviderSignal) -> Vec<JournalEvent> {
        Vec::new()
    }

    fn cooperative_stop(&self, _session: &ProviderRuntime) -> StopStrategy {
        StopStrategy::Unsupported
    }

    async fn observe_quota(
        &self,
        _executable: &Path,
    ) -> Result<Option<QuotaObservation>, ProviderError> {
        Ok(None)
    }
}

#[tokio::test]
async fn provider_adapter_boundary_is_object_safe_and_capability_gated() {
    let adapter: Arc<dyn ProviderAdapter> = Arc::new(BoundaryAdapter);
    let temp = tempdir().unwrap();
    let executable_path = executable_file(temp.path(), "claude", b"adapter");
    let executable = ProviderExecutable::from_path(executable_path).unwrap();
    let launch = adapter.build_launch(LaunchProviderRequest::new(executable, None, None));
    assert!(matches!(
        launch,
        Err(ProviderError::UnsupportedCapability(
            ProviderCapability::BuildLaunch
        ))
    ));

    let signal = adapter.parse_signal(ProviderSignal::SessionEnded);
    assert!(signal.is_empty());

    let stop = adapter.cooperative_stop(&ProviderRuntime);
    assert_eq!(stop, StopStrategy::Unsupported);

    let quota = adapter
        .observe_quota(Path::new("C:/bin/claude"))
        .await
        .unwrap();
    assert!(quota.is_none());
}

#[test]
fn provider_probe_result_has_bounded_structured_status() {
    let request = ProviderProbeRequest::with_limits(
        PathBuf::from("C:/bin/claude"),
        devmanager::providers::ProviderProbeKind::Help,
        Duration::from_secs(1),
        64,
    )
    .unwrap();
    let result = ProviderProbeResult::completed(&request, 0, 12, 4).unwrap();
    assert_eq!(
        result.status(),
        devmanager::providers::ProviderProbeStatus::Completed
    );
    assert_eq!(result.stdout_bytes(), 12);
    assert_eq!(result.stderr_bytes(), 4);
}

#[test]
fn executable_and_probe_result_constructors_reject_unbounded_inputs() {
    assert!(ProviderExecutable::new(PathBuf::new(), [0; 32]).is_err());
    assert!(ProviderProbeRequest::new(
        PathBuf::new(),
        devmanager::providers::ProviderProbeKind::Help,
    )
    .is_err());
    let request = ProviderProbeRequest::with_limits(
        PathBuf::from("C:/bin/claude"),
        devmanager::providers::ProviderProbeKind::Help,
        Duration::from_secs(1),
        10,
    )
    .unwrap();
    assert!(ProviderProbeResult::completed(&request, 0, 10, 1).is_err());
}

#[test]
fn provider_executable_policy_rejects_undeclared_runners_and_desktop_cursor() {
    let policy = ProviderExecutablePolicy::new(["cursor-agent", "cursor-agent.cmd"]).unwrap();
    assert!(policy
        .validate_canonical_path(Path::new("C:/bin/cursor-agent"))
        .is_ok());
    assert!(policy
        .validate_canonical_path(Path::new("C:/bin/cursor.exe"))
        .is_err());
    assert!(policy
        .validate_canonical_path(Path::new("C:/bin/npx.exe"))
        .is_err());
    assert!(policy
        .validate_canonical_path(Path::new("C:/bin/node.exe"))
        .is_err());
    let cmd_policy = ProviderExecutablePolicy::new(["claude.cmd"]).unwrap();
    assert!(cmd_policy
        .validate_canonical_path(Path::new("C:/bin/claude.cmd"))
        .is_ok());
    assert!(cmd_policy
        .validate_canonical_path(Path::new("C:/bin/claude.exe"))
        .is_err());
}

struct SequenceInspector {
    identities: Mutex<Vec<ProviderExecutable>>,
    events: Arc<Mutex<Vec<&'static str>>>,
}

#[async_trait]
impl ExecutableInspector for SequenceInspector {
    async fn inspect(
        &self,
        _path: &Path,
    ) -> Result<ProviderExecutable, devmanager::providers::ProviderExecutableError> {
        let mut events = self.events.lock().unwrap();
        let inspected = events
            .iter()
            .filter(|event| event.starts_with("inspect"))
            .count();
        events.push(if inspected == 0 {
            "inspect-before"
        } else {
            "inspect-after"
        });
        self.identities.lock().unwrap().pop().ok_or(
            devmanager::providers::ProviderExecutableError::Missing(PathBuf::from("C:/missing")),
        )
    }
}

struct OrderedAdapter {
    events: Arc<Mutex<Vec<&'static str>>>,
}

#[async_trait]
impl ProviderAdapter for OrderedAdapter {
    fn kind(&self) -> ProviderKind {
        ProviderKind::ClaudeCode
    }

    async fn probe(&self, _executable: &Path) -> Result<ProviderCapabilities, ProviderError> {
        self.events.lock().unwrap().push("probe-capabilities");
        Ok(capabilities(
            ProviderKind::ClaudeCode,
            "fixture-1",
            ProviderAuthState::Unknown,
            CapabilitySupport::Unknown,
            CapabilitySupport::Unknown,
            CapabilitySupport::Unknown,
        ))
    }

    fn build_launch(
        &self,
        _request: LaunchProviderRequest,
    ) -> Result<ProviderLaunchSpec, ProviderError> {
        Err(ProviderError::UnsupportedCapability(
            ProviderCapability::BuildLaunch,
        ))
    }

    fn parse_signal(&self, _signal: ProviderSignal) -> Vec<JournalEvent> {
        Vec::new()
    }

    fn cooperative_stop(&self, _session: &ProviderRuntime) -> StopStrategy {
        StopStrategy::Unsupported
    }

    async fn observe_quota(
        &self,
        _executable: &Path,
    ) -> Result<Option<QuotaObservation>, ProviderError> {
        Ok(None)
    }
}

#[tokio::test]
async fn observe_rejects_identity_replacement_between_capability_probe_and_after_inspection() {
    let events = Arc::new(Mutex::new(Vec::new()));
    let temp = tempdir().unwrap();
    let identity_path = executable_file(temp.path(), "claude", b"before");
    let before = ProviderExecutable::from_path(&identity_path).unwrap();
    std::fs::OpenOptions::new()
        .append(true)
        .open(&identity_path)
        .unwrap()
        .write_all(b"after")
        .unwrap();
    let after = ProviderExecutable::from_path(&identity_path).unwrap();
    let inspector = Arc::new(SequenceInspector {
        identities: Mutex::new(vec![before, after]),
        events: events.clone(),
    });
    let adapter = Arc::new(OrderedAdapter {
        events: events.clone(),
    });
    let mut registry = ProviderRegistry::with_executable_inspector(inspector);
    registry.register(adapter).unwrap();

    let result = registry
        .observe(
            ProviderKind::ClaudeCode,
            &discovery(Some(PathBuf::from("C:/bin/claude")), None),
        )
        .await;

    assert!(matches!(
        result,
        Err(ProviderError::ExecutableChanged { .. })
    ));
    assert_eq!(
        events.lock().unwrap().as_slice(),
        &["inspect-before", "probe-capabilities", "inspect-after"]
    );
}

#[cfg(windows)]
#[tokio::test]
async fn windows_probe_runner_bounds_both_output_streams_exactly() {
    let temp = tempdir().unwrap();
    let executable = copied_probe_fixture(&temp, "probe-flood");
    let runner = probe_runner(&executable);
    let request = ProviderProbeRequest::with_limits(
        executable,
        devmanager::providers::ProviderProbeKind::Help,
        std::time::Duration::from_secs(2),
        257,
    )
    .unwrap();

    let result = runner.run(request).await.unwrap();

    assert_eq!(
        result.status(),
        devmanager::providers::ProviderProbeStatus::OutputTooLarge
    );
    assert!(result.stdout_bytes() + result.stderr_bytes() <= 257);
    assert!(result.stdout().len() + result.stderr().len() <= 257);
}

#[cfg(windows)]
#[tokio::test]
async fn windows_probe_runner_scrubs_inherited_provider_secrets() {
    let temp = tempdir().unwrap();
    let executable = copied_probe_fixture(&temp, "probe-env");
    let previous = std::env::var_os("ANTHROPIC_API_KEY");
    std::env::set_var("ANTHROPIC_API_KEY", "fixture-secret-must-not-cross");

    let runner = probe_runner(&executable);
    let request = ProviderProbeRequest::with_limits(
        executable,
        devmanager::providers::ProviderProbeKind::Help,
        std::time::Duration::from_secs(2),
        4096,
    )
    .unwrap();
    let result = runner.run(request).await.unwrap();

    match previous {
        Some(value) => std::env::set_var("ANTHROPIC_API_KEY", value),
        None => std::env::remove_var("ANTHROPIC_API_KEY"),
    }

    let output = String::from_utf8_lossy(result.stdout());
    assert!(output.contains("ANTHROPIC_API_KEY=<unset>"));
    assert!(!output.contains("fixture-secret-must-not-cross"));
}

#[cfg(windows)]
#[tokio::test]
async fn windows_probe_runner_timeout_kills_the_entire_probe_tree() {
    let temp = tempdir().unwrap();
    let executable = copied_probe_fixture(&temp, "probe-tree");
    let child_pid_path = executable.with_extension("child.pid");
    let runner = probe_runner(&executable);
    let request = ProviderProbeRequest::with_limits(
        executable,
        devmanager::providers::ProviderProbeKind::Help,
        std::time::Duration::from_millis(100),
        4096,
    )
    .unwrap();

    let result = runner.run(request).await;

    assert!(matches!(result, Err(ProviderProbeError::TimedOut)));
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    let child_pid = loop {
        if let Ok(value) = std::fs::read_to_string(&child_pid_path) {
            break value.parse::<u32>().unwrap();
        }
        assert!(
            std::time::Instant::now() < deadline,
            "fixture child pid was not published"
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    };
    while std::time::Instant::now() < deadline
        && devmanager::services::platform_service::is_pid_running(child_pid)
    {
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(!devmanager::services::platform_service::is_pid_running(
        child_pid
    ));
}
