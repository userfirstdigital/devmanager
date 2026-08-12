use cargo_packager_updater::{
    self,
    semver::{BuildMetadata, Prerelease, Version},
    url::Url,
    Config as PackagerUpdaterConfig, Update as PackagerUpdate, UpdaterBuilder,
    WindowsConfig as PackagerWindowsConfig, WindowsUpdateInstallMode,
};
use serde::Deserialize;
use std::cmp::Ordering;
use std::collections::HashMap;
use std::path::Path;
use std::sync::{
    atomic::{AtomicBool, Ordering as AtomicOrdering},
    Arc, Mutex, RwLock,
};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub mod handoff;
pub mod replace;

pub use handoff::{
    assert_atomic_installer_bundle, capture_preservation_checkpoint, classify_user_state_path,
    extract_build_version, inspect_atomic_installer_payload_dir, update_state_policy,
    validate_preservation_checkpoint, verify_downloaded_artifact_sha256, ActiveResourceProbe,
    ActiveUpdateResource, AtomicBundleError, AtomicInstallerBundle, FixedActiveResourceProbe,
    HandoffBlockReason, HostUpdateAdmission, HostUpdateControlPort, HostUpdateHandoff,
    HostUpdateRuntimeGate, IdentityPreservationReport, IgnoredUserStateKind,
    PreservationCheckpoint, PreservationError, PreservedUserStateKind, SilentReplacementDecision,
    UpdateCutoverKind, UpdateHandoffError, UpdateHandoffMachine, UpdateHandoffPhase,
    UpdateHandoffRecoveryMarker, UpdateHandoffToken, UpdateResourceInspection,
    UserStateClassification, UPDATE_HANDOFF_RECOVERY_MARKER, UPDATE_IPC_DEADLINE,
};
pub use replace::{
    clear_update_handoff_recovery_marker, persist_update_handoff_recovery_marker,
    read_update_handoff_recovery_marker, StagedBinaryReplacement, StagedReplaceError,
    StagedReplacePhase, StagedReplaceProgress,
};

