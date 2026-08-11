use async_trait::async_trait;
use devmanager::domain::{
    AgentRole, AgentSessionFacts, AgentSessionId, ProviderSessionId, TaskId,
    MAX_PROVIDER_SESSION_ID_BYTES,
};
use devmanager::providers::adapter::{
    JournalEvent, LaunchProviderRequest, ProviderAdapter, ProviderArgument, ProviderError,
    ProviderInput, ProviderLaunchSpec, ProviderProbeError, ProviderProbeRequest,
    ProviderProbeResult, ProviderProbeRunner, ProviderRuntime, ProviderSignal, QuotaObservation,
    StopStrategy,
};
use devmanager::providers::capabilities::{
    AdapterRevision, CapabilityEvidence, CapabilityEvidenceError, CapabilitySupport,
    EvidenceDiagnostic, EvidenceDiagnosticCode, EvidenceSourceId, EvidenceStatus,
    ProviderAuthState, ProviderCapabilities, ProviderCapability, ProviderExecutable,
    ProviderExecutableError, ProviderExecutableHandle, ProviderExecutablePolicy, ProviderKind,
    ProviderVersion, ProviderVersionError, SemanticSchemaVersion, MAX_CAPABILITY_EVIDENCE_ITEMS,
};
use devmanager::providers::registry::{
    CacheStatus, CapabilityCacheKey, ExecutableInspector, FileSystemExecutableInspector,
    ProviderDiscoveryConfig, ProviderObservation, ProviderRegistry,
};
use serde_json::Value;
use std::ffi::OsString;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tempfile::tempdir;

#[cfg(any(windows, unix))]
fn copied_probe_fixture(temp: &tempfile::TempDir, stem: &str) -> PathBuf {
    let path = if cfg!(windows) {
        temp.path().join(format!("{stem}.exe"))
    } else {
        temp.path().join(stem)
    };
    std::fs::copy(
        env!("CARGO_BIN_EXE_devmanager-provider-probe-fixture"),
        &path,
    )
    .expect("copy harmless provider probe fixture");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = std::fs::metadata(&path)
            .expect("fixture metadata")
            .permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&path, permissions).expect("fixture is executable");
    }
    path
}

#[cfg(any(windows, unix))]
fn probe_runner(path: &Path) -> devmanager::providers::WindowsProviderProbeRunner {
    let file_name = path.file_name().unwrap().to_string_lossy().into_owned();
    devmanager::providers::WindowsProviderProbeRunner::new(
        ProviderExecutablePolicy::new([file_name]).expect("fixture allowlist"),
    )
}

async fn accept_trusted_auth_probe(
    registry: &ProviderRegistry,
    invocation: devmanager::providers::ProviderAuthProbeInvocation,
) -> Result<devmanager::providers::ProviderAuthEvidenceReceipt, ProviderError> {
    let handle = invocation.executable_handle().clone();
    let file_name = handle
        .canonical_path()
        .file_name()
        .expect("fixture file name")
        .to_string_lossy()
        .into_owned();
    let request = invocation
        .bind_request(ProviderProbeRequest::auth_status(handle).unwrap())
        .map_err(ProviderError::AuthEvidence)?;
    let runner = devmanager::providers::WindowsProviderProbeRunner::new(
        ProviderExecutablePolicy::new([file_name]).unwrap(),
    );
    let result = runner
        .run(request.clone())
        .await
        .map_err(ProviderError::Probe)?;
    registry.accept_auth_probe_result(invocation, request, result)
}