const UPDATE_ENDPOINTS_VAR: &str = "DEVMANAGER_UPDATE_ENDPOINTS";
const UPDATE_PUBKEY_VAR: &str = "DEVMANAGER_UPDATE_PUBKEY";
const UPDATE_WINDOWS_INSTALL_MODE_VAR: &str = "DEVMANAGER_UPDATE_WINDOWS_INSTALL_MODE";
const BACKGROUND_UPDATE_INTERVAL: Duration = Duration::from_secs(30 * 60);
const READY_UPDATE_RECHECK_INTERVAL: Duration = Duration::from_secs(5 * 60);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdaterStage {
    Disabled,
    Idle,
    Checking,
    UpToDate,
    UpdateAvailable,
    Downloading,
    ReadyToInstall,
    Installing,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdaterSnapshot {
    pub configured: bool,
    pub current_version: String,
    pub endpoints: Vec<String>,
    pub stage: UpdaterStage,
    pub target_version: Option<String>,
    pub detail: String,
    pub release_notes: Option<String>,
    pub last_checked_at: Option<SystemTime>,
    pub downloaded_bytes: u64,
    pub total_bytes: Option<u64>,
}

impl UpdaterSnapshot {
    pub fn is_busy(&self) -> bool {
        matches!(
            self.stage,
            UpdaterStage::Checking | UpdaterStage::Downloading | UpdaterStage::Installing
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdaterWindowsInstallMode {
    BasicUi,
    Quiet,
    Passive,
}

impl UpdaterWindowsInstallMode {
    fn into_packager(self) -> WindowsUpdateInstallMode {
        match self {
            Self::BasicUi => WindowsUpdateInstallMode::BasicUi,
            Self::Quiet => WindowsUpdateInstallMode::Quiet,
            Self::Passive => WindowsUpdateInstallMode::Passive,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedUpdaterConfig {
    pub endpoints: Vec<String>,
    pub pubkey: String,
    pub windows_install_mode: UpdaterWindowsInstallMode,
}

impl ResolvedUpdaterConfig {
    fn into_packager_config(self) -> Result<PackagerUpdaterConfig, String> {
        let endpoints = self
            .endpoints
            .iter()
            .map(|endpoint| {
                Url::parse(endpoint).map_err(|error| {
                    format!("Failed to parse updater endpoint `{endpoint}`: {error}")
                })
            })
            .collect::<Result<Vec<_>, _>>()?;

        Ok(PackagerUpdaterConfig {
            endpoints,
            pubkey: self.pubkey,
            windows: Some(PackagerWindowsConfig {
                installer_args: None,
                install_mode: Some(self.windows_install_mode.into_packager()),
            }),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ReleaseManifest {
    pub version: String,
    #[serde(default)]
    pub notes: Option<String>,
    #[serde(default)]
    pub pub_date: Option<String>,
    /// Minimum negotiated protocol (`major.minor`) required by the release.
    #[serde(default)]
    pub minimum_protocol: Option<String>,
    #[serde(default)]
    pub platforms: HashMap<String, ReleaseManifestPlatform>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ReleaseManifestPlatform {
    pub format: String,
    pub signature: String,
    pub url: String,
    /// Immutable artifact digest, typically `sha256:<hex>`.
    #[serde(default)]
    pub hash: Option<String>,
    /// Packaged client identity, e.g. `devmanager/0.4.2`.
    #[serde(default)]
    pub client_build: Option<String>,
    /// Packaged host identity, e.g. `devmanager-host/0.4.2`.
    #[serde(default)]
    pub host_build: Option<String>,
}

/// Installed package identity used for update detection.
///
/// Always derived from package/binary metadata — never from a development
/// checkout file or stale PWA asset.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstalledPackageIdentity {
    pub version: Version,
    pub source: PackageVersionSource,
    pub client_build: String,
    pub host_build: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PackageVersionSource {
    /// Windows VERSIONINFO / ProductVersion from the running binary.
    BinaryMetadata,
    /// Compile-time package metadata stamped into the binary.
    EmbeddedPackageMetadata,
}

/// Why a remote candidate must not be admitted for download.
///
/// Manifest field checks are a prefilter only. Cryptographic signature validity
/// is established solely by `cargo_packager_updater` during download verification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateRejection {
    MalformedManifestField {
        detail: String,
    },
    Downgrade {
        current: String,
        remote: String,
    },
    MatchingVersion {
        version: String,
    },
    HostClientMismatch {
        client_build: String,
        host_build: String,
    },
    MissingPlatform {
        platform: String,
    },
    InvalidRemoteVersion {
        detail: String,
    },
    StaleCachedMetadata {
        detail: String,
    },
    SignatureNotVerifiedByPackager,
    ProtocolIncompatible {
        required: String,
        local: String,
    },
    MissingRequiredSha256,
    ArtifactHashMismatch {
        expected: String,
        actual: String,
    },
    MissingBuildIdentity,
}

impl std::fmt::Display for UpdateRejection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MalformedManifestField { detail } => {
                write!(f, "malformed update manifest field: {detail}")
            }
            Self::Downgrade { current, remote } => {
                write!(f, "refusing downgrade from {current} to {remote}")
            }
            Self::MatchingVersion { version } => {
                write!(f, "remote version {version} matches the installed build")
            }
            Self::HostClientMismatch {
                client_build,
                host_build,
            } => write!(
                f,
                "host/client build mismatch: client={client_build} host={host_build}"
            ),
            Self::MissingPlatform { platform } => {
                write!(f, "release manifest is missing platform `{platform}`")
            }
            Self::InvalidRemoteVersion { detail } => {
                write!(f, "invalid remote version: {detail}")
            }
            Self::StaleCachedMetadata { detail } => {
                write!(f, "stale cached update metadata: {detail}")
            }
            Self::SignatureNotVerifiedByPackager => write!(
                f,
                "update signature was not verified by cargo_packager_updater"
            ),
            Self::ProtocolIncompatible { required, local } => write!(
                f,
                "release requires protocol {required} but local is {local}"
            ),
            Self::MissingRequiredSha256 => {
                write!(
                    f,
                    "release manifest is missing required sha256 artifact hash"
                )
            }
            Self::ArtifactHashMismatch { expected, actual } => {
                write!(
                    f,
                    "downloaded artifact hash mismatch: expected {expected}, actual {actual}"
                )
            }
            Self::MissingBuildIdentity => {
                write!(
                    f,
                    "release manifest is missing required client/host build identity"
                )
            }
        }
    }
}

impl std::error::Error for UpdateRejection {}

/// Admitted remote release ready for download.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdmittedUpdate {
    pub version: Version,
    pub notes: Option<String>,
    pub platform: String,
    pub url: String,
    pub signature: String,
    pub format: Option<String>,
    pub hash: Option<String>,
    pub minimum_protocol: Option<String>,
    pub client_build: String,
    pub host_build: String,
}

/// Cache-busting headers and query policy for latest.json fetches.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheBustingRequestPolicy {
    pub cache_control: &'static str,
    pub pragma: &'static str,
    pub query_param: &'static str,
    pub nonce: String,
}

impl CacheBustingRequestPolicy {
    pub fn for_instant(now: SystemTime) -> Self {
        let millis = now
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        Self {
            cache_control: "no-cache, no-store, must-revalidate",
            pragma: "no-cache",
            query_param: "devmanager_cb",
            nonce: millis.to_string(),
        }
    }

    pub fn apply_to_endpoint(&self, endpoint: &str) -> Result<String, String> {
        let mut url = Url::parse(endpoint)
            .map_err(|error| format!("Failed to parse updater endpoint `{endpoint}`: {error}"))?;
        url.query_pairs_mut()
            .append_pair(self.query_param, &self.nonce);
        Ok(url.to_string())
    }

    pub fn header_pairs(&self) -> [(&'static str, &'static str); 3] {
        [
            ("Cache-Control", self.cache_control),
            ("Pragma", self.pragma),
            ("X-DevManager-Update-Check", "1"),
        ]
    }
}

#[derive(Clone)]
pub struct UpdaterService {
    inner: Arc<UpdaterInner>,
}

/// Options for [`UpdaterService::install_update_with_options`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct InstallUpdateOptions {
    /// When true, active resources may proceed after explicit confirm/drain.
    /// When false, active resources refuse silent replacement.
    pub allow_explicit_confirm_with_active: bool,
}

struct UpdaterInner {
    current_version: Version,
    config: Option<PackagerUpdaterConfig>,
    background_checks_started: AtomicBool,
    state: RwLock<UpdaterState>,
    /// Shared host-owned FSM (bound from HostRequestHandle / tests).
    update_gate: Mutex<Option<Arc<HostUpdateRuntimeGate>>>,
    /// Timed IPC port that drives the same host gate across the process boundary.
    control_port: Mutex<Option<Box<dyn HostUpdateControlPort>>>,
    resource_probe: Mutex<Option<Box<dyn ActiveResourceProbe>>>,
    /// The exact token returned by PrepareUpdate and confirmed for drain.
    ///
    /// The public install API intentionally accepts only the token id when the
    /// irreversible phase begins, so retain the full token here until that
    /// phase consumes it.  This keeps the durable recovery marker correlated
    /// to the original host boot and inspection rather than reconstructing
    /// those fields from defaults.
    prepared_token: Mutex<Option<UpdateHandoffToken>>,
    /// Live Host Hello `server_build` — never fabricated from checkout metadata.
    live_host_build: Mutex<Option<String>>,
    live_protocol: Mutex<Option<(u16, u16)>>,
}

struct DownloadedUpdate {
    update: PackagerUpdate,
    bytes: Vec<u8>,
    /// Set only after packager download+verify and sha256 of actual bytes succeed.
    package_identity: AtomicInstallerBundle,
    /// Required digest from the signed manifest; hashed against `bytes`.
    required_hash: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CheckPlan {
    Fresh,
    PreserveReady,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AutoDownloadAction {
    Start,
    KeepReady,
}

struct UpdaterState {
    snapshot: UpdaterSnapshot,
    pending_update: Option<PackagerUpdate>,
    ready_update: Option<DownloadedUpdate>,
    /// Release identity from evaluate_release_candidate; required before ReadyToInstall.
    pending_release: Option<PendingReleaseIdentity>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PendingReleaseIdentity {
    required_hash: String,
    client_build: String,
    host_build: String,
    protocol_major: u16,
    protocol_minor: u16,
    packager_target: String,
    download_url: String,
    signature: String,
    format: String,
}

impl UpdaterService {
    pub fn new() -> Self {
        let identity = resolve_running_package_identity();
        let current_version = identity.version.clone();
        let resolved = resolve_embedded_config();
        let (config, snapshot) = match resolved {
            Ok(Some(config)) => {
                let snapshot = UpdaterSnapshot {
                    configured: true,
                    current_version: current_version.to_string(),
                    endpoints: config.endpoints.clone(),
                    stage: UpdaterStage::Idle,
                    target_version: None,
                    detail: format!(
                        "Ready to check {} for updates.",
                        summarize_endpoint(config.endpoints.first())
                    ),
                    release_notes: None,
                    last_checked_at: None,
                    downloaded_bytes: 0,
                    total_bytes: None,
                };
                match config.into_packager_config() {
                    Ok(config) => (Some(config), snapshot),
                    Err(error) => (
                        None,
                        UpdaterSnapshot {
                            configured: false,
                            current_version: current_version.to_string(),
                            endpoints: Vec::new(),
                            stage: UpdaterStage::Disabled,
                            target_version: None,
                            detail: format!("Updater is disabled: {error}"),
                            release_notes: None,
                            last_checked_at: None,
                            downloaded_bytes: 0,
                            total_bytes: None,
                        },
                    ),
                }
            }
            Ok(None) => (
                None,
                UpdaterSnapshot {
                    configured: false,
                    current_version: current_version.to_string(),
                    endpoints: Vec::new(),
                    stage: UpdaterStage::Disabled,
                    target_version: None,
                    detail: format!(
                        "Updater is disabled. Build with {UPDATE_ENDPOINTS_VAR} and \
{UPDATE_PUBKEY_VAR} to enable GitHub-hosted updates."
                    ),
                    release_notes: None,
                    last_checked_at: None,
                    downloaded_bytes: 0,
                    total_bytes: None,
                },
            ),
            Err(error) => (
                None,
                UpdaterSnapshot {
                    configured: false,
                    current_version: current_version.to_string(),
                    endpoints: Vec::new(),
                    stage: UpdaterStage::Disabled,
                    target_version: None,
                    detail: format!("Updater is disabled: {error}"),
                    release_notes: None,
                    last_checked_at: None,
                    downloaded_bytes: 0,
                    total_bytes: None,
                },
            ),
        };

        Self {
            inner: Arc::new(UpdaterInner {
                current_version,
                config,
                background_checks_started: AtomicBool::new(false),
                state: RwLock::new(UpdaterState {
                    snapshot,
                    pending_update: None,
                    ready_update: None,
                    pending_release: None,
                }),
                update_gate: Mutex::new(None),
                control_port: Mutex::new(None),
                resource_probe: Mutex::new(None),
                prepared_token: Mutex::new(None),
                live_host_build: Mutex::new(None),
                live_protocol: Mutex::new(None),
            }),
        }
    }

    /// Bind the shared host-owned update FSM + timed IPC control + owned probe.
    pub fn bind_host_update_runtime(
        &self,
        gate: Arc<HostUpdateRuntimeGate>,
        control: Box<dyn HostUpdateControlPort>,
        probe: Box<dyn ActiveResourceProbe>,
    ) {
        if let Ok(mut slot) = self.inner.update_gate.lock() {
            *slot = Some(gate);
        }
        if let Ok(mut slot) = self.inner.control_port.lock() {
            *slot = Some(control);
        }
        self.set_active_resource_probe(probe);
    }

    /// Record live Host Hello identity (protocol + `server_build`).
    pub fn bind_live_host_hello(
        &self,
        server_build: impl Into<String>,
        protocol_major: u16,
        protocol_minor: u16,
    ) {
        if let Ok(mut slot) = self.inner.live_host_build.lock() {
            *slot = Some(server_build.into());
        }
        if let Ok(mut slot) = self.inner.live_protocol.lock() {
            *slot = Some((protocol_major, protocol_minor));
        }
    }

    /// Production client startup: bind live Host Hello and fail closed when a
    /// residual recovery marker disagrees with the negotiated Hello.
    pub fn observe_production_host_hello(
        &self,
        server_build: &str,
        protocol_major: u16,
        protocol_minor: u16,
        install_dir: &Path,
    ) -> Result<(), String> {
        self.bind_live_host_hello(server_build, protocol_major, protocol_minor);
        if let Some(marker) = read_update_handoff_recovery_marker(install_dir)? {
            marker.validate_live_host_hello(server_build, protocol_major, protocol_minor)?;
        }
        Ok(())
    }

    /// Inject an owned Send+'static active-resource probe (host IPC / executor).
    ///
    /// Borrowed probes must not be stored here. Use
    /// [`crate::host::update::OwnedActiveResourceProbe`].
    pub fn set_active_resource_probe(&self, probe: Box<dyn ActiveResourceProbe>) {
        if let Ok(mut slot) = self.inner.resource_probe.lock() {
            *slot = Some(probe);
        }
    }

    pub fn clear_active_resource_probe(&self) {
        if let Ok(mut slot) = self.inner.resource_probe.lock() {
            *slot = None;
        }
    }

    pub fn handoff_admission(&self) -> HostUpdateAdmission {
        self.inner
            .update_gate
            .lock()
            .ok()
            .and_then(|guard| guard.as_ref().map(|gate| gate.admission()))
            .unwrap_or(HostUpdateAdmission::Ready)
    }

    /// Abort a prepared handoff before irreversible durable seal.
    pub fn abort_update_handoff(&self) -> Result<(), String> {
        self.inner.clear_prepared_token();
        let deadline = Instant::now() + UPDATE_IPC_DEADLINE;
        if let Ok(port) = self.inner.control_port.lock() {
            if let Some(port) = port.as_ref() {
                return port.abort_pre_install(deadline);
            }
        }
        let gate = self
            .inner
            .update_gate
            .lock()
            .map_err(|_| "Update gate lock is unavailable.".to_string())?;
        let gate = gate.as_ref().ok_or_else(|| {
            "Host update gate is not bound; bind HostRequestHandle / HostUpdateRuntimeGate first."
                .to_string()
        })?;
        gate.abort_pre_install()
            .map(|_| ())
            .map_err(|error| error.to_string())
    }

    /// Prepare admission only (inspect+token). Confirm drain separately or via full gate.
    pub fn prepare_update_install(
        &self,
        options: InstallUpdateOptions,
    ) -> Result<UpdateHandoffToken, String> {
        let ready_meta = {
            let state = self
                .inner
                .state
                .read()
                .map_err(|_| "Updater state is unavailable.".to_string())?;
            let ready = state
                .ready_update
                .as_ref()
                .ok_or_else(|| "No downloaded update is ready to install.".to_string())?;
            self.inner
                .validate_ready_update(ready)
                .map_err(|error| error.to_string())?;
            (
                ready.update.version.clone(),
                ready.package_identity.client_build.clone(),
                ready.package_identity.host_build.clone(),
            )
        };
        let (version, client_build, host_build) = ready_meta;
        let deadline = Instant::now() + UPDATE_IPC_DEADLINE;

        if let Ok(port) = self.inner.control_port.lock() {
            if let Some(port) = port.as_ref() {
                let token = port.prepare_update(
                    &version,
                    &client_build,
                    &host_build,
                    options.allow_explicit_confirm_with_active,
                    deadline,
                )?;
                port.confirm_drain(token.token_id, Instant::now() + UPDATE_IPC_DEADLINE)?;
                if let Err(error) = self.remember_prepared_token(token.clone()) {
                    let _ = port.abort_pre_install(Instant::now() + UPDATE_IPC_DEADLINE);
                    return Err(error);
                }
                return Ok(token);
            }
        }

        let mut probe_slot = self
            .inner
            .resource_probe
            .lock()
            .map_err(|_| "Update resource probe lock is unavailable.".to_string())?;
        let probe = probe_slot.as_mut().ok_or_else(|| {
            "Active resource probe is required before install; bind an owned Host IPC/executor probe."
                .to_string()
        })?;
        let gate_slot = self
            .inner
            .update_gate
            .lock()
            .map_err(|_| "Update gate lock is unavailable.".to_string())?;
        let gate = gate_slot.as_ref().ok_or_else(|| {
            "Host update gate is not bound; bind HostRequestHandle / HostUpdateRuntimeGate first."
                .to_string()
        })?;
        let now = SystemTime::now();
        match gate.prepare_update(
            probe.as_mut(),
            &version,
            &client_build,
            &host_build,
            now,
            options.allow_explicit_confirm_with_active,
        ) {
            Ok(token) => {
                if let Err(error) = gate.confirm_drain(token.token_id, now) {
                    let _ = gate.abort_pre_install();
                    return Err(error.to_string());
                }
                if let Err(error) = self.remember_prepared_token(token.clone()) {
                    let _ = gate.abort_pre_install();
                    return Err(error);
                }
                Ok(token)
            }
            Err(error) => {
                let _ = gate.abort_pre_install();
                Err(error.to_string())
            }
        }
    }

    /// Final irreversible-capable old-process action: staged two-binary replace.
    ///
    /// Call only after host admission/drain and resource shutdown. Durably stages
    /// backups before sealing irreversibility; launch/stage failure aborts to a
    /// retryable Ready admission and retains ready bytes.
    pub fn launch_verified_installer(
        &self,
        token_id: uuid::Uuid,
    ) -> Result<InstallerLaunchOutcome, String> {
        let now = SystemTime::now();
        let prepared_token = self.inner.take_prepared_token(token_id)?;
        let ready_update = {
            let state = self
                .inner
                .state
                .read()
                .map_err(|_| "Updater state is unavailable.".to_string())?;
            let ready = state
                .ready_update
                .as_ref()
                .ok_or_else(|| "No downloaded update is ready to install.".to_string())?;
            self.inner.validate_ready_update(ready)?;
            DownloadedUpdate {
                update: ready.update.clone(),
                bytes: ready.bytes.clone(),
                package_identity: ready.package_identity.clone(),
                required_hash: ready.required_hash.clone(),
            }
        };

        // Correlate the prepared host token with the ready package before any
        // stage, seal, or install action can become irreversible.
        if prepared_token.target_version != ready_update.update.version
            || prepared_token.client_build != ready_update.package_identity.client_build
            || prepared_token.host_build != ready_update.package_identity.host_build
        {
            let _ = self.abort_update_handoff();
            self.inner.restore_ready_snapshot(Some(
                "Prepared update token does not match the ready package identity.".into(),
            ));
            return Err("Prepared update token does not match the ready package identity.".into());
        }

        {
            let deadline = Instant::now() + UPDATE_IPC_DEADLINE;
            if let Ok(port) = self.inner.control_port.lock() {
                if let Some(port) = port.as_ref() {
                    port.begin_atomic_install(token_id, deadline)?;
                } else {
                    self.with_bound_gate(|gate| {
                        gate.begin_atomic_install(token_id, now)
                            .map_err(|error| error.to_string())
                    })?;
                }
            } else {
                self.with_bound_gate(|gate| {
                    gate.begin_atomic_install(token_id, now)
                        .map_err(|error| error.to_string())
                })?;
            }
        }

        if let Err(error) = self.inner.mark_installing_snapshot(&ready_update) {
            let _ = self.abort_update_handoff();
            return Err(error);
        }

        let install_dir = ready_update
            .update
            .extract_path
            .clone()
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| ready_update.update.extract_path.clone());
        let staged_dir = install_dir.join(".devmanager-update-stage");
        if let Err(error) = materialize_staged_binaries(
            &staged_dir,
            &ready_update.bytes,
            &ready_update.package_identity,
        ) {
            let _ = self.abort_update_handoff();
            self.inner.restore_ready_snapshot(Some(format!(
                "Staging failed; ready update retained for retry: {error}"
            )));
            return Err(error);
        }

        let replacement = StagedBinaryReplacement::new(
            &install_dir,
            &staged_dir,
            ready_update.package_identity.clone(),
        );
        if let Err(error) = replacement.validate_staged_payload() {
            let _ = self.abort_update_handoff();
            self.inner.restore_ready_snapshot(Some(error.to_string()));
            return Err(error.to_string());
        }
        // Durable recoverable marker + backups before seal.
        if let Err(error) = replacement.prepare_durable_backups() {
            let _ = self.abort_update_handoff();
            self.inner.restore_ready_snapshot(Some(error.to_string()));
            return Err(error.to_string());
        }
        {
            let deadline = Instant::now() + UPDATE_IPC_DEADLINE;
            if let Ok(port) = self.inner.control_port.lock() {
                if let Some(port) = port.as_ref() {
                    if let Err(error) = port.seal_after_durable_stage(deadline) {
                        let _ = self.abort_update_handoff();
                        self.inner.restore_ready_snapshot(Some(error.clone()));
                        return Err(error);
                    }
                } else if let Err(error) = self.with_bound_gate(|gate| {
                    gate.seal_after_durable_stage()
                        .map_err(|error| error.to_string())
                }) {
                    let _ = self.abort_update_handoff();
                    self.inner.restore_ready_snapshot(Some(error.clone()));
                    return Err(error);
                }
            } else if let Err(error) = self.with_bound_gate(|gate| {
                gate.seal_after_durable_stage()
                    .map_err(|error| error.to_string())
            }) {
                let _ = self.abort_update_handoff();
                self.inner.restore_ready_snapshot(Some(error.clone()));
                return Err(error);
            }
        }

        // Persist recoverable handoff marker before irreversible binary commit /
        // old-process exit so the new host/client can finish Hello resync.
        let recovery = UpdateHandoffRecoveryMarker::from_token(
            &prepared_token,
            ready_update.package_identity.protocol_major,
            ready_update.package_identity.protocol_minor,
        );
        if let Err(error) = persist_update_handoff_recovery_marker(&install_dir, &recovery) {
            let _ = self.abort_update_handoff();
            self.inner.restore_ready_snapshot(Some(error.clone()));
            return Err(error);
        }

        let version = ready_update.update.version.clone();
        match replacement.commit_after_durable_backups() {
            Ok(_) => {
                let _ = self.inner.consume_ready_after_installer_launch();
                // Post-install host start/Hello is performed by the new process;
                // old process must exit and must not assume continued execution.
                Ok(InstallerLaunchOutcome {
                    version,
                    process_must_exit: true,
                    require_host_hello_resync: true,
                })
            }
            Err(error) => {
                let _ = replacement.recover_interrupted();
                let _ = self.abort_update_handoff();
                self.inner.restore_ready_snapshot(Some(format!(
                    "Staged replace failed; ready update retained when abortable: {error}"
                )));
                Err(error.to_string())
            }
        }
    }

    fn with_bound_gate<T>(
        &self,
        f: impl FnOnce(&HostUpdateRuntimeGate) -> Result<T, String>,
    ) -> Result<T, String> {
        let gate = self
            .inner
            .update_gate
            .lock()
            .map_err(|_| "Update gate lock is unavailable.".to_string())?;
        let gate = gate.as_ref().ok_or_else(|| {
            "Host update gate is not bound; bind HostRequestHandle / HostUpdateRuntimeGate first."
                .to_string()
        })?;
        f(gate)
    }

    pub fn install_update(&self) -> Result<String, String> {
        self.install_update_with_options(InstallUpdateOptions::default())
    }

    /// Prepare handoff then launch installer. Prefer explicit
    /// [`Self::prepare_update_install`] + resource shutdown +
    /// [`Self::launch_verified_installer`] on Windows so shutdown precedes launch.
    pub fn install_update_with_options(
        &self,
        options: InstallUpdateOptions,
    ) -> Result<String, String> {
        let token = self.prepare_update_install(options)?;
        self.launch_verified_installer(token.token_id)
            .map(|outcome| outcome.version)
    }

    pub fn snapshot(&self) -> UpdaterSnapshot {
        self.inner
            .state
            .read()
            .map(|state| state.snapshot.clone())
            .unwrap_or_else(|_| UpdaterSnapshot {
                configured: false,
                current_version: self.inner.current_version.to_string(),
                endpoints: Vec::new(),
                stage: UpdaterStage::Error,
                target_version: None,
                detail: "Updater state is unavailable.".to_string(),
                release_notes: None,
                last_checked_at: None,
                downloaded_bytes: 0,
                total_bytes: None,
            })
    }

    pub fn is_configured(&self) -> bool {
        self.snapshot().configured
    }

    pub fn start_background_checks(&self) {
        if !self.is_configured() {
            return;
        }
        if self
            .inner
            .background_checks_started
            .swap(true, AtomicOrdering::SeqCst)
        {
            return;
        }

        let updater = self.clone();
        thread::spawn(move || {
            let _ = updater.check_for_updates();
            loop {
                thread::sleep(updater.background_check_interval());
                let _ = updater.check_for_updates();
            }
        });
    }

    pub fn check_for_updates(&self) -> Result<(), String> {
        let config = self
            .inner
            .config
            .clone()
            .ok_or_else(|| self.snapshot().detail)?;

        let check_plan = self.inner.prepare_check()?;
        let inner = self.inner.clone();
        let current_version = self.inner.current_version.clone();
        thread::spawn(move || {
            let policy = CacheBustingRequestPolicy::for_instant(SystemTime::now());
            match check_update_with_policy(current_version, config.clone(), &policy) {
                Ok(Some(update)) => {
                    if let Err(error) =
                        arm_pending_identity_from_packager_config(&inner, &config, &policy, &update)
                    {
                        inner.finish_check_error(
                            check_plan,
                            format!(
                                "Version {} is available, but release identity could not be verified: {error}",
                                update.version
                            ),
                        );
                        return;
                    }
                    match inner.prepare_auto_download(&update) {
                        Ok(AutoDownloadAction::Start) => {
                            Self::spawn_download_thread(inner, update);
                        }
                        Ok(AutoDownloadAction::KeepReady) => {
                            inner.restore_ready_snapshot(None);
                        }
                        Err(error) => inner.finish_check_error(
                            check_plan,
                            format!(
                                "Version {} is available, but the background download could not start: {error}",
                                update.version
                            ),
                        ),
                    }
                }
                Ok(None) => inner.finish_check_without_update(),
                Err(error) => {
                    inner.finish_check_error(check_plan, format!("Update check failed: {error}"))
                }
            }
        });
        Ok(())
    }

    pub fn download_update(&self) -> Result<(), String> {
        let update = self.inner.prepare_download()?;
        Self::spawn_download_thread(self.inner.clone(), update);
        Ok(())
    }

    fn remember_prepared_token(&self, token: UpdateHandoffToken) -> Result<(), String> {
        let mut slot = self
            .inner
            .prepared_token
            .lock()
            .map_err(|_| "Prepared update token state is unavailable.".to_string())?;
        if let Some(existing) = slot.as_ref() {
            if existing.token_id != token.token_id {
                return Err("Another update handoff is already prepared.".into());
            }
        }
        *slot = Some(token);
        Ok(())
    }
}

impl UpdaterInner {
    fn take_prepared_token(&self, token_id: uuid::Uuid) -> Result<UpdateHandoffToken, String> {
        let mut slot = self
            .prepared_token
            .lock()
            .map_err(|_| "Prepared update token state is unavailable.".to_string())?;
        let token = slot
            .as_ref()
            .ok_or_else(|| "No prepared update handoff token is available.".to_string())?;
        if token.token_id != token_id {
            return Err(format!(
                "Prepared update handoff token mismatch: expected {}, observed {}.",
                token.token_id, token_id
            ));
        }
        slot.take()
            .ok_or_else(|| "Prepared update handoff token was consumed concurrently.".into())
    }

    fn clear_prepared_token(&self) {
        if let Ok(mut slot) = self.prepared_token.lock() {
            *slot = None;
        }
    }
}

/// Result of launching the packager installer (final old-process action).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallerLaunchOutcome {
    pub version: String,
    /// Callers must exit; do not assume reconnect runs in this process.
    pub process_must_exit: bool,
    /// New process must Hello the matching host and finish snapshot resync.
    pub require_host_hello_resync: bool,
}

impl Default for UpdaterService {
    fn default() -> Self {
        Self::new()
    }
}

impl UpdaterService {
    fn background_check_interval(&self) -> Duration {
        self.inner.background_check_interval()
    }

    fn spawn_download_thread(inner: Arc<UpdaterInner>, update: PackagerUpdate) {
        let version = update.version.clone();
        thread::spawn(move || {
            let progress_inner = inner.clone();
            match update.download_extended(
                move |chunk_size, total| {
                    progress_inner.record_download_progress(chunk_size as u64, total);
                },
                || {},
            ) {
                Ok(bytes) => inner.set_ready_to_install(update, bytes),
                Err(error) => inner.restore_ready_after_failed_download(format!(
                    "Download failed for {version}: {error}"
                )),
            }
        });
    }
}

impl UpdaterInner {
    fn background_check_interval(&self) -> Duration {
        self.state
            .read()
            .map(|state| {
                if state.ready_update.is_some() {
                    READY_UPDATE_RECHECK_INTERVAL
                } else {
                    BACKGROUND_UPDATE_INTERVAL
                }
            })
            .unwrap_or(BACKGROUND_UPDATE_INTERVAL)
    }

    fn prepare_check(&self) -> Result<CheckPlan, String> {
        let mut state = self
            .state
            .write()
            .map_err(|_| "Updater state is unavailable.".to_string())?;
        if state.snapshot.is_busy() {
            return Err("Updater is busy. Wait for the current action to finish.".to_string());
        }
        if !state.snapshot.configured {
            return Err(state.snapshot.detail.clone());
        }
        if state.ready_update.is_some() {
            state.snapshot.stage = UpdaterStage::Checking;
            state.snapshot.detail = format!(
                "Checking {} for something newer than the downloaded update...",
                summarize_endpoint(state.snapshot.endpoints.first())
            );
            return Ok(CheckPlan::PreserveReady);
        }
        state.pending_update = None;
        state.ready_update = None;
        clear_update_metadata(&mut state.snapshot);
        state.snapshot.stage = UpdaterStage::Checking;
        state.snapshot.detail = format!(
            "Checking {} for a newer release...",
            summarize_endpoint(state.snapshot.endpoints.first())
        );
        Ok(CheckPlan::Fresh)
    }

    fn finish_check_without_update(&self) {
        self.set_up_to_date();
    }

    fn finish_check_error(&self, plan: CheckPlan, message: String) {
        match plan {
            CheckPlan::Fresh => self.set_error(message),
            CheckPlan::PreserveReady => self.restore_ready_snapshot(None),
        }
    }

    fn set_up_to_date(&self) {
        if let Ok(mut state) = self.state.write() {
            state.pending_update = None;
            state.ready_update = None;
            state.snapshot.stage = UpdaterStage::UpToDate;
            state.snapshot.last_checked_at = Some(SystemTime::now());
            clear_update_metadata(&mut state.snapshot);
            state.snapshot.detail = format!(
                "DevManager {} is up to date.",
                state.snapshot.current_version
            );
        }
    }

    fn prepare_auto_download(&self, update: &PackagerUpdate) -> Result<AutoDownloadAction, String> {
        let mut state = self
            .state
            .write()
            .map_err(|_| "Updater state is unavailable.".to_string())?;
        if matches!(
            state.snapshot.stage,
            UpdaterStage::Downloading | UpdaterStage::Installing
        ) {
            return Err("Updater is busy. Wait for the current action to finish.".to_string());
        }
        if let Some(ready_update) = state.ready_update.as_ref() {
            match compare_versions(&update.version, &ready_update.update.version)? {
                Ordering::Equal => return Ok(AutoDownloadAction::KeepReady),
                Ordering::Greater | Ordering::Less => state.ready_update = None,
            }
        }
        state.pending_update = Some(update.clone());
        state.snapshot.stage = UpdaterStage::Downloading;
        state.snapshot.target_version = Some(update.version.clone());
        state.snapshot.release_notes = update.body.clone();
        state.snapshot.downloaded_bytes = 0;
        state.snapshot.total_bytes = None;
        state.snapshot.detail = format!(
            "Version {} is available. Downloading it in the background...",
            update.version
        );
        Ok(AutoDownloadAction::Start)
    }

    fn prepare_download(&self) -> Result<PackagerUpdate, String> {
        let mut state = self
            .state
            .write()
            .map_err(|_| "Updater state is unavailable.".to_string())?;
        if state.snapshot.is_busy() {
            return Err("Updater is busy. Wait for the current action to finish.".to_string());
        }
        let update = state.pending_update.clone().ok_or_else(|| {
            state
                .ready_update
                .as_ref()
                .map(|ready_update| {
                    format!(
                        "Version {} is already downloaded. Restart DevManager to install it.",
                        ready_update.update.version
                    )
                })
                .unwrap_or_else(|| "Check for an available update first.".to_string())
        })?;
        state.snapshot.stage = UpdaterStage::Downloading;
        state.snapshot.target_version = Some(update.version.clone());
        state.snapshot.release_notes = update.body.clone();
        state.snapshot.downloaded_bytes = 0;
        state.snapshot.total_bytes = None;
        state.snapshot.detail = format!("Downloading version {}...", update.version);
        Ok(update)
    }

    fn record_download_progress(&self, chunk_size: u64, total: Option<u64>) {
        if let Ok(mut state) = self.state.write() {
            state.snapshot.stage = UpdaterStage::Downloading;
            state.snapshot.downloaded_bytes =
                state.snapshot.downloaded_bytes.saturating_add(chunk_size);
            state.snapshot.total_bytes = total;
            state.snapshot.detail = if let Some(total) = total {
                format!(
                    "Downloaded {} of {}.",
                    human_bytes(state.snapshot.downloaded_bytes),
                    human_bytes(total)
                )
            } else {
                format!(
                    "Downloaded {}...",
                    human_bytes(state.snapshot.downloaded_bytes)
                )
            };
        }
    }

    fn set_ready_to_install(&self, update: PackagerUpdate, bytes: Vec<u8>) {
        if let Ok(mut state) = self.state.write() {
            let pending = match state.pending_release.clone() {
                Some(pending) => pending,
                None => {
                    drop(state);
                    self.set_error(UpdateRejection::MissingRequiredSha256.to_string());
                    return;
                }
            };
            let actual_hash =
                match verify_downloaded_artifact_sha256(&bytes, &pending.required_hash) {
                    Ok(hash) => hash,
                    Err(error) => {
                        drop(state);
                        self.set_error(error.to_string());
                        return;
                    }
                };
            // Update.target is OS-only; pending.packager_target is the OS-ARCH
            // manifest key used for release metadata selection.
            let os_target = packager_os_target();
            let arch_target = packager_architecture_target();
            if os_target.as_deref() != Some(update.target.as_str())
                || arch_target.as_deref() != Some(pending.packager_target.as_str())
                || pending.download_url != update.download_url.as_str()
                || pending.signature != update.signature
                || pending.format != update.format.to_string()
            {
                drop(state);
                self.set_error(
                    "Packager-selected artifact URL/signature/format/platform drifted from armed release identity"
                        .to_string(),
                );
                return;
            }
            let live_host = self
                .live_host_build
                .lock()
                .ok()
                .and_then(|guard| guard.clone());
            let host_build = match live_host {
                Some(live) if live == pending.host_build => live,
                Some(live) => {
                    drop(state);
                    self.set_error(format!(
                        "live Host Hello server_build `{live}` does not match release host_build `{}`",
                        pending.host_build
                    ));
                    return;
                }
                None => {
                    // Fail closed for production install identity: live Hello required.
                    // Unit download-path tests may arm without a live host by setting
                    // the same build into live_host_build first.
                    drop(state);
                    self.set_error(
                        "live Host Hello server_build is required before ReadyToInstall"
                            .to_string(),
                    );
                    return;
                }
            };
            if let Some((major, minor)) = self.live_protocol.lock().ok().and_then(|g| *g) {
                if major != pending.protocol_major || minor < pending.protocol_minor {
                    drop(state);
                    self.set_error(format!(
                        "live Host Hello protocol {major}.{minor} incompatible with release {}.{}",
                        pending.protocol_major, pending.protocol_minor
                    ));
                    return;
                }
            }
            let proof = handoff::VerifiedPackagerDownload::new(
                update.version.clone(),
                actual_hash,
                pending.packager_target.clone(),
                pending.download_url.clone(),
                pending.signature.clone(),
                pending.format.clone(),
            );
            let package_identity = match AtomicInstallerBundle::from_verified_download(
                proof,
                pending.protocol_major,
                pending.protocol_minor,
                pending.client_build.clone(),
                host_build,
            ) {
                Ok(identity) => identity,
                Err(error) => {
                    drop(state);
                    self.set_error(format!(
                        "Downloaded update failed atomic package identity checks: {error}"
                    ));
                    return;
                }
            };
            state.pending_update = None;
            state.ready_update = Some(DownloadedUpdate {
                update,
                bytes,
                package_identity,
                required_hash: pending.required_hash,
            });
            restore_ready_snapshot_locked(&mut state, None);
        }
    }

    /// Record evaluated release identity before download marks ReadyToInstall.
    pub(crate) fn arm_pending_release_identity(
        &self,
        admitted: &AdmittedUpdate,
    ) -> Result<(), String> {
        let hash = admitted
            .hash
            .clone()
            .ok_or_else(|| UpdateRejection::MissingRequiredSha256.to_string())?;
        validate_manifest_artifact_hash_field(&hash).map_err(|error| error.to_string())?;
        let (protocol_major, protocol_minor) =
            parse_minimum_protocol(admitted.minimum_protocol.as_deref())?;
        if admitted.platform.trim().is_empty()
            || admitted.url.trim().is_empty()
            || admitted.signature.trim().is_empty()
        {
            return Err("admitted release missing packager target/url/signature".into());
        }
        let format = admitted
            .format
            .clone()
            .ok_or_else(|| "admitted release missing packager format binding".to_string())?;
        let mut state = self
            .state
            .write()
            .map_err(|_| "Updater state is unavailable.".to_string())?;
        state.pending_release = Some(PendingReleaseIdentity {
            required_hash: hash,
            client_build: admitted.client_build.clone(),
            host_build: admitted.host_build.clone(),
            protocol_major,
            protocol_minor,
            packager_target: admitted.platform.clone(),
            download_url: admitted.url.clone(),
            signature: admitted.signature.clone(),
            format,
        });
        Ok(())
    }

    fn validate_ready_update(&self, ready: &DownloadedUpdate) -> Result<(), String> {
        if !ready.package_identity.signature_verified_by_packager {
            return Err(UpdateRejection::SignatureNotVerifiedByPackager.to_string());
        }
        verify_downloaded_artifact_sha256(&ready.bytes, &ready.required_hash)
            .map_err(|error| error.to_string())?;
        assert_atomic_installer_bundle(&ready.package_identity)
            .map_err(|error| error.to_string())?;
        Ok(())
    }

    fn mark_installing_snapshot(&self, ready: &DownloadedUpdate) -> Result<(), String> {
        let mut state = self
            .state
            .write()
            .map_err(|_| "Updater state is unavailable.".to_string())?;
        if state.snapshot.is_busy()
            && !matches!(
                state.snapshot.stage,
                UpdaterStage::ReadyToInstall | UpdaterStage::Installing
            )
        {
            return Err("Updater is busy. Wait for the current action to finish.".to_string());
        }
        if state.ready_update.is_none() {
            return Err("No downloaded update is ready to install.".to_string());
        }
        let version = ready.update.version.clone();
        let size = ready.bytes.len() as u64;
        state.snapshot.stage = UpdaterStage::Installing;
        state.snapshot.target_version = Some(version.clone());
        state.snapshot.release_notes = ready.update.body.clone();
        state.snapshot.downloaded_bytes = size;
        state.snapshot.total_bytes = Some(size);
        state.snapshot.detail = format!("Launching installer for version {version}...");
        Ok(())
    }

    fn consume_ready_after_installer_launch(&self) -> Result<(), String> {
        let mut state = self
            .state
            .write()
            .map_err(|_| "Updater state is unavailable.".to_string())?;
        state.ready_update = None;
        state.pending_update = None;
        state.pending_release = None;
        Ok(())
    }

    fn restore_ready_after_failed_download(&self, message: String) {
        if let Ok(mut state) = self.state.write() {
            state.pending_update = None;
            if state.ready_update.is_some() {
                restore_ready_snapshot_locked(&mut state, None);
            } else {
                state.snapshot.stage = UpdaterStage::Error;
                state.snapshot.last_checked_at = Some(SystemTime::now());
                clear_update_metadata(&mut state.snapshot);
                state.snapshot.detail = message;
            }
        }
    }

    fn restore_ready_snapshot(&self, detail_override: Option<String>) {
        if let Ok(mut state) = self.state.write() {
            if state.ready_update.is_some() {
                state.pending_update = None;
                restore_ready_snapshot_locked(&mut state, detail_override);
            }
        }
    }

    fn set_error(&self, message: String) {
        if let Ok(mut state) = self.state.write() {
            state.pending_update = None;
            state.ready_update = None;
            state.pending_release = None;
            state.snapshot.stage = UpdaterStage::Error;
            state.snapshot.last_checked_at = Some(SystemTime::now());
            clear_update_metadata(&mut state.snapshot);
            state.snapshot.detail = message;
        }
    }
}

pub fn resolve_embedded_config() -> Result<Option<ResolvedUpdaterConfig>, String> {
    resolve_updater_config(
        read_runtime_or_embedded(UPDATE_ENDPOINTS_VAR),
        read_runtime_or_embedded(UPDATE_PUBKEY_VAR),
        read_runtime_or_embedded(UPDATE_WINDOWS_INSTALL_MODE_VAR),
    )
}

pub fn resolve_updater_config(
    endpoints_value: Option<String>,
    pubkey_value: Option<String>,
    install_mode_value: Option<String>,
) -> Result<Option<ResolvedUpdaterConfig>, String> {
    let endpoints = split_config_list(endpoints_value);
    let pubkey = pubkey_value.unwrap_or_default().trim().to_string();

    if endpoints.is_empty() && pubkey.is_empty() {
        return Ok(None);
    }

    if endpoints.is_empty() || pubkey.is_empty() {
        return Err(format!(
            "{UPDATE_ENDPOINTS_VAR} and {UPDATE_PUBKEY_VAR} must both be set to enable updates."
        ));
    }

    let windows_install_mode = parse_windows_install_mode(install_mode_value.as_deref())?;

    Ok(Some(ResolvedUpdaterConfig {
        endpoints,
        pubkey,
        windows_install_mode,
    }))
}

pub fn parse_release_manifest(json: &str) -> Result<ReleaseManifest, serde_json::Error> {
    serde_json::from_str(json)
}

pub fn is_remote_version_newer(
    current_version: &str,
    remote_version: &str,
) -> Result<bool, String> {
    let current = parse_version(current_version)?;
    let remote = parse_version(remote_version)?;
    Ok(remote > current)
}

/// Single semantic-version parser for installed and remote release identities.
pub fn parse_semver(value: &str) -> Result<Version, String> {
    parse_version(value)
}

pub fn next_patch_release_version(
    latest_release: Option<&str>,
    cargo_version: &str,
) -> Result<String, String> {
    let mut version = parse_version(latest_release.unwrap_or(cargo_version))?;
    version.patch = version.patch.saturating_add(1);
    version.pre = Prerelease::EMPTY;
    version.build = BuildMetadata::EMPTY;
    Ok(version.to_string())
}

pub fn github_release_manifest_endpoint(repository: &str) -> String {
    let repository = repository.trim().trim_matches('/');
    format!("https://github.com/{repository}/releases/latest/download/latest.json")
}

/// Resolve the running installed package identity from binary/package metadata.
pub fn resolve_running_package_identity() -> InstalledPackageIdentity {
    if let Some(version) = read_current_exe_product_version() {
        return InstalledPackageIdentity {
            client_build: format!("devmanager/{version}"),
            host_build: format!("devmanager-host/{version}"),
            version,
            source: PackageVersionSource::BinaryMetadata,
        };
    }
    let version =
        parse_version(env!("CARGO_PKG_VERSION")).unwrap_or_else(|_| Version::new(0, 0, 0));
    InstalledPackageIdentity {
        client_build: format!("devmanager/{version}"),
        host_build: format!("devmanager-host/{version}"),
        version,
        source: PackageVersionSource::EmbeddedPackageMetadata,
    }
}

/// Build an installed identity for tests and deterministic fixtures.
pub fn package_identity_for_version(
    version: &str,
    source: PackageVersionSource,
) -> Result<InstalledPackageIdentity, String> {
    let version = parse_version(version)?;
    Ok(InstalledPackageIdentity {
        client_build: format!("devmanager/{version}"),
        host_build: format!("devmanager-host/{version}"),
        version,
        source,
    })
}

/// Evaluate a signed release manifest against the installed package identity.
pub fn evaluate_release_candidate(
    current: &InstalledPackageIdentity,
    manifest: &ReleaseManifest,
    platform: &str,
) -> Result<AdmittedUpdate, UpdateRejection> {
    let remote = parse_version(&manifest.version)
        .map_err(|detail| UpdateRejection::InvalidRemoteVersion { detail })?;

    match remote.cmp(&current.version) {
        Ordering::Less => {
            return Err(UpdateRejection::Downgrade {
                current: current.version.to_string(),
                remote: remote.to_string(),
            })
        }
        Ordering::Equal => {
            return Err(UpdateRejection::MatchingVersion {
                version: remote.to_string(),
            })
        }
        Ordering::Greater => {}
    }

    let minimum_protocol = manifest.minimum_protocol.clone().ok_or_else(|| {
        UpdateRejection::MalformedManifestField {
            detail: "minimum_protocol is required".into(),
        }
    })?;
    let (required_major, required_minor) = parse_minimum_protocol(Some(&minimum_protocol))
        .map_err(|detail| UpdateRejection::MalformedManifestField { detail })?;
    let local = crate::protocol::ProtocolVersion::current();
    if required_major != local.major || required_minor > local.minor {
        return Err(UpdateRejection::ProtocolIncompatible {
            required: minimum_protocol,
            local: format!("{}.{}", local.major, local.minor),
        });
    }

    let platform_entry =
        manifest
            .platforms
            .get(platform)
            .ok_or_else(|| UpdateRejection::MissingPlatform {
                platform: platform.to_string(),
            })?;

    validate_manifest_signature_field(&platform_entry.signature)?;
    if platform_entry.format.trim().is_empty() {
        return Err(UpdateRejection::MalformedManifestField {
            detail: "format is required".into(),
        });
    }
    let hash = platform_entry
        .hash
        .clone()
        .ok_or(UpdateRejection::MissingRequiredSha256)?;
    validate_manifest_artifact_hash_field(&hash)?;

    let client_build = platform_entry
        .client_build
        .clone()
        .ok_or(UpdateRejection::MissingBuildIdentity)?;
    let host_build = platform_entry
        .host_build
        .clone()
        .ok_or(UpdateRejection::MissingBuildIdentity)?;

    let client_version =
        extract_build_version(&client_build).and_then(|value| parse_version(value).ok());
    let host_version =
        extract_build_version(&host_build).and_then(|value| parse_version(value).ok());
    match (client_version, host_version) {
        (Some(client), Some(host)) if client == host && client == remote => {}
        _ => {
            return Err(UpdateRejection::HostClientMismatch {
                client_build,
                host_build,
            })
        }
    }

    Ok(AdmittedUpdate {
        version: remote,
        notes: manifest.notes.clone(),
        platform: platform.to_string(),
        url: platform_entry.url.clone(),
        signature: platform_entry.signature.clone(),
        format: Some(platform_entry.format.clone()),
        hash: Some(hash),
        minimum_protocol: Some(minimum_protocol),
        client_build,
        host_build,
    })
}

fn parse_minimum_protocol(value: Option<&str>) -> Result<(u16, u16), String> {
    let raw = value.ok_or_else(|| "minimum_protocol is required".to_string())?;
    let mut parts = raw.trim().split('.');
    let major = parts
        .next()
        .ok_or_else(|| format!("invalid minimum_protocol `{raw}`"))?
        .parse::<u16>()
        .map_err(|_| format!("invalid minimum_protocol major in `{raw}`"))?;
    let minor = parts
        .next()
        .unwrap_or("0")
        .parse::<u16>()
        .map_err(|_| format!("invalid minimum_protocol minor in `{raw}`"))?;
    if parts.next().is_some() {
        return Err(format!("invalid minimum_protocol `{raw}`"));
    }
    Ok((major, minor))
}

/// Reject indefinitely stale local metadata when a fresher signed body is available.
pub fn prefer_signed_manifest_over_stale_cache(
    cached: &ReleaseManifest,
    signed_fresh: &ReleaseManifest,
) -> Result<ReleaseManifest, UpdateRejection> {
    let cached_version = parse_version(&cached.version)
        .map_err(|detail| UpdateRejection::InvalidRemoteVersion { detail })?;
    let fresh_version = parse_version(&signed_fresh.version)
        .map_err(|detail| UpdateRejection::InvalidRemoteVersion { detail })?;
    if fresh_version > cached_version {
        return Ok(signed_fresh.clone());
    }
    if fresh_version == cached_version {
        return Ok(signed_fresh.clone());
    }
    Err(UpdateRejection::StaleCachedMetadata {
        detail: format!(
            "cached {} is newer than signed fresh {}; refusing to honor stale local cache over signed content",
            cached_version, fresh_version
        ),
    })
}

/// Prefilter only: missing/malformed signature *field* shape.
///
/// This is not cryptographic verification. Packager download verification is
/// authoritative for signature success.
pub fn validate_manifest_signature_field(signature: &str) -> Result<(), UpdateRejection> {
    let trimmed = signature.trim();
    if trimmed.is_empty() {
        return Err(UpdateRejection::MalformedManifestField {
            detail: "signature field is empty".into(),
        });
    }
    if trimmed.len() < 16 {
        return Err(UpdateRejection::MalformedManifestField {
            detail: "signature field is too short to be a packager .sig payload".into(),
        });
    }
    if !trimmed.bytes().all(|byte| {
        byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'/' | b'=' | b'-' | b'_')
    }) {
        return Err(UpdateRejection::MalformedManifestField {
            detail: "signature field contains invalid characters".into(),
        });
    }
    Ok(())
}

pub fn validate_manifest_artifact_hash_field(hash: &str) -> Result<(), UpdateRejection> {
    let trimmed = hash.trim();
    let Some(hex) = trimmed.strip_prefix("sha256:") else {
        return Err(UpdateRejection::MalformedManifestField {
            detail: format!("artifact hash must use sha256: prefix, got `{trimmed}`"),
        });
    };
    if hex.len() != 64 || !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(UpdateRejection::MalformedManifestField {
            detail: "artifact sha256 digest must be 64 hex characters".into(),
        });
    }
    Ok(())
}

/// Apply cache-busting query + headers to the packager updater config used for HTTP checks.
pub fn apply_cache_busting_to_packager_config(
    mut config: PackagerUpdaterConfig,
    policy: &CacheBustingRequestPolicy,
) -> Result<PackagerUpdaterConfig, String> {
    let busted_endpoints = config
        .endpoints
        .iter()
        .map(|endpoint| {
            let busted = policy.apply_to_endpoint(endpoint.as_str())?;
            Url::parse(&busted).map_err(|error| {
                format!("Failed to parse cache-busted updater endpoint `{busted}`: {error}")
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    config.endpoints = busted_endpoints;
    Ok(config)
}

fn check_update_with_policy(
    current_version: Version,
    config: PackagerUpdaterConfig,
    policy: &CacheBustingRequestPolicy,
) -> Result<Option<PackagerUpdate>, String> {
    let config = apply_cache_busting_to_packager_config(config, policy)?;

    let mut builder = UpdaterBuilder::new(current_version, config);
    for (key, value) in policy.header_pairs() {
        builder = builder
            .header(key, value)
            .map_err(|error| format!("Failed to apply updater cache-busting header: {error}"))?;
    }
    let updater = builder
        .build()
        .map_err(|error| format!("Failed to build updater: {error}"))?;
    updater
        .check()
        .map_err(|error| format!("Update check failed: {error}"))
}

fn arm_pending_identity_from_packager_config(
    inner: &UpdaterInner,
    config: &PackagerUpdaterConfig,
    policy: &CacheBustingRequestPolicy,
    update: &PackagerUpdate,
) -> Result<(), String> {
    let endpoint = config
        .endpoints
        .first()
        .ok_or_else(|| "updater config has no endpoints".to_string())?;
    let busted = policy.apply_to_endpoint(endpoint.as_str())?;
    let mut request = ureq::get(&busted);
    for (key, value) in policy.header_pairs() {
        request = request.header(key, value);
    }
    let body = request
        .call()
        .map_err(|error| format!("failed to fetch release manifest: {error}"))?
        .into_body()
        .read_to_string()
        .map_err(|error| format!("failed to read release manifest body: {error}"))?;
    let manifest = parse_release_manifest(&body)
        .map_err(|error| format!("failed to parse release manifest: {error}"))?;
    if manifest.version != update.version {
        return Err(format!(
            "packager update version {} does not match manifest version {}",
            update.version, manifest.version
        ));
    }
    let current = resolve_running_package_identity();
    // cargo-packager's `Update.target` is the OS selector used to choose the
    // updater format (for example `windows`).  The signed manifest, however,
    // is indexed by the full OS-ARCH key (for example `windows-x86_64`).
    // Keep those two identities separate: comparing Update.target with the
    // manifest key makes every Windows release fail closed before download.
    let platform = packager_architecture_target()
        .ok_or_else(|| "unable to derive cargo_packager_updater target key".to_string())?;
    let os_target = packager_os_target()
        .ok_or_else(|| "unable to derive cargo_packager_updater OS target".to_string())?;
    if update.target != os_target {
        return Err(format!(
            "packager update target `{}` does not match runtime OS `{os_target}`",
            update.target
        ));
    }
    let admitted = evaluate_release_candidate(&current, &manifest, &platform)
        .map_err(|error| error.to_string())?;
    if admitted.url != update.download_url.as_str()
        || admitted.signature != update.signature
        || admitted.format.as_deref() != Some(update.format.to_string().as_str())
    {
        return Err(
            "packager-selected artifact URL/signature/format does not match release manifest"
                .into(),
        );
    }
    inner.arm_pending_release_identity(&admitted)
}

/// Exact packager `OS-ARCH` key (`cargo_packager_updater::target()`).
pub fn packager_architecture_target() -> Option<String> {
    cargo_packager_updater::target()
}

/// The `cargo_packager_updater::Update.target` value used for request/format
/// selection.  This intentionally omits the architecture; the remote release
/// manifest is still keyed by [`packager_architecture_target`].
pub fn packager_os_target() -> Option<String> {
    if cfg!(target_os = "windows") {
        Some("windows".to_string())
    } else if cfg!(target_os = "macos") {
        Some("macos".to_string())
    } else if cfg!(target_os = "linux") {
        Some("linux".to_string())
    } else {
        None
    }
}

/// Materialize both product binaries into `staged_dir` from verified download bytes.
///
/// Accepts an uncompressed ZIP containing `devmanager.exe` + `devmanager-host.exe`,
/// or the compact `DMUP1` dual-exe container used by production-shaped fixtures.
/// NSIS/WiX installer execution is not performed here (see external packaging note).
pub fn materialize_staged_binaries(
    staged_dir: &Path,
    bytes: &[u8],
    identity: &AtomicInstallerBundle,
) -> Result<(), String> {
    assert_atomic_installer_bundle(identity).map_err(|error| error.to_string())?;
    if staged_dir.exists() {
        std::fs::remove_dir_all(staged_dir)
            .map_err(|error| format!("failed to clear stage dir: {error}"))?;
    }
    std::fs::create_dir_all(staged_dir)
        .map_err(|error| format!("failed to create stage dir: {error}"))?;

    if bytes.starts_with(b"DMUP1") {
        materialize_dmup1_container(staged_dir, bytes)?;
    } else if bytes.starts_with(b"PK\x03\x04") {
        materialize_uncompressed_zip(staged_dir, bytes)?;
    } else {
        return Err(
            "verified update bytes are not a dual-exe ZIP/DMUP1 payload; NSIS/WiX execution is not the in-process Windows replace path"
                .into(),
        );
    }
    inspect_atomic_installer_payload_dir(staged_dir, identity).map_err(|error| error.to_string())
}

fn materialize_dmup1_container(staged_dir: &Path, bytes: &[u8]) -> Result<(), String> {
    if bytes.len() < 13 || &bytes[..5] != b"DMUP1" {
        return Err("invalid DMUP1 container header".into());
    }
    let client_len = u32::from_le_bytes(bytes[5..9].try_into().unwrap()) as usize;
    let host_len = u32::from_le_bytes(bytes[9..13].try_into().unwrap()) as usize;
    let client_end = 13usize.checked_add(client_len).ok_or("DMUP1 overflow")?;
    let host_end = client_end.checked_add(host_len).ok_or("DMUP1 overflow")?;
    if host_end != bytes.len() {
        return Err("DMUP1 container length mismatch".into());
    }
    std::fs::write(staged_dir.join("devmanager.exe"), &bytes[13..client_end])
        .map_err(|error| format!("write client: {error}"))?;
    std::fs::write(
        staged_dir.join("devmanager-host.exe"),
        &bytes[client_end..host_end],
    )
    .map_err(|error| format!("write host: {error}"))?;
    Ok(())
}

fn materialize_uncompressed_zip(staged_dir: &Path, bytes: &[u8]) -> Result<(), String> {
    // Minimal local-file ZIP reader for store-only (method 0) entries.
    let mut offset = 0usize;
    let mut found_client = false;
    let mut found_host = false;
    while offset + 30 <= bytes.len() {
        if &bytes[offset..offset + 4] != b"PK\x03\x04" {
            break;
        }
        let method = u16::from_le_bytes(bytes[offset + 8..offset + 10].try_into().unwrap());
        let comp_size =
            u32::from_le_bytes(bytes[offset + 18..offset + 22].try_into().unwrap()) as usize;
        let name_len =
            u16::from_le_bytes(bytes[offset + 26..offset + 28].try_into().unwrap()) as usize;
        let extra_len =
            u16::from_le_bytes(bytes[offset + 28..offset + 30].try_into().unwrap()) as usize;
        let name_start = offset + 30;
        let name_end = name_start
            .checked_add(name_len)
            .ok_or("zip name overflow")?;
        let data_start = name_end
            .checked_add(extra_len)
            .ok_or("zip extra overflow")?;
        let data_end = data_start
            .checked_add(comp_size)
            .ok_or("zip data overflow")?;
        if data_end > bytes.len() {
            return Err("zip entry truncated".into());
        }
        let name = std::str::from_utf8(&bytes[name_start..name_end])
            .map_err(|_| "zip entry name is not utf8".to_string())?;
        let file_name = name.rsplit('/').next().unwrap_or(name);
        if method != 0 {
            return Err(format!(
                "zip entry `{file_name}` is compressed; only store-method dual-exe zips are supported in-process"
            ));
        }
        if file_name == "devmanager.exe" {
            std::fs::write(staged_dir.join(file_name), &bytes[data_start..data_end])
                .map_err(|error| format!("write {file_name}: {error}"))?;
            found_client = true;
        } else if file_name == "devmanager-host.exe" {
            std::fs::write(staged_dir.join(file_name), &bytes[data_start..data_end])
                .map_err(|error| format!("write {file_name}: {error}"))?;
            found_host = true;
        }
        offset = data_end;
    }
    if !found_client || !found_host {
        return Err("zip payload missing devmanager.exe and/or devmanager-host.exe".into());
    }
    Ok(())
}

fn read_current_exe_product_version() -> Option<Version> {
    let exe = std::env::current_exe().ok()?;
    read_binary_product_version(&exe)
}

fn read_binary_product_version(path: &Path) -> Option<Version> {
    read_binary_product_version_string(path).and_then(|value| parse_version(&value).ok())
}

pub(crate) fn read_binary_product_version_string(path: &Path) -> Option<String> {
    #[cfg(windows)]
    {
        read_windows_product_version_string(path)
    }
    #[cfg(not(windows))]
    {
        let _ = path;
        None
    }
}

#[cfg(windows)]
fn read_windows_product_version_string(path: &Path) -> Option<String> {
    use std::os::windows::ffi::OsStrExt;
    use windows::core::PCWSTR;
    use windows::Win32::Storage::FileSystem::{
        GetFileVersionInfoSizeW, GetFileVersionInfoW, VerQueryValueW, VS_FIXEDFILEINFO,
    };

    let wide: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    unsafe {
        let mut handle = 0u32;
        let size = GetFileVersionInfoSizeW(PCWSTR(wide.as_ptr()), Some(&mut handle));
        if size == 0 {
            return None;
        }
        let mut buffer = vec![0u8; size as usize];
        if GetFileVersionInfoW(
            PCWSTR(wide.as_ptr()),
            None,
            size,
            buffer.as_mut_ptr() as *mut _,
        )
        .is_err()
        {
            return None;
        }
        let mut length = 0u32;
        let mut value: *mut std::ffi::c_void = std::ptr::null_mut();
        let sub_block: Vec<u16> = "\\".encode_utf16().chain(std::iter::once(0)).collect();
        if !VerQueryValueW(
            buffer.as_ptr() as *const _,
            PCWSTR(sub_block.as_ptr()),
            &mut value,
            &mut length,
        )
        .as_bool()
            || value.is_null()
            || (length as usize) < std::mem::size_of::<VS_FIXEDFILEINFO>()
        {
            return None;
        }
        let info = &*(value as *const VS_FIXEDFILEINFO);
        let major = u64::from(info.dwProductVersionMS >> 16);
        let minor = u64::from(info.dwProductVersionMS & 0xffff);
        let patch = u64::from(info.dwProductVersionLS >> 16);
        Some(format!("{major}.{minor}.{patch}"))
    }
}

fn clear_update_metadata(snapshot: &mut UpdaterSnapshot) {
    snapshot.target_version = None;
    snapshot.release_notes = None;
    snapshot.downloaded_bytes = 0;
    snapshot.total_bytes = None;
}

fn restore_ready_snapshot_locked(state: &mut UpdaterState, detail_override: Option<String>) {
    let Some(ready_update) = state.ready_update.as_ref() else {
        clear_update_metadata(&mut state.snapshot);
        return;
    };
    let size = ready_update.bytes.len() as u64;
    state.snapshot.stage = UpdaterStage::ReadyToInstall;
    state.snapshot.target_version = Some(ready_update.update.version.clone());
    state.snapshot.release_notes = ready_update.update.body.clone();
    state.snapshot.last_checked_at = Some(SystemTime::now());
    state.snapshot.downloaded_bytes = size;
    state.snapshot.total_bytes = Some(size);
    state.snapshot.detail = detail_override.unwrap_or_else(|| {
        format!(
            "Version {} is downloaded. Restart DevManager to install it.",
            ready_update.update.version
        )
    });
}

fn parse_windows_install_mode(value: Option<&str>) -> Result<UpdaterWindowsInstallMode, String> {
    match value.unwrap_or("passive").trim() {
        "" | "passive" | "Passive" => Ok(UpdaterWindowsInstallMode::Passive),
        "basic-ui" | "basic_ui" | "basicUi" | "BasicUi" => Ok(UpdaterWindowsInstallMode::BasicUi),
        "quiet" | "Quiet" => Ok(UpdaterWindowsInstallMode::Quiet),
        other => Err(format!(
            "Unsupported Windows install mode `{other}`. Use `passive`, `quiet`, or `basicUi`."
        )),
    }
}

fn split_config_list(value: Option<String>) -> Vec<String> {
    value
        .unwrap_or_default()
        .split(|character| matches!(character, ',' | ';' | '\n' | '\r'))
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

fn compare_versions(left: &str, right: &str) -> Result<Ordering, String> {
    let left = parse_version(left)?;
    let right = parse_version(right)?;
    Ok(left.cmp(&right))
}

fn parse_version(value: &str) -> Result<Version, String> {
    for candidate in split_version_tokens(value) {
        let candidate = candidate
            .trim_matches(|character| {
                matches!(
                    character,
                    '\'' | '"' | '(' | ')' | '[' | ']' | '{' | '}' | ',' | ';' | ':'
                )
            })
            .trim_start_matches(|character| matches!(character, 'v' | 'V'));
        if candidate.is_empty() {
            continue;
        }
        if let Ok(version) = Version::parse(candidate) {
            return Ok(version);
        }
    }

    Err(format!(
        "Invalid version `{value}`: no valid semantic version found"
    ))
}

fn split_version_tokens(value: &str) -> Vec<&str> {
    let mut result = Vec::new();
    let mut start = None;

    for (index, character) in value.char_indices() {
        let is_token_char =
            character.is_ascii_alphanumeric() || matches!(character, '.' | '-' | '+');
        if is_token_char {
            if start.is_none() && (character.is_ascii_digit() || matches!(character, 'v' | 'V')) {
                start = Some(index);
            }
        } else if let Some(token_start) = start.take() {
            if token_start < index {
                result.push(&value[token_start..index]);
            }
        }
    }

    if let Some(token_start) = start {
        result.push(&value[token_start..]);
    }

    result
}

fn read_runtime_or_embedded(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| {
            let embedded = match name {
                UPDATE_ENDPOINTS_VAR => option_env!("DEVMANAGER_UPDATE_ENDPOINTS"),
                UPDATE_PUBKEY_VAR => option_env!("DEVMANAGER_UPDATE_PUBKEY"),
                UPDATE_WINDOWS_INSTALL_MODE_VAR => {
                    option_env!("DEVMANAGER_UPDATE_WINDOWS_INSTALL_MODE")
                }
                _ => None,
            }?;
            let trimmed = embedded.trim();
            (!trimmed.is_empty()).then(|| trimmed.to_string())
        })
}

fn summarize_endpoint(endpoint: Option<&String>) -> String {
    endpoint
        .map(|value| value.as_str())
        .unwrap_or("the configured update endpoint")
        .to_string()
}

fn human_bytes(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = KB * 1024.0;
    const GB: f64 = MB * 1024.0;

    let value = bytes as f64;
    if value >= GB {
        format!("{:.1} GB", value / GB)
    } else if value >= MB {
        format!("{:.1} MB", value / MB)
    } else if value >= KB {
        format!("{:.1} KB", value / KB)
    } else {
        format!("{bytes} B")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cargo_packager_updater::UpdateFormat;
    use std::path::PathBuf;
    use std::time::{Duration, UNIX_EPOCH};

    fn test_update(version: &str, body: Option<&str>) -> PackagerUpdate {
        PackagerUpdate {
            config: PackagerUpdaterConfig {
                endpoints: vec![Url::parse("https://example.com/latest.json").unwrap()],
                pubkey: "public-key".to_string(),
                windows: None,
            },
            body: body.map(ToOwned::to_owned),
            current_version: "0.2.0".to_string(),
            version: version.to_string(),
            date: None,
            target: "windows".to_string(),
            extract_path: PathBuf::from("."),
            download_url: Url::parse("https://example.com/devmanager.exe").unwrap(),
            signature: "signature".to_string(),
            timeout: None,
            headers: Default::default(),
            format: UpdateFormat::Nsis,
        }
    }

    fn test_inner() -> UpdaterInner {
        UpdaterInner {
            current_version: Version::new(0, 2, 0),
            config: None,
            background_checks_started: AtomicBool::new(false),
            state: RwLock::new(UpdaterState {
                snapshot: UpdaterSnapshot {
                    configured: true,
                    current_version: "0.2.0".to_string(),
                    endpoints: vec!["https://example.com/latest.json".to_string()],
                    stage: UpdaterStage::Idle,
                    target_version: None,
                    detail: "Ready to check https://example.com/latest.json for updates."
                        .to_string(),
                    release_notes: None,
                    last_checked_at: None,
                    downloaded_bytes: 0,
                    total_bytes: None,
                },
                pending_update: None,
                ready_update: None,
                pending_release: None,
            }),
            update_gate: Mutex::new(None),
            control_port: Mutex::new(None),
            resource_probe: Mutex::new(None),
            live_host_build: Mutex::new(None),
            live_protocol: Mutex::new(None),
        }
    }

    fn arm_ready(inner: &UpdaterInner, update: &PackagerUpdate, bytes: &[u8]) {
        use sha2::{Digest, Sha256};
        let hash = format!("sha256:{:x}", Sha256::digest(bytes));
        let admitted = AdmittedUpdate {
            version: parse_version(&update.version).unwrap(),
            notes: update.body.clone(),
            platform: packager_architecture_target()
                .unwrap_or_else(|| "windows-x86_64".to_string()),
            url: update.download_url.to_string(),
            signature: update.signature.clone(),
            format: Some(update.format.to_string()),
            hash: Some(hash),
            minimum_protocol: Some("1.0".into()),
            client_build: format!("devmanager/{}", update.version),
            host_build: format!("devmanager-host/{}", update.version),
        };
        *inner.live_host_build.lock().unwrap() = Some(admitted.host_build.clone());
        *inner.live_protocol.lock().unwrap() = Some((1, 0));
        inner.arm_pending_release_identity(&admitted).unwrap();
    }

    #[test]
    fn parse_version_accepts_plain_semver() {
        assert_eq!(
            parse_version("0.4.2").expect("parse"),
            Version::new(0, 4, 2)
        );
    }

    #[test]
    fn parse_version_accepts_prefixed_semver() {
        assert_eq!(
            parse_version("v0.4.2").expect("parse"),
            Version::new(0, 4, 2)
        );
    }

    #[test]
    fn parse_version_extracts_version_from_noisy_text() {
        assert_eq!(
            parse_version("Release v0.4.2 is now available").expect("parse"),
            Version::new(0, 4, 2)
        );
        assert_eq!(
            parse_version("tag=v0.4.2").expect("parse"),
            Version::new(0, 4, 2)
        );
        assert_eq!(
            parse_version("0.4.2 (beta)").expect("parse"),
            Version::new(0, 4, 2)
        );
    }

    #[test]
    fn parse_version_rejects_invalid_input() {
        assert!(parse_version("not a version").is_err());
    }

    #[test]
    fn newer_release_supersedes_downloaded_ready_update() {
        let inner = test_inner();
        let ready_update = test_update("0.2.1", Some("old release"));
        let newer_update = test_update("0.2.2", Some("new release"));

        arm_ready(&inner, &ready_update, &[1, 2, 3]);
        inner.set_ready_to_install(ready_update.clone(), vec![1, 2, 3]);
        assert_eq!(inner.prepare_check().unwrap(), CheckPlan::PreserveReady);
        {
            let state = inner.state.read().unwrap();
            assert_eq!(state.snapshot.stage, UpdaterStage::Checking);
            assert_eq!(state.snapshot.target_version.as_deref(), Some("0.2.1"));
        }
        assert_eq!(
            inner.prepare_auto_download(&newer_update).unwrap(),
            AutoDownloadAction::Start
        );

        let state = inner.state.read().unwrap();
        assert_eq!(state.snapshot.stage, UpdaterStage::Downloading);
        assert_eq!(state.snapshot.target_version.as_deref(), Some("0.2.2"));
        assert_eq!(
            state
                .ready_update
                .as_ref()
                .map(|update| update.update.version.as_str()),
            None
        );
    }

    #[test]
    fn ready_update_uses_faster_background_recheck_interval() {
        let inner = test_inner();
        assert_eq!(
            inner.background_check_interval(),
            BACKGROUND_UPDATE_INTERVAL
        );

        arm_ready(&inner, &test_update("0.2.1", None), &[1, 2, 3]);
        inner.set_ready_to_install(test_update("0.2.1", None), vec![1, 2, 3]);

        assert_eq!(
            inner.background_check_interval(),
            READY_UPDATE_RECHECK_INTERVAL
        );
    }

    #[test]
    fn failed_authoritative_replacement_cannot_restore_superseded_ready_update() {
        let inner = test_inner();
        let ready_update = test_update("0.2.1", Some("old release"));
        let replacement = test_update("0.2.2", Some("new release"));

        arm_ready(&inner, &ready_update, &[1, 2, 3]);
        inner.set_ready_to_install(ready_update, vec![1, 2, 3]);
        assert_eq!(
            inner.prepare_auto_download(&replacement).unwrap(),
            AutoDownloadAction::Start
        );
        inner.restore_ready_after_failed_download("Download failed".to_string());

        let state = inner.state.read().unwrap();
        assert_eq!(state.snapshot.stage, UpdaterStage::Error);
        assert!(state.snapshot.target_version.is_none());
        assert!(state.snapshot.release_notes.is_none());
        assert_eq!(state.snapshot.downloaded_bytes, 0);
        assert!(state.ready_update.is_none());
    }

    #[test]
    fn authoritative_no_update_discards_ready_update_while_check_error_preserves_it() {
        let error_inner = test_inner();
        arm_ready(
            &error_inner,
            &test_update("0.2.1", Some("recalled release")),
            &[1, 2, 3],
        );
        error_inner.set_ready_to_install(
            test_update("0.2.1", Some("recalled release")),
            vec![1, 2, 3],
        );
        let error_plan = error_inner.prepare_check().unwrap();
        error_inner.finish_check_error(error_plan, "Update check failed".to_string());

        {
            let state = error_inner.state.read().unwrap();
            assert_eq!(state.snapshot.stage, UpdaterStage::ReadyToInstall);
            assert_eq!(state.snapshot.target_version.as_deref(), Some("0.2.1"));
            assert_eq!(state.snapshot.downloaded_bytes, 3);
            assert!(state.ready_update.is_some());
        }

        let no_update_inner = test_inner();
        arm_ready(
            &no_update_inner,
            &test_update("0.2.1", Some("recalled release")),
            &[1, 2, 3],
        );
        no_update_inner.set_ready_to_install(
            test_update("0.2.1", Some("recalled release")),
            vec![1, 2, 3],
        );
        no_update_inner.prepare_check().unwrap();
        no_update_inner.finish_check_without_update();

        let state = no_update_inner.state.read().unwrap();
        assert_eq!(state.snapshot.stage, UpdaterStage::UpToDate);
        assert!(state.snapshot.target_version.is_none());
        assert!(state.snapshot.release_notes.is_none());
        assert_eq!(state.snapshot.downloaded_bytes, 0);
        assert!(state.snapshot.total_bytes.is_none());
        assert!(state.ready_update.is_none());
    }

    #[test]
    fn authoritative_lower_release_discards_recalled_ready_update_before_downloading() {
        let inner = test_inner();
        arm_ready(
            &inner,
            &test_update("0.3.0", Some("recalled release")),
            &[1, 2, 3],
        );
        inner.set_ready_to_install(
            test_update("0.3.0", Some("recalled release")),
            vec![1, 2, 3],
        );
        assert_eq!(inner.prepare_check().unwrap(), CheckPlan::PreserveReady);

        let authoritative_update = test_update("0.2.1", Some("fallback release"));
        assert_eq!(
            inner.prepare_auto_download(&authoritative_update).unwrap(),
            AutoDownloadAction::Start
        );

        {
            let state = inner.state.read().unwrap();
            assert_eq!(state.snapshot.stage, UpdaterStage::Downloading);
            assert_eq!(state.snapshot.target_version.as_deref(), Some("0.2.1"));
            assert_eq!(state.snapshot.downloaded_bytes, 0);
            assert!(state.ready_update.is_none());
        }

        inner.restore_ready_after_failed_download("Download failed".to_string());

        let state = inner.state.read().unwrap();
        assert_eq!(state.snapshot.stage, UpdaterStage::Error);
        assert!(state.ready_update.is_none());
        assert!(state.snapshot.target_version.is_none());
        assert_eq!(state.snapshot.downloaded_bytes, 0);
    }

    #[test]
    fn set_error_clears_stale_update_metadata() {
        let inner = test_inner();
        let update = test_update("0.2.1", Some("release notes"));

        inner.prepare_auto_download(&update).unwrap();
        inner.set_error("boom".to_string());

        let state = inner.state.read().unwrap();
        assert_eq!(state.snapshot.stage, UpdaterStage::Error);
        assert!(state.pending_update.is_none());
        assert!(state.ready_update.is_none());
        assert!(state.snapshot.target_version.is_none());
        assert!(state.snapshot.release_notes.is_none());
        assert_eq!(state.snapshot.downloaded_bytes, 0);
        assert!(state.snapshot.total_bytes.is_none());
    }

    #[test]
    fn evaluate_release_candidate_accepts_newer_matching_pair() {
        let current =
            package_identity_for_version("0.4.1", PackageVersionSource::EmbeddedPackageMetadata)
                .unwrap();
        let manifest = ReleaseManifest {
            version: "0.4.2".into(),
            notes: Some("notes".into()),
            pub_date: None,
            minimum_protocol: Some("1.0".into()),
            platforms: HashMap::from([(
                "windows-x86_64".into(),
                ReleaseManifestPlatform {
                    format: "nsis".into(),
                    signature: "dGVzdC1zaWduYXR1cmUtbG9uZy1lbm91Z2gtb2s=".into(),
                    url: "https://example.com/setup.exe".into(),
                    hash: Some(
                        "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
                            .into(),
                    ),
                    client_build: Some("devmanager/0.4.2".into()),
                    host_build: Some("devmanager-host/0.4.2".into()),
                },
            )]),
        };
        let admitted = evaluate_release_candidate(&current, &manifest, "windows-x86_64").unwrap();
        assert_eq!(admitted.version, Version::new(0, 4, 2));
        assert!(admitted.hash.is_some());
    }

    #[test]
    fn cache_bust_policy_mutates_latest_json_url() {
        let policy = CacheBustingRequestPolicy::for_instant(UNIX_EPOCH + Duration::from_secs(9));
        let busted = policy
            .apply_to_endpoint("https://example.com/latest.json")
            .unwrap();
        assert!(busted.contains("devmanager_cb=9000") || busted.contains("devmanager_cb=9"));
        assert!(busted.contains('?'));
    }
}