fn test_executable_handle() -> ProviderExecutableHandle {
    ProviderExecutable::from_path(std::env::current_exe().expect("test executable path"))
        .expect("test executable is inspectable")
        .open_for_launch()
        .expect("test executable handle")
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

    async fn probe(
        &self,
        executable: &ProviderExecutableHandle,
    ) -> Result<ProviderCapabilities, ProviderError> {
        self.capability_probes.fetch_add(1, Ordering::Relaxed);
        let delay = *self.probe_delay.lock().unwrap();
        if let Some(delay) = delay {
            tokio::time::sleep(delay).await;
        }
        let path_delay = self.path_delay.lock().unwrap().clone();
        if let Some((marker, delay)) = path_delay {
            if executable
                .canonical_path()
                .to_string_lossy()
                .contains(&marker)
            {
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
        _executable: &ProviderExecutableHandle,
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

    async fn probe(
        &self,
        _executable: &ProviderExecutableHandle,
    ) -> Result<ProviderCapabilities, ProviderError> {
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
        _executable: &ProviderExecutableHandle,
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
    let _ = auth_state;
    ProviderCapabilities {
        kind,
        version: ProviderVersion::new(version).unwrap(),
        auth_state: ProviderAuthState::Unknown,
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
    let _ = auth_state;
    vec![CapabilityEvidence::new(
        EvidenceSourceId::Registry,
        observed_at,
        EvidenceStatus::Unknown,
        None,
    )
    .unwrap()]
}

fn executable_file(root: &Path, name: &str, bytes: &[u8]) -> PathBuf {
    // Test-only candidate material; the copied harness is metadata input for
    // registry contract tests, not a real stock provider executable.
    let file_name = if cfg!(windows) && Path::new(name).extension().is_none() {
        format!("{name}.exe")
    } else {
        name.to_owned()
    };
    let path = root.join(file_name);
    std::fs::copy(
        env!("CARGO_BIN_EXE_devmanager-provider-probe-fixture"),
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
    let alias = temp
        .path()
        .join("bin")
        .join("..")
        .join("bin")
        .join(if cfg!(windows) { "codex.exe" } else { "codex" });

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
    let first_invocation = registry
        .begin_auth_probe(ProviderKind::ClaudeCode, &config, Duration::from_secs(30))
        .await
        .unwrap();
    let first_receipt = accept_trusted_auth_probe(&registry, first_invocation)
        .await
        .unwrap();
    let authenticated = registry
        .observe_with_auth_receipt(ProviderKind::ClaudeCode, &config, first_receipt)
        .await
        .unwrap();

    let second_invocation = registry
        .begin_auth_probe(ProviderKind::ClaudeCode, &config, Duration::from_secs(30))
        .await
        .unwrap();
    let second_receipt = accept_trusted_auth_probe(&registry, second_invocation)
        .await
        .unwrap();
    let second = registry
        .observe_with_auth_receipt(ProviderKind::ClaudeCode, &config, second_receipt)
        .await
        .unwrap();

    assert_eq!(first.cache_status, CacheStatus::Miss);
    assert_eq!(first.capabilities.auth_state, ProviderAuthState::Unknown);
    assert_eq!(authenticated.cache_status, CacheStatus::Hit);
    assert_eq!(
        authenticated.capabilities.auth_state,
        ProviderAuthState::AuthRequired
    );
    assert!(authenticated
        .capabilities
        .evidence
        .iter()
        .any(|evidence| evidence.source() == EvidenceSourceId::AuthStatusProbe));
    assert_eq!(second.cache_status, CacheStatus::Hit);
    assert_eq!(
        second.capabilities.auth_state,
        ProviderAuthState::AuthRequired
    );
    assert_eq!(adapter.capability_probes.load(Ordering::Relaxed), 5);
}

#[tokio::test]
async fn registry_consumes_only_current_receipts_and_does_not_promote_api_keys() {
    let temp = tempdir().unwrap();
    let executable = executable_file(temp.path(), "claude", b"receipt-boundary");
    let adapter = FakeAdapter::new(capabilities(
        ProviderKind::ClaudeCode,
        "fixture-1",
        ProviderAuthState::Unknown,
        CapabilitySupport::Supported,
        CapabilitySupport::Unknown,
        CapabilitySupport::Unknown,
    ));
    let mut registry = ProviderRegistry::new();
    registry.register(adapter).unwrap();
    let config = discovery(Some(executable), None);
    let identity = registry
        .resolve_executable(ProviderKind::ClaudeCode, &config)
        .await
        .unwrap();

    let invocation = registry
        .begin_auth_probe(ProviderKind::ClaudeCode, &config, Duration::from_secs(30))
        .await
        .unwrap();
    let receipt = accept_trusted_auth_probe(&registry, invocation)
        .await
        .unwrap();
    let observation = registry
        .observe_with_auth_receipt(ProviderKind::ClaudeCode, &config, receipt.clone())
        .await
        .unwrap();
    assert_eq!(
        observation.capabilities.auth_state,
        ProviderAuthState::AuthRequired
    );

    let replay = registry
        .observe_with_auth_receipt(ProviderKind::ClaudeCode, &config, receipt)
        .await;
    assert!(matches!(
        replay,
        Err(ProviderError::AuthEvidence(
            devmanager::providers::ProviderAuthEvidenceError::AlreadyConsumed
        ))
    ));

    let api_key_invocation = registry
        .begin_auth_probe(ProviderKind::ClaudeCode, &config, Duration::from_secs(30))
        .await
        .unwrap();
    let api_key_result = registry
        .accept_auth_probe(
            ProviderKind::ClaudeCode,
            &identity,
            api_key_invocation,
            devmanager::providers::ProviderAuthProbeResult::ApiKeyDetected,
            Instant::now(),
        )
        .unwrap_err();
    assert!(matches!(
        api_key_result,
        ProviderError::AuthEvidence(
            devmanager::providers::ProviderAuthEvidenceError::UntrustedAuthenticationEvidence
        )
    ));
}

#[tokio::test]
async fn registry_rejects_auth_receipt_after_provider_version_changes() {
    let temp = tempdir().unwrap();
    let executable = executable_file(temp.path(), "claude", b"version-bound-receipt");
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
    let invocation = registry
        .begin_auth_probe(ProviderKind::ClaudeCode, &config, Duration::from_secs(30))
        .await
        .unwrap();
    let receipt = accept_trusted_auth_probe(&registry, invocation)
        .await
        .unwrap();

    adapter.set_version(ProviderVersion::new("fixture-2").unwrap());
    let result = registry
        .observe_with_auth_receipt(ProviderKind::ClaudeCode, &config, receipt)
        .await;
    assert!(matches!(
        result,
        Err(ProviderError::AuthEvidence(
            devmanager::providers::ProviderAuthEvidenceError::WrongVersion
        ))
    ));
}

#[tokio::test]
async fn registry_does_not_accept_adapter_auth_without_a_registry_receipt() {
    let temp = tempdir().unwrap();
    let executable = executable_file(temp.path(), "claude", b"forged-auth");
    let mut forged = capabilities(
        ProviderKind::ClaudeCode,
        "fixture-1",
        ProviderAuthState::Unknown,
        CapabilitySupport::Supported,
        CapabilitySupport::Unknown,
        CapabilitySupport::Unknown,
    );
    forged.auth_state = ProviderAuthState::AuthenticatedSubscription;
    let adapter = FakeAdapter::new(forged);
    let mut registry = ProviderRegistry::new();
    registry.register(adapter).unwrap();

    let result = registry
        .observe(ProviderKind::ClaudeCode, &discovery(Some(executable), None))
        .await;

    assert!(result.is_err(), "adapter-returned auth must fail closed");
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
async fn cancelled_probe_leader_stays_charged_until_worker_completion() {
    let temp = tempdir().unwrap();
    let executable = executable_file(temp.path(), "claude", b"cancelled-leader");
    let adapter = FakeAdapter::new(capabilities(
        ProviderKind::ClaudeCode,
        "fixture-1",
        ProviderAuthState::Unknown,
        CapabilitySupport::Unknown,
        CapabilitySupport::Unknown,
        CapabilitySupport::Unknown,
    ));
    adapter.set_probe_delay(Duration::from_millis(500));
    let mut registry = ProviderRegistry::new();
    registry.register(adapter).unwrap();

    let observed = tokio::time::timeout(
        Duration::from_millis(100),
        registry.observe(ProviderKind::ClaudeCode, &discovery(Some(executable), None)),
    )
    .await;
    assert!(observed.is_err(), "the delayed leader should be cancelled");
    assert_eq!(registry.in_flight_len(), 1);
    let deadline = Instant::now() + Duration::from_secs(2);
    while registry.in_flight_len() != 0 && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(registry.in_flight_len(), 0);
}

struct PausedNativeAdapter {
    capabilities: ProviderCapabilities,
    started: AtomicUsize,
    finished: AtomicUsize,
    release: Arc<std::sync::atomic::AtomicBool>,
}

#[async_trait]
impl ProviderAdapter for PausedNativeAdapter {
    fn kind(&self) -> ProviderKind {
        self.capabilities.kind
    }

    async fn probe(
        &self,
        _executable: &ProviderExecutableHandle,
    ) -> Result<ProviderCapabilities, ProviderError> {
        self.started.fetch_add(1, Ordering::SeqCst);
        let release = Arc::clone(&self.release);
        tokio::task::spawn_blocking(move || {
            while !release.load(Ordering::Acquire) {
                std::thread::sleep(Duration::from_millis(5));
            }
        })
        .await
        .unwrap();
        self.finished.fetch_add(1, Ordering::SeqCst);
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
        _executable: &ProviderExecutableHandle,
    ) -> Result<Option<QuotaObservation>, ProviderError> {
        Ok(None)
    }
}

#[tokio::test]
async fn cancelled_paused_native_leader_stays_charged_until_native_join_and_rejects_new_work() {
    let temp = tempdir().unwrap();
    let executable = executable_file(temp.path(), "claude", b"paused-native-leader");
    let adapter = Arc::new(PausedNativeAdapter {
        capabilities: capabilities(
            ProviderKind::ClaudeCode,
            "fixture-1",
            ProviderAuthState::Unknown,
            CapabilitySupport::Unknown,
            CapabilitySupport::Unknown,
            CapabilitySupport::Unknown,
        ),
        started: AtomicUsize::new(0),
        finished: AtomicUsize::new(0),
        release: Arc::new(std::sync::atomic::AtomicBool::new(false)),
    });
    let release = Arc::clone(&adapter.release);
    let mut registry = ProviderRegistry::new();
    registry.register(adapter.clone()).unwrap();
    let config = discovery(Some(executable.clone()), None);

    let task_registry = Arc::new(registry);
    let observed_registry = Arc::clone(&task_registry);
    let observed = tokio::spawn(async move {
        observed_registry
            .observe(ProviderKind::ClaudeCode, &config)
            .await
    });
    let startup_deadline = Instant::now() + Duration::from_secs(2);
    while adapter.started.load(Ordering::Acquire) == 0 && Instant::now() < startup_deadline {
        tokio::task::yield_now().await;
    }
    assert_eq!(adapter.started.load(Ordering::Acquire), 1);
    observed.abort();
    let _ = observed.await;

    assert_eq!(task_registry.in_flight_len(), 1);
    let replacement = tokio::time::timeout(
        Duration::from_millis(100),
        task_registry.observe(ProviderKind::ClaudeCode, &discovery(Some(executable), None)),
    )
    .await;
    assert!(matches!(
        replacement,
        Err(_) | Ok(Err(ProviderError::Probe(ProviderProbeError::TimedOut)))
    ));
    assert_eq!(adapter.started.load(Ordering::Acquire), 1);

    // Dropping the registry must cancel admission without dropping the
    // retained worker or its native leader. Release it only after Drop and
    // prove the native task still joins exactly once.
    let registry = match Arc::try_unwrap(task_registry) {
        Ok(registry) => registry,
        Err(_) => panic!("no registry clones remain"),
    };
    drop(registry);
    release.store(true, Ordering::Release);
    let deadline = Instant::now() + Duration::from_secs(5);
    while adapter.finished.load(Ordering::Acquire) == 0 && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(adapter.finished.load(Ordering::Acquire), 1);
}

#[tokio::test]
async fn inflight_eviction_cancels_oldest_without_exceeding_native_leader_bound() {
    const MAX_PAUSED_LEADERS: usize = 64;

    let temp = tempdir().unwrap();
    let adapter = Arc::new(PausedNativeAdapter {
        capabilities: capabilities(
            ProviderKind::ClaudeCode,
            "fixture-1",
            ProviderAuthState::Unknown,
            CapabilitySupport::Unknown,
            CapabilitySupport::Unknown,
            CapabilitySupport::Unknown,
        ),
        started: AtomicUsize::new(0),
        finished: AtomicUsize::new(0),
        release: Arc::new(std::sync::atomic::AtomicBool::new(false)),
    });
    let mut registry = ProviderRegistry::new();
    registry.register(adapter.clone()).unwrap();
    let task_registry = Arc::new(registry);
    let mut tasks = Vec::new();
    for index in 0..MAX_PAUSED_LEADERS {
        let root = temp.path().join(format!("candidate-{index}"));
        std::fs::create_dir_all(&root).unwrap();
        let executable = executable_file(&root, "claude", b"paused-native-capacity");
        let observed_registry = Arc::clone(&task_registry);
        tasks.push(tokio::spawn(async move {
            observed_registry
                .observe(ProviderKind::ClaudeCode, &discovery(Some(executable), None))
                .await
        }));
    }

    let startup_deadline = Instant::now() + Duration::from_secs(5);
    while adapter.started.load(Ordering::Acquire) < MAX_PAUSED_LEADERS
        && Instant::now() < startup_deadline
    {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert_eq!(adapter.started.load(Ordering::Acquire), MAX_PAUSED_LEADERS);
    assert_eq!(task_registry.in_flight_len(), MAX_PAUSED_LEADERS);

    let rejected_root = temp.path().join("rejected");
    std::fs::create_dir_all(&rejected_root).unwrap();
    let rejected_executable = executable_file(&rejected_root, "claude", b"paused-native-rejected");
    let rejected = tokio::time::timeout(
        Duration::from_millis(250),
        task_registry.observe(
            ProviderKind::ClaudeCode,
            &discovery(Some(rejected_executable), None),
        ),
    )
    .await;
    assert!(matches!(
        rejected,
        Err(_) | Ok(Err(ProviderError::Probe(ProviderProbeError::TimedOut)))
    ));
    assert_eq!(adapter.started.load(Ordering::Acquire), MAX_PAUSED_LEADERS);
    assert_eq!(task_registry.in_flight_len(), MAX_PAUSED_LEADERS);

    adapter.release.store(true, Ordering::Release);
    for task in tasks {
        let _ = task.await.unwrap();
    }
    let deadline = Instant::now() + Duration::from_secs(5);
    while task_registry.in_flight_len() != 0 && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(adapter.finished.load(Ordering::Acquire), MAX_PAUSED_LEADERS);
    assert_eq!(task_registry.in_flight_len(), 0);
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
    let replacement_allowed = std::fs::OpenOptions::new()
        .append(true)
        .open(&executable)
        .and_then(|mut file| file.write_all(b"binary-replaced"))
        .is_ok();
    let second = registry
        .observe(ProviderKind::Codex, &config)
        .await
        .unwrap();

    assert_eq!(
        second.cache_status,
        if replacement_allowed {
            CacheStatus::Miss
        } else {
            CacheStatus::Hit
        }
    );
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

#[tokio::test]
async fn capability_cache_has_a_deterministic_bound_and_evicts_oldest_versions() {
    let temp = tempdir().unwrap();
    let executable = executable_file(temp.path(), "claude", b"bounded-cache");
    let adapter = FakeAdapter::new(capabilities(
        ProviderKind::ClaudeCode,
        "fixture-0",
        ProviderAuthState::Unknown,
        CapabilitySupport::Unknown,
        CapabilitySupport::Unknown,
        CapabilitySupport::Unknown,
    ));
    let mut registry = ProviderRegistry::new();
    registry.register(adapter.clone()).unwrap();
    let config = discovery(Some(executable), None);

    for index in 0..80_u32 {
        adapter.set_version(ProviderVersion::new(format!("fixture-{index}")).unwrap());
        registry
            .observe(ProviderKind::ClaudeCode, &config)
            .await
            .unwrap();
    }

    assert!(registry.cache_len() <= 64);

    adapter.set_version(ProviderVersion::new("fixture-0").unwrap());
    let oldest = registry
        .observe(ProviderKind::ClaudeCode, &config)
        .await
        .unwrap();
    assert_eq!(oldest.cache_status, CacheStatus::Miss);
}

#[test]
fn authenticated_subscription_fixture_is_rejected_without_a_registry_receipt() {
    let raw = include_str!("fixtures/providers/registry/authenticated_subscription.json");
    assert!(serde_json::from_str::<ProviderCapabilities>(raw).is_err());
}

#[test]
fn authenticated_states_require_matching_auth_status_evidence() {
    let forged = CapabilityEvidence::new(
        EvidenceSourceId::AuthStatusProbe,
        1_700_000_000_001,
        EvidenceStatus::Authenticated,
        None,
    );

    assert!(matches!(
        forged,
        Err(CapabilityEvidenceError::AuthEvidenceRequiresReceipt)
    ));
}

#[test]
fn auth_required_observation_contains_no_credentials_or_raw_output() {
    let raw = include_str!("fixtures/providers/registry/auth_required.json");
    assert!(serde_json::from_str::<ProviderCapabilities>(raw).is_err());
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
        Err(ProviderVersionError::NonCanonical)
    ));
    assert!(matches!(
        ProviderVersion::new("   "),
        Err(ProviderVersionError::NonCanonical)
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
    );
    assert!(matches!(
        evidence,
        Err(CapabilityEvidenceError::AuthEvidenceRequiresReceipt)
    ));

    let evidence = CapabilityEvidence::new(
        EvidenceSourceId::Registry,
        1_700_000_000_000,
        EvidenceStatus::Unknown,
        Some(EvidenceDiagnostic::new(
            EvidenceDiagnosticCode::ProbeFailed,
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
    assert_eq!(evidence.source(), EvidenceSourceId::Registry);
    assert_eq!(evidence.status(), EvidenceStatus::Unknown);
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
    let original = "opaque-\u{00e9}-bytes".to_string();
    let session = ProviderSessionId::new(original.clone()).unwrap();
    assert_eq!(session.as_str(), original);
    assert_eq!(session.as_bytes(), original.as_bytes());
    assert_eq!(
        ProviderSessionId::try_from(original.as_str()).unwrap(),
        session
    );

    assert!(ProviderSessionId::new("").is_err());
    assert!(ProviderSessionId::new(" surrounding-whitespace").is_err());
    assert!(ProviderSessionId::new("surrounding-whitespace ").is_err());
    assert!(ProviderSessionId::new("has\ncontrol").is_err());
    assert!(ProviderSessionId::new("x".repeat(MAX_PROVIDER_SESSION_ID_BYTES + 1)).is_err());
}

#[test]
fn provider_identity_values_reject_surrounding_whitespace_and_redact_display() {
    let secret_version = "provider-version-secret";
    assert!(devmanager::providers::ProviderVersion::new(" fixture-1 ").is_err());
    let version = devmanager::providers::ProviderVersion::new(secret_version).unwrap();
    assert!(!format!("{version:?}").contains(secret_version));
    assert!(!version.to_string().contains(secret_version));

    let secret_session = "provider-session-secret";
    assert!(ProviderSessionId::new(" session ").is_err());
    let session = ProviderSessionId::new(secret_session).unwrap();
    assert!(!format!("{session:?}").contains(secret_session));
    assert!(!session.to_string().contains(secret_session));
}

#[test]
fn provider_kind_wire_rejects_noncanonical_alias() {
    assert!(serde_json::from_value::<ProviderKind>(serde_json::json!("claude_code")).is_err());
    assert_eq!(
        serde_json::to_value(ProviderKind::ClaudeCode).unwrap(),
        serde_json::json!("claude")
    );
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
    let executable = test_executable_handle();
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

#[tokio::test]
async fn provider_observation_wire_is_versioned_and_validates_cross_fields() {
    let temp = tempdir().unwrap();
    let executable = executable_file(temp.path(), "claude", b"observation-wire");
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
        .observe(ProviderKind::ClaudeCode, &discovery(Some(executable), None))
        .await
        .unwrap();
    let encoded = serde_json::to_value(&observation).unwrap();

    assert_eq!(encoded["schema_version"], serde_json::json!(1));

    let mut missing_schema = encoded.clone();
    missing_schema
        .as_object_mut()
        .unwrap()
        .remove("schema_version");
    assert!(serde_json::from_value::<ProviderObservation>(missing_schema).is_err());

    let mut mismatched_kind = encoded;
    mismatched_kind["kind"] = serde_json::json!("codex");
    assert!(serde_json::from_value::<ProviderObservation>(mismatched_kind).is_err());

    let mut unknown = serde_json::to_value(&observation).unwrap();
    unknown["unexpected"] = serde_json::json!(true);
    assert!(serde_json::from_value::<ProviderObservation>(unknown).is_err());

    let raw = serde_json::to_string(&serde_json::to_value(&observation).unwrap()).unwrap();
    let duplicate = raw.replacen(
        "\"schema_version\":1,",
        "\"schema_version\":1,\"schema_version\":1,",
        1,
    );
    assert!(serde_json::from_str::<ProviderObservation>(&duplicate).is_err());

    let cache_key = CapabilityCacheKey::new(
        observation.kind,
        observation.executable.clone(),
        observation.version.clone(),
        observation.adapter_revision,
        observation.semantic_schema_version,
    );
    let cache_value = serde_json::to_value(&cache_key).unwrap();
    assert_eq!(cache_value["schema_version"], serde_json::json!(1));
    let mut cache_missing_schema = cache_value.clone();
    cache_missing_schema
        .as_object_mut()
        .unwrap()
        .remove("schema_version");
    assert!(serde_json::from_value::<CapabilityCacheKey>(cache_missing_schema).is_err());
    let mut cache_unknown = cache_value;
    cache_unknown["unexpected"] = serde_json::json!(true);
    assert!(serde_json::from_value::<CapabilityCacheKey>(cache_unknown).is_err());
}

#[test]
fn agent_session_facts_deserialization_preserves_exact_opaque_provider_id() {
    let exact = "opaque\u{00e9}-provider-id";
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
    let exact = "opaque\u{00e9}-constructor-id";
    let session = ProviderSessionId::new(exact).unwrap();
    let facts = AgentSessionFacts::new(
        TaskId::new(),
        AgentRole::Primary,
        ProviderKind::ClaudeCode,
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
    );
    assert!(matches!(
        evidence,
        Err(CapabilityEvidenceError::AuthEvidenceRequiresReceipt)
    ));

    let stable = CapabilityEvidence::new(
        EvidenceSourceId::Registry,
        1_700_000_000_000,
        EvidenceStatus::Unknown,
        Some(EvidenceDiagnostic::new(
            EvidenceDiagnosticCode::ProbeFailed,
            Some([0xabu8; 32]),
        )),
    )
    .unwrap();
    let encoded: Value = serde_json::to_value(&stable).unwrap();
    let object = encoded.as_object().unwrap();
    assert_eq!(object["source"], "registry");
    assert_eq!(object["status"], "unknown");
    assert!(object.contains_key("observed_at"));
    assert!(object.contains_key("diagnostic"));
    assert!(!object.contains_key("detail"));
    assert!(!object.contains_key("command"));
    assert!(!object.contains_key("path"));
    assert!(
        serde_json::from_value::<CapabilityEvidence>(serde_json::json!({
            "schema_version": 1,
            "source": "registry",
            "observed_at": 1_700_000_000_000u64,
            "expires_at": null,
            "confidence": "unknown",
            "auth_source": null,
            "status": "unknown",
            "diagnostic": null,
            "detail": "raw stdout OPENAI_API_KEY=secret"
        }))
        .is_err()
    );
}

#[test]
fn probe_requests_include_typed_noninteractive_auth_status_contract() {
    let request = ProviderProbeRequest::auth_status(test_executable_handle()).unwrap();

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

    async fn probe(
        &self,
        _executable: &ProviderExecutableHandle,
    ) -> Result<ProviderCapabilities, ProviderError> {
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
        _executable: &ProviderExecutableHandle,
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
    let launch = adapter.build_launch(LaunchProviderRequest::new(
        executable.open_for_launch().unwrap(),
        None,
        None,
    ));
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
        .observe_quota(&executable.open_for_launch().unwrap())
        .await
        .unwrap();
    assert!(quota.is_none());
}

#[test]
fn provider_probe_result_has_bounded_structured_status() {
    let request = ProviderProbeRequest::with_limits(
        test_executable_handle(),
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
fn provider_errors_and_probe_arguments_redact_sensitive_paths_and_tokens() {
    let secret = "provider-secret-token";
    let secret_path = PathBuf::from(format!("C:/private/{secret}/claude.exe"));
    let errors = vec![
        ProviderError::MissingCli {
            kind: ProviderKind::ClaudeCode,
            requested: Some(secret_path.clone()),
        },
        ProviderError::WrapperCommandNotAllowed {
            path: secret_path.clone(),
        },
        ProviderError::ExecutableNotAllowed {
            kind: ProviderKind::ClaudeCode,
            path: secret_path.clone(),
        },
        ProviderError::Executable(ProviderExecutableError::Missing(secret_path.clone())),
    ];
    for error in errors {
        assert!(!format!("{error:?}").contains(secret));
        assert!(!error.to_string().contains(secret));
    }

    let executable_errors = vec![
        ProviderExecutableError::EmptyPath,
        ProviderExecutableError::PathTooLong,
        ProviderExecutableError::Missing(secret_path.clone()),
        ProviderExecutableError::NotAFile(secret_path.clone()),
        ProviderExecutableError::NotNativeExecutable(secret_path.clone()),
        ProviderExecutableError::SymlinkOrReparse(secret_path.clone()),
        ProviderExecutableError::HardlinkAmbiguous(secret_path.clone()),
        ProviderExecutableError::ChangedDuringValidation(secret_path.clone()),
        ProviderExecutableError::InvalidFileIdentity(secret_path.clone()),
        ProviderExecutableError::UnsupportedPlatform(secret_path.clone()),
        ProviderExecutableError::NotCanonical {
            requested: secret_path.clone(),
            canonical: secret_path.clone(),
        },
        ProviderExecutableError::HashMismatch(secret_path.clone()),
        ProviderExecutableError::UnsupportedSchemaVersion(2),
        ProviderExecutableError::Io {
            path: secret_path.clone(),
            kind: std::io::ErrorKind::Other,
        },
        ProviderExecutableError::BackgroundTask,
    ];
    for error in executable_errors {
        assert!(!format!("{error:?}").contains(secret));
        assert!(!error.to_string().contains(secret));
    }

    let discovery_errors = vec![
        devmanager::providers::ProviderDiscoveryError::UnsupportedPlatform,
        devmanager::providers::ProviderDiscoveryError::NoCandidate(ProviderKind::ClaudeCode),
        devmanager::providers::ProviderDiscoveryError::InvalidPathSnapshot(secret_path.clone()),
        devmanager::providers::ProviderDiscoveryError::OriginNotAllowed(secret_path.clone()),
        devmanager::providers::ProviderDiscoveryError::ForbiddenRunner(secret_path.clone()),
        devmanager::providers::ProviderDiscoveryError::WrongEntrypoint(secret_path.clone()),
        devmanager::providers::ProviderDiscoveryError::WrongFileType(secret_path.clone()),
        devmanager::providers::ProviderDiscoveryError::ShimProofInvalid(secret_path.clone()),
        devmanager::providers::ProviderDiscoveryError::Executable(
            ProviderExecutableError::Missing(secret_path.clone()),
        ),
    ];
    for error in discovery_errors {
        assert!(!format!("{error:?}").contains(secret));
        assert!(!error.to_string().contains(secret));
    }

    let wrapped_errors = vec![
        ProviderError::Discovery(devmanager::providers::ProviderDiscoveryError::Executable(
            ProviderExecutableError::Missing(secret_path.clone()),
        )),
        ProviderError::AuthEvidence(
            devmanager::providers::ProviderAuthEvidenceError::ExecutableChanged(
                ProviderExecutableError::Missing(secret_path.clone()),
            ),
        ),
        ProviderError::UntrustedAuthenticationEvidence,
    ];
    for error in wrapped_errors {
        assert!(!format!("{error:?}").contains(secret));
        assert!(!error.to_string().contains(secret));
    }

    let request = ProviderProbeRequest::help(test_executable_handle()).unwrap();
    assert!(!format!("{request:?}").contains(secret));

    let argument = ProviderArgument::new(secret).unwrap();
    assert!(!format!("{argument:?}").contains(secret));

    let input = ProviderInput::new(secret.as_bytes().to_vec()).unwrap();
    assert!(!format!("{input:?}").contains(secret));

    let result = ProviderProbeResult::completed(&request, 0, 0, 0).unwrap();
    assert!(!format!("{result:?}").contains(secret));
}

#[tokio::test]
async fn registry_rejects_relative_and_oversized_path_entries_before_fallback() {
    let temp = tempdir().unwrap();
    let path_root = temp.path().join("fallback");
    std::fs::create_dir_all(&path_root).unwrap();
    let executable = executable_file(&path_root, "claude", b"path-boundary");
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

    let separator = if cfg!(windows) { ";" } else { ":" };
    let relative_path = OsString::from(format!(".{separator}{}", path_root.display()));
    let relative_result = registry
        .resolve_executable(
            ProviderKind::ClaudeCode,
            &ProviderDiscoveryConfig {
                executable_override: None,
                path: Some(relative_path),
            },
        )
        .await;
    assert!(relative_result.is_err());

    let oversized_entry = "x".repeat(16 * 1024);
    let oversized_path = OsString::from(format!(
        "{oversized_entry}{separator}{}",
        path_root.display()
    ));
    let oversized_result = registry
        .resolve_executable(
            ProviderKind::ClaudeCode,
            &ProviderDiscoveryConfig {
                executable_override: None,
                path: Some(oversized_path),
            },
        )
        .await;
    assert!(oversized_result.is_err());

    assert!(executable.exists());
}

#[test]
fn executable_and_probe_result_constructors_reject_unbounded_inputs() {
    assert!(ProviderExecutable::new(PathBuf::new(), [0; 32]).is_err());
    let request = ProviderProbeRequest::with_limits(
        test_executable_handle(),
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

    async fn probe(
        &self,
        _executable: &ProviderExecutableHandle,
    ) -> Result<ProviderCapabilities, ProviderError> {
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
        _executable: &ProviderExecutableHandle,
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
    let after_root = tempdir().unwrap();
    let after_path = executable_file(after_root.path(), "claude", b"after");
    let after = ProviderExecutable::from_path(&after_path).unwrap();
    let inspector = Arc::new(SequenceInspector {
        // The inspector consumes from the back: the first observation must
        // agree with the configured path, while the post-probe observation is
        // a different attested graph.
        identities: Mutex::new(vec![after, before]),
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
            &discovery(Some(identity_path), None),
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
        ProviderExecutable::from_path(&executable)
            .unwrap()
            .open_for_launch()
            .unwrap(),
        devmanager::providers::ProviderProbeKind::Help,
        std::time::Duration::from_secs(5),
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
        ProviderExecutable::from_path(&executable)
            .unwrap()
            .open_for_launch()
            .unwrap(),
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
        ProviderExecutable::from_path(&executable)
            .unwrap()
            .open_for_launch()
            .unwrap(),
        devmanager::providers::ProviderProbeKind::Help,
        std::time::Duration::from_secs(3),
        4096,
    )
    .unwrap();

    let result = runner.run(request).await;

    assert!(matches!(result, Err(ProviderProbeError::TimedOut)));
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
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

#[cfg(unix)]
#[tokio::test]
async fn unix_probe_runner_timeout_kills_and_joins_the_entire_probe_tree() {
    let temp = tempdir().unwrap();
    let executable = copied_probe_fixture(&temp, "probe-tree");
    let child_pid_path = executable.with_extension("child.pid");
    let runner = probe_runner(&executable);
    let request = ProviderProbeRequest::with_limits(
        ProviderExecutable::from_path(&executable)
            .unwrap()
            .open_for_launch()
            .unwrap(),
        devmanager::providers::ProviderProbeKind::Help,
        std::time::Duration::from_secs(2),
        4096,
    )
    .unwrap();

    let result = runner.run(request).await;

    assert!(matches!(result, Err(ProviderProbeError::TimedOut)));
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
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
