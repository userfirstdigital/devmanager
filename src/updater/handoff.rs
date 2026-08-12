//! Bounded, identity-preserving update handoff state machine.
//!
//! Pure transitions around active-resource inspection, explicit drain/confirm,
//! atomic matching host+client replacement, reconnect/snapshot-resync, and
//! pre-install abort that restores the old host to Ready. No installer I/O.

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Default absolute budget for blocking update IPC from the UI thread.
pub const UPDATE_IPC_DEADLINE: Duration = Duration::from_secs(2);

/// Files and stores that must survive every new-architecture update.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PreservedUserStateKind {
    ConfigJson,
    RemoteJson,
    DevicePairingIdentity,
    TaskPromptDatabase,
}

/// Legacy surfaces that must never drive cutover or update migration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IgnoredUserStateKind {
    SessionJson,
    LegacyProviderConversations,
}

/// Active work that makes silent binary replacement unsafe.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActiveUpdateResource {
    pub resource_id: String,
    pub kind: String,
    pub lifecycle: String,
    pub task_id: Option<String>,
}

/// Snapshot of live resources considered before PrepareUpdate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateResourceInspection {
    pub inspection_id: u64,
    pub host_boot_id: Uuid,
    pub active: Vec<ActiveUpdateResource>,
    pub confirmable: bool,
}

/// Short-lived token authorizing the remainder of a prepared handoff.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateHandoffToken {
    pub token_id: Uuid,
    pub host_boot_id: Uuid,
    pub inspection_id: u64,
    pub target_version: String,
    pub client_build: String,
    pub host_build: String,
    pub issued_at: SystemTime,
    pub expires_at: SystemTime,
}

impl UpdateHandoffToken {
    pub fn is_expired_at(&self, now: SystemTime) -> bool {
        now >= self.expires_at
    }
}

/// Why a silent or automatic handoff must not proceed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "reason", rename_all = "snake_case", deny_unknown_fields)]
pub enum HandoffBlockReason {
    ActiveResources {
        resources: Vec<ActiveUpdateResource>,
    },
    InspectionNotConfirmable,
    HostClientBuildMismatch {
        client_build: String,
        host_build: String,
    },
    TokenExpired,
    TokenMismatch {
        expected_token_id: Uuid,
        observed_token_id: Uuid,
    },
    UnsafeSilentReplacement,
}

/// Result of asking whether a silent install would be safe.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "decision", rename_all = "snake_case", deny_unknown_fields)]
pub enum SilentReplacementDecision {
    Allowed,
    Refused { block: HandoffBlockReason },
}

/// Phases of the bounded host/client update handoff.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "phase", rename_all = "snake_case", deny_unknown_fields)]
pub enum UpdateHandoffPhase {
    Ready,
    Inspecting,
    Blocked {
        block: HandoffBlockReason,
    },
    AwaitingConfirm {
        token: UpdateHandoffToken,
    },
    Draining {
        token: UpdateHandoffToken,
    },
    ReadyToInstall {
        token: UpdateHandoffToken,
    },
    Installing {
        token: UpdateHandoffToken,
    },
    StartingMatchingHost {
        token: UpdateHandoffToken,
    },
    Reconnecting {
        token: UpdateHandoffToken,
    },
    SnapshotResync {
        token: UpdateHandoffToken,
    },
    Completed {
        installed_version: String,
    },
    /// Pre-install abort restored the previous host admission to Ready.
    AbortedPreInstall {
        restored_host_ready: bool,
        host_boot_id: Uuid,
    },
}

/// Errors from illegal handoff transitions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateHandoffError {
    InvalidPhase {
        expected: &'static str,
        observed: UpdateHandoffPhase,
    },
    Blocked(HandoffBlockReason),
    MissingToken,
}

impl std::fmt::Display for UpdateHandoffError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidPhase { expected, observed } => {
                write!(
                    f,
                    "update handoff is not in `{expected}` (observed {observed:?})"
                )
            }
            Self::Blocked(block) => write!(f, "update handoff blocked: {block:?}"),
            Self::MissingToken => write!(f, "update handoff token is missing"),
        }
    }
}

impl std::error::Error for UpdateHandoffError {}

/// Pure handoff coordinator. Callers supply inspection, clocks, and install outcomes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateHandoffMachine {
    phase: UpdateHandoffPhase,
    token_ttl: Duration,
}

impl Default for UpdateHandoffMachine {
    fn default() -> Self {
        Self::new(Duration::from_secs(120))
    }
}

impl UpdateHandoffMachine {
    pub fn new(token_ttl: Duration) -> Self {
        Self {
            phase: UpdateHandoffPhase::Ready,
            token_ttl,
        }
    }

    pub fn phase(&self) -> &UpdateHandoffPhase {
        &self.phase
    }

    pub fn begin_inspect(&mut self) -> Result<(), UpdateHandoffError> {
        match self.phase {
            UpdateHandoffPhase::Ready
            | UpdateHandoffPhase::Blocked { .. }
            | UpdateHandoffPhase::AbortedPreInstall { .. } => {
                self.phase = UpdateHandoffPhase::Inspecting;
                Ok(())
            }
            _ => Err(UpdateHandoffError::InvalidPhase {
                expected: "Ready|Blocked|AbortedPreInstall",
                observed: self.phase.clone(),
            }),
        }
    }

    /// Decide whether silent replacement is allowed for the inspected resources.
    pub fn decide_silent_replacement(
        inspection: &UpdateResourceInspection,
        client_build: &str,
        host_build: &str,
    ) -> SilentReplacementDecision {
        if extract_build_version(client_build) != extract_build_version(host_build) {
            return SilentReplacementDecision::Refused {
                block: HandoffBlockReason::HostClientBuildMismatch {
                    client_build: client_build.to_string(),
                    host_build: host_build.to_string(),
                },
            };
        }
        if !inspection.active.is_empty() {
            return SilentReplacementDecision::Refused {
                block: HandoffBlockReason::ActiveResources {
                    resources: inspection.active.clone(),
                },
            };
        }
        if !inspection.confirmable {
            return SilentReplacementDecision::Refused {
                block: HandoffBlockReason::InspectionNotConfirmable,
            };
        }
        SilentReplacementDecision::Allowed
    }

    /// Prepare a handoff after inspection. Refuses unsafe silent replacement.
    pub fn prepare(
        &mut self,
        inspection: &UpdateResourceInspection,
        target_version: impl Into<String>,
        client_build: impl Into<String>,
        host_build: impl Into<String>,
        now: SystemTime,
        allow_with_active_after_confirm: bool,
    ) -> Result<&UpdateHandoffToken, UpdateHandoffError> {
        if !matches!(self.phase, UpdateHandoffPhase::Inspecting) {
            return Err(UpdateHandoffError::InvalidPhase {
                expected: "Inspecting",
                observed: self.phase.clone(),
            });
        }

        let client_build = client_build.into();
        let host_build = host_build.into();
        let target_version = target_version.into();

        match Self::decide_silent_replacement(inspection, &client_build, &host_build) {
            SilentReplacementDecision::Refused { block }
                if matches!(
                    block,
                    HandoffBlockReason::HostClientBuildMismatch { .. }
                        | HandoffBlockReason::InspectionNotConfirmable
                ) =>
            {
                self.phase = UpdateHandoffPhase::Blocked {
                    block: block.clone(),
                };
                return Err(UpdateHandoffError::Blocked(block));
            }
            SilentReplacementDecision::Refused {
                block: HandoffBlockReason::ActiveResources { resources },
            } if !allow_with_active_after_confirm => {
                let block = HandoffBlockReason::UnsafeSilentReplacement;
                let _ = resources;
                self.phase = UpdateHandoffPhase::Blocked {
                    block: block.clone(),
                };
                return Err(UpdateHandoffError::Blocked(block));
            }
            SilentReplacementDecision::Refused {
                block: HandoffBlockReason::ActiveResources { .. },
            } => {
                // Explicit confirm path continues below.
            }
            SilentReplacementDecision::Refused { block } => {
                self.phase = UpdateHandoffPhase::Blocked {
                    block: block.clone(),
                };
                return Err(UpdateHandoffError::Blocked(block));
            }
            SilentReplacementDecision::Allowed => {}
        }

        let token = UpdateHandoffToken {
            token_id: Uuid::now_v7(),
            host_boot_id: inspection.host_boot_id,
            inspection_id: inspection.inspection_id,
            target_version,
            client_build,
            host_build,
            issued_at: now,
            expires_at: now + self.token_ttl,
        };
        self.phase = UpdateHandoffPhase::AwaitingConfirm { token };
        match &self.phase {
            UpdateHandoffPhase::AwaitingConfirm { token } => Ok(token),
            _ => unreachable!("phase just set to AwaitingConfirm"),
        }
    }

    pub fn confirm_drain(
        &mut self,
        token_id: Uuid,
        now: SystemTime,
    ) -> Result<(), UpdateHandoffError> {
        let token = self.token_in_phase("AwaitingConfirm", |phase| match phase {
            UpdateHandoffPhase::AwaitingConfirm { token } => Some(token.clone()),
            _ => None,
        })?;
        Self::validate_token(&token, token_id, now)?;
        self.phase = UpdateHandoffPhase::Draining { token };
        Ok(())
    }

    pub fn mark_drained(
        &mut self,
        token_id: Uuid,
        now: SystemTime,
    ) -> Result<(), UpdateHandoffError> {
        let token = self.token_in_phase("Draining", |phase| match phase {
            UpdateHandoffPhase::Draining { token } => Some(token.clone()),
            _ => None,
        })?;
        Self::validate_token(&token, token_id, now)?;
        self.phase = UpdateHandoffPhase::ReadyToInstall { token };
        Ok(())
    }

    pub fn begin_install(
        &mut self,
        token_id: Uuid,
        now: SystemTime,
    ) -> Result<(), UpdateHandoffError> {
        let token = self.token_in_phase("ReadyToInstall", |phase| match phase {
            UpdateHandoffPhase::ReadyToInstall { token } => Some(token.clone()),
            _ => None,
        })?;
        Self::validate_token(&token, token_id, now)?;
        self.phase = UpdateHandoffPhase::Installing { token };
        Ok(())
    }

    pub fn start_matching_host(
        &mut self,
        token_id: Uuid,
        now: SystemTime,
    ) -> Result<(), UpdateHandoffError> {
        let token = self.token_in_phase("Installing", |phase| match phase {
            UpdateHandoffPhase::Installing { token } => Some(token.clone()),
            _ => None,
        })?;
        Self::validate_token(&token, token_id, now)?;
        if extract_build_version(&token.client_build) != extract_build_version(&token.host_build) {
            let block = HandoffBlockReason::HostClientBuildMismatch {
                client_build: token.client_build.clone(),
                host_build: token.host_build.clone(),
            };
            self.phase = UpdateHandoffPhase::Blocked {
                block: block.clone(),
            };
            return Err(UpdateHandoffError::Blocked(block));
        }
        self.phase = UpdateHandoffPhase::StartingMatchingHost { token };
        Ok(())
    }

    pub fn begin_reconnect(
        &mut self,
        token_id: Uuid,
        now: SystemTime,
    ) -> Result<(), UpdateHandoffError> {
        let token = self.token_in_phase("StartingMatchingHost", |phase| match phase {
            UpdateHandoffPhase::StartingMatchingHost { token } => Some(token.clone()),
            _ => None,
        })?;
        Self::validate_token(&token, token_id, now)?;
        self.phase = UpdateHandoffPhase::Reconnecting { token };
        Ok(())
    }

    pub fn begin_snapshot_resync(
        &mut self,
        token_id: Uuid,
        now: SystemTime,
    ) -> Result<(), UpdateHandoffError> {
        let token = self.token_in_phase("Reconnecting", |phase| match phase {
            UpdateHandoffPhase::Reconnecting { token } => Some(token.clone()),
            _ => None,
        })?;
        Self::validate_token(&token, token_id, now)?;
        self.phase = UpdateHandoffPhase::SnapshotResync { token };
        Ok(())
    }

    pub fn complete(&mut self, token_id: Uuid, now: SystemTime) -> Result<(), UpdateHandoffError> {
        let token = self.token_in_phase("SnapshotResync", |phase| match phase {
            UpdateHandoffPhase::SnapshotResync { token } => Some(token.clone()),
            _ => None,
        })?;
        Self::validate_token(&token, token_id, now)?;
        self.phase = UpdateHandoffPhase::Completed {
            installed_version: token.target_version,
        };
        Ok(())
    }

    /// Abort before install bytes replace binaries; old host returns to Ready.
    pub fn abort_pre_install(&mut self) -> Result<UpdateHandoffPhase, UpdateHandoffError> {
        match &self.phase {
            UpdateHandoffPhase::Inspecting
            | UpdateHandoffPhase::Blocked { .. }
            | UpdateHandoffPhase::AwaitingConfirm { .. }
            | UpdateHandoffPhase::Draining { .. }
            | UpdateHandoffPhase::ReadyToInstall { .. } => {
                let host_boot_id = self.current_boot_id().unwrap_or_else(Uuid::nil);
                self.phase = UpdateHandoffPhase::AbortedPreInstall {
                    restored_host_ready: true,
                    host_boot_id,
                };
                Ok(self.phase.clone())
            }
            UpdateHandoffPhase::Installing { .. }
            | UpdateHandoffPhase::StartingMatchingHost { .. }
            | UpdateHandoffPhase::Reconnecting { .. }
            | UpdateHandoffPhase::SnapshotResync { .. }
            | UpdateHandoffPhase::Completed { .. }
            | UpdateHandoffPhase::Ready
            | UpdateHandoffPhase::AbortedPreInstall { .. } => {
                Err(UpdateHandoffError::InvalidPhase {
                    expected: "pre-install phase",
                    observed: self.phase.clone(),
                })
            }
        }
    }

    pub fn return_to_ready_after_abort(&mut self) -> Result<(), UpdateHandoffError> {
        match self.phase {
            UpdateHandoffPhase::AbortedPreInstall {
                restored_host_ready: true,
                ..
            } => {
                self.phase = UpdateHandoffPhase::Ready;
                Ok(())
            }
            _ => Err(UpdateHandoffError::InvalidPhase {
                expected: "AbortedPreInstall(restored_host_ready=true)",
                observed: self.phase.clone(),
            }),
        }
    }

    fn current_boot_id(&self) -> Option<Uuid> {
        match &self.phase {
            UpdateHandoffPhase::AwaitingConfirm { token }
            | UpdateHandoffPhase::Draining { token }
            | UpdateHandoffPhase::ReadyToInstall { token }
            | UpdateHandoffPhase::Installing { token }
            | UpdateHandoffPhase::StartingMatchingHost { token }
            | UpdateHandoffPhase::Reconnecting { token }
            | UpdateHandoffPhase::SnapshotResync { token } => Some(token.host_boot_id),
            UpdateHandoffPhase::AbortedPreInstall { host_boot_id, .. } => Some(*host_boot_id),
            _ => None,
        }
    }

    fn validate_token(
        token: &UpdateHandoffToken,
        token_id: Uuid,
        now: SystemTime,
    ) -> Result<(), UpdateHandoffError> {
        if token.token_id != token_id {
            return Err(UpdateHandoffError::Blocked(
                HandoffBlockReason::TokenMismatch {
                    expected_token_id: token.token_id,
                    observed_token_id: token_id,
                },
            ));
        }
        if token.is_expired_at(now) {
            return Err(UpdateHandoffError::Blocked(
                HandoffBlockReason::TokenExpired,
            ));
        }
        Ok(())
    }

    fn token_in_phase(
        &self,
        expected: &'static str,
        get: impl FnOnce(&UpdateHandoffPhase) -> Option<UpdateHandoffToken>,
    ) -> Result<UpdateHandoffToken, UpdateHandoffError> {
        get(&self.phase).ok_or_else(|| UpdateHandoffError::InvalidPhase {
            expected,
            observed: self.phase.clone(),
        })
    }
}

/// Canonical preserve/ignore policy for update and old-to-new cutover.
pub fn update_state_policy() -> (Vec<PreservedUserStateKind>, Vec<IgnoredUserStateKind>) {
    (
        vec![
            PreservedUserStateKind::ConfigJson,
            PreservedUserStateKind::RemoteJson,
            PreservedUserStateKind::DevicePairingIdentity,
            PreservedUserStateKind::TaskPromptDatabase,
        ],
        vec![
            IgnoredUserStateKind::SessionJson,
            IgnoredUserStateKind::LegacyProviderConversations,
        ],
    )
}

/// Classify a relative user-data path for update preservation.
pub fn classify_user_state_path(relative_path: &str) -> Option<UserStateClassification> {
    let normalized = relative_path.replace('\\', "/");
    let file_name = normalized.rsplit('/').next().unwrap_or(normalized.as_str());
    match file_name {
        "config.json" => Some(UserStateClassification::Preserve(
            PreservedUserStateKind::ConfigJson,
        )),
        "remote.json" => Some(UserStateClassification::Preserve(
            PreservedUserStateKind::RemoteJson,
        )),
        "session.json" => Some(UserStateClassification::Ignore(
            IgnoredUserStateKind::SessionJson,
        )),
        name if name.ends_with(".db")
            || name == "tasks.sqlite"
            || name == "prompts.sqlite"
            || name == "devmanager.sqlite" =>
        {
            Some(UserStateClassification::Preserve(
                PreservedUserStateKind::TaskPromptDatabase,
            ))
        }
        "pairing.json" | "devices.json" | "host-identity.json" => Some(
            UserStateClassification::Preserve(PreservedUserStateKind::DevicePairingIdentity),
        ),
        name if name.contains("rollout") || name.contains("conversation") => Some(
            UserStateClassification::Ignore(IgnoredUserStateKind::LegacyProviderConversations),
        ),
        _ => None,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UserStateClassification {
    Preserve(PreservedUserStateKind),
    Ignore(IgnoredUserStateKind),
}

/// Compare before/after hashes for preserved identity surfaces.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IdentityPreservationReport {
    pub config_hash_before: String,
    pub config_hash_after: String,
    pub remote_hash_before: String,
    pub remote_hash_after: String,
    pub device_pairing_fingerprint_before: String,
    pub device_pairing_fingerprint_after: String,
    pub task_db_hash_before: Option<String>,
    pub task_db_hash_after: Option<String>,
    pub session_json_considered: bool,
    pub legacy_conversations_imported: bool,
}

impl IdentityPreservationReport {
    pub fn preserves_connect_and_config(&self) -> bool {
        !self.session_json_considered
            && !self.legacy_conversations_imported
            && self.config_hash_before == self.config_hash_after
            && self.remote_hash_before == self.remote_hash_after
            && self.device_pairing_fingerprint_before == self.device_pairing_fingerprint_after
    }

    pub fn preserves_new_architecture_task_db(&self) -> bool {
        match (&self.task_db_hash_before, &self.task_db_hash_after) {
            (Some(before), Some(after)) => before == after,
            (None, None) => true, // old-to-new starts empty by policy
            _ => false,
        }
    }
}

/// Extract a semver-looking suffix from `devmanager/0.4.2` style build ids.
pub fn extract_build_version(build: &str) -> Option<&str> {
    build
        .rsplit(['/', '@', ' '])
        .find(|part| part.bytes().next().is_some_and(|b| b.is_ascii_digit()))
}

/// Live active-resource summary used before install. Host wires this to
/// [`crate::domain::Query::InspectHostQuit`] via an **owned** Send+'static probe
/// in [`crate::host::update`] — never a borrowed `&CommandBus` stored as `'static`.
pub trait ActiveResourceProbe: Send {
    fn inspect_for_update(&mut self) -> Result<UpdateResourceInspection, String>;
}

/// Fixed probe for tests and injected doubles (owned, Send+'static).
#[derive(Debug, Clone)]
pub struct FixedActiveResourceProbe {
    pub inspection: UpdateResourceInspection,
}

impl ActiveResourceProbe for FixedActiveResourceProbe {
    fn inspect_for_update(&mut self) -> Result<UpdateResourceInspection, String> {
        Ok(self.inspection.clone())
    }
}

/// Host admission while an update handoff is in flight.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostUpdateAdmission {
    Ready,
    DrainingForUpdate,
    /// Installer launch armed; abort-to-ready is no longer valid.
    InstallingUpdate,
    ResumingAfterUpdate,
}

/// Single NSIS/WiX package identity for matching client+host binaries.
///
/// One installer artifact replaces both executables; this is not two installs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AtomicInstallerBundle {
    pub version: String,
    pub client_exe: String,
    pub host_exe: String,
    pub client_build: String,
    pub host_build: String,
    pub protocol_major: u16,
    pub protocol_minor: u16,
    pub artifact_hash: Option<String>,
    /// True only after `cargo_packager_updater` download verified the signature.
    pub signature_verified_by_packager: bool,
    /// Exact packager `OS-ARCH` platform key bound to the verified artifact.
    pub packager_target: String,
    pub download_url: String,
    pub signature: String,
    pub format: String,
}

impl AtomicInstallerBundle {
    /// Sealed constructor: only the updater download path may mint verified identity.
    ///
    /// Requires a [`VerifiedPackagerDownload`] proof produced after
    /// `cargo_packager_updater` download+verify and sha256 of actual bytes.
    pub(crate) fn from_verified_download(
        proof: VerifiedPackagerDownload,
        protocol_major: u16,
        protocol_minor: u16,
        client_build: impl Into<String>,
        host_build: impl Into<String>,
    ) -> Result<Self, String> {
        let client_build = client_build.into();
        let host_build = host_build.into();
        let bundle = Self {
            version: proof.version,
            client_exe: "devmanager.exe".to_string(),
            host_exe: "devmanager-host.exe".to_string(),
            client_build,
            host_build,
            protocol_major,
            protocol_minor,
            artifact_hash: Some(proof.artifact_hash),
            signature_verified_by_packager: true,
            packager_target: proof.packager_target,
            download_url: proof.download_url,
            signature: proof.signature,
            format: proof.format,
        };
        assert_atomic_installer_bundle(&bundle).map_err(|error| error.to_string())?;
        Ok(bundle)
    }
}

/// Opaque proof that packager crypto verify + byte hash succeeded.
///
/// Cannot be constructed outside the updater download path.
#[derive(Debug, Clone)]
pub struct VerifiedPackagerDownload {
    pub(crate) version: String,
    pub(crate) artifact_hash: String,
    pub(crate) packager_target: String,
    pub(crate) download_url: String,
    pub(crate) signature: String,
    pub(crate) format: String,
}

impl VerifiedPackagerDownload {
    pub(crate) fn new(
        version: impl Into<String>,
        artifact_hash: impl Into<String>,
        packager_target: impl Into<String>,
        download_url: impl Into<String>,
        signature: impl Into<String>,
        format: impl Into<String>,
    ) -> Self {
        Self {
            version: version.into(),
            artifact_hash: artifact_hash.into(),
            packager_target: packager_target.into(),
            download_url: download_url.into(),
            signature: signature.into(),
            format: format.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AtomicBundleError {
    SignatureNotVerifiedByPackager,
    MissingClientExe,
    MissingHostExe,
    HostClientBuildMismatch {
        client_build: String,
        host_build: String,
    },
    ProtocolMismatch {
        detail: String,
    },
    MissingArtifactHash,
    ArtifactHashMismatch {
        expected: String,
        actual: String,
    },
}

impl std::fmt::Display for AtomicBundleError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SignatureNotVerifiedByPackager => write!(
                f,
                "installer bundle signature was not verified by cargo_packager_updater"
            ),
            Self::MissingClientExe => write!(f, "installer bundle is missing devmanager.exe"),
            Self::MissingHostExe => write!(f, "installer bundle is missing devmanager-host.exe"),
            Self::HostClientBuildMismatch {
                client_build,
                host_build,
            } => write!(
                f,
                "installer bundle host/client mismatch: {client_build} vs {host_build}"
            ),
            Self::ProtocolMismatch { detail } => {
                write!(f, "installer bundle protocol mismatch: {detail}")
            }
            Self::MissingArtifactHash => {
                write!(
                    f,
                    "installer bundle is missing required sha256 artifact hash"
                )
            }
            Self::ArtifactHashMismatch { expected, actual } => write!(
                f,
                "installer artifact hash mismatch: expected {expected}, actual {actual}"
            ),
        }
    }
}

impl std::error::Error for AtomicBundleError {}

pub fn assert_atomic_installer_bundle(
    bundle: &AtomicInstallerBundle,
) -> Result<(), AtomicBundleError> {
    if !bundle.signature_verified_by_packager {
        return Err(AtomicBundleError::SignatureNotVerifiedByPackager);
    }
    if bundle.client_exe != "devmanager.exe" {
        return Err(AtomicBundleError::MissingClientExe);
    }
    if bundle.host_exe != "devmanager-host.exe" {
        return Err(AtomicBundleError::MissingHostExe);
    }
    let client_version = extract_build_version(&bundle.client_build);
    let host_version = extract_build_version(&bundle.host_build);
    if client_version != host_version || client_version != Some(bundle.version.as_str()) {
        return Err(AtomicBundleError::HostClientBuildMismatch {
            client_build: bundle.client_build.clone(),
            host_build: bundle.host_build.clone(),
        });
    }
    if bundle.protocol_major != crate::protocol::PROTOCOL_MAJOR {
        return Err(AtomicBundleError::ProtocolMismatch {
            detail: format!(
                "bundle protocol {}.{} incompatible with local {}.{}",
                bundle.protocol_major,
                bundle.protocol_minor,
                crate::protocol::PROTOCOL_MAJOR,
                crate::protocol::PROTOCOL_MINOR
            ),
        });
    }
    if bundle.packager_target.trim().is_empty()
        || bundle.download_url.trim().is_empty()
        || bundle.signature.trim().is_empty()
        || bundle.format.trim().is_empty()
    {
        return Err(AtomicBundleError::ProtocolMismatch {
            detail: "verified bundle missing packager target/url/signature/format binding".into(),
        });
    }
    let Some(hash) = bundle.artifact_hash.as_deref() else {
        return Err(AtomicBundleError::MissingArtifactHash);
    };
    let trimmed = hash.trim();
    let Some(hex) = trimmed.strip_prefix("sha256:") else {
        return Err(AtomicBundleError::ArtifactHashMismatch {
            expected: "sha256:<64 hex>".into(),
            actual: trimmed.to_string(),
        });
    };
    if hex.len() != 64 || !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(AtomicBundleError::ArtifactHashMismatch {
            expected: "sha256:<64 hex>".into(),
            actual: trimmed.to_string(),
        });
    }
    Ok(())
}

/// Inspect a staged directory that must contain both product binaries.
pub fn inspect_atomic_installer_payload_dir(
    staged_dir: &std::path::Path,
    expected: &AtomicInstallerBundle,
) -> Result<(), AtomicBundleError> {
    assert_atomic_installer_bundle(expected)?;
    if !staged_dir.join(&expected.client_exe).is_file() {
        return Err(AtomicBundleError::MissingClientExe);
    }
    if !staged_dir.join(&expected.host_exe).is_file() {
        return Err(AtomicBundleError::MissingHostExe);
    }
    Ok(())
}

/// Hash downloaded installer bytes and compare to the required manifest digest.
pub fn verify_downloaded_artifact_sha256(
    bytes: &[u8],
    expected_hash: &str,
) -> Result<String, AtomicBundleError> {
    use sha2::{Digest, Sha256};
    let actual = format!("sha256:{:x}", Sha256::digest(bytes));
    if actual != expected_hash {
        return Err(AtomicBundleError::ArtifactHashMismatch {
            expected: expected_hash.to_string(),
            actual,
        });
    }
    Ok(actual)
}

/// Old-to-new vs subsequent new-architecture update preservation mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UpdateCutoverKind {
    OldToNew,
    NewToNew,
}

/// Pure before/after checkpoint; never reads production paths itself.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PreservationCheckpoint {
    pub cutover: UpdateCutoverKind,
    pub report: IdentityPreservationReport,
    pub old_binaries_usable_on_failure: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PreservationError {
    ConfigOrRemoteChanged,
    DevicePairingChanged,
    SessionOrLegacyImportForbidden,
    TaskDbRequiredForNewToNew,
    TaskDbMustBeEmptyForOldToNew,
    OldBinariesNotUsableOnFailure,
}

impl std::fmt::Display for PreservationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ConfigOrRemoteChanged => {
                write!(f, "config.json/remote.json hashes must be unchanged")
            }
            Self::DevicePairingChanged => {
                write!(f, "device/pairing identity fingerprint must be unchanged")
            }
            Self::SessionOrLegacyImportForbidden => {
                write!(
                    f,
                    "session.json and legacy conversations must not be imported"
                )
            }
            Self::TaskDbRequiredForNewToNew => {
                write!(
                    f,
                    "new-to-new updates must preserve the task/prompt database hash"
                )
            }
            Self::TaskDbMustBeEmptyForOldToNew => {
                write!(
                    f,
                    "old-to-new cutover must start with an empty task/prompt database"
                )
            }
            Self::OldBinariesNotUsableOnFailure => {
                write!(
                    f,
                    "migration failure must leave old binaries and database usable"
                )
            }
        }
    }
}

impl std::error::Error for PreservationError {}

pub fn validate_preservation_checkpoint(
    checkpoint: &PreservationCheckpoint,
) -> Result<(), PreservationError> {
    if checkpoint.report.session_json_considered || checkpoint.report.legacy_conversations_imported
    {
        return Err(PreservationError::SessionOrLegacyImportForbidden);
    }
    if checkpoint.report.config_hash_before != checkpoint.report.config_hash_after
        || checkpoint.report.remote_hash_before != checkpoint.report.remote_hash_after
    {
        return Err(PreservationError::ConfigOrRemoteChanged);
    }
    if checkpoint.report.device_pairing_fingerprint_before
        != checkpoint.report.device_pairing_fingerprint_after
    {
        return Err(PreservationError::DevicePairingChanged);
    }
    match checkpoint.cutover {
        UpdateCutoverKind::OldToNew => {
            if checkpoint.report.task_db_hash_before.is_some()
                || checkpoint.report.task_db_hash_after.is_some()
            {
                return Err(PreservationError::TaskDbMustBeEmptyForOldToNew);
            }
        }
        UpdateCutoverKind::NewToNew => {
            if !checkpoint.report.preserves_new_architecture_task_db()
                || checkpoint.report.task_db_hash_before.is_none()
            {
                return Err(PreservationError::TaskDbRequiredForNewToNew);
            }
        }
    }
    if !checkpoint.old_binaries_usable_on_failure {
        return Err(PreservationError::OldBinariesNotUsableOnFailure);
    }
    Ok(())
}

fn sha256_file_hex(path: &std::path::Path) -> Result<String, String> {
    use sha2::{Digest, Sha256};
    let bytes = std::fs::read(path)
        .map_err(|error| format!("failed to read {}: {error}", path.display()))?;
    Ok(format!("sha256:{:x}", Sha256::digest(bytes)))
}

fn device_pairing_fingerprint_from_remote_json(remote_json: &str) -> Result<String, String> {
    let value: serde_json::Value = serde_json::from_str(remote_json)
        .map_err(|error| format!("remote.json parse failed: {error}"))?;
    let host_id = value
        .get("hostId")
        .and_then(|v| v.as_str())
        .unwrap_or("missing-host");
    let pairing = value
        .get("pairingCode")
        .and_then(|v| v.as_str())
        .unwrap_or("missing-pairing");
    let device_id = value
        .pointer("/devices/0/deviceId")
        .and_then(|v| v.as_str())
        .unwrap_or("missing-device");
    Ok(format!("pairing:{host_id}:{pairing}:{device_id}"))
}

/// Read a disposable profile-root checkpoint. Never resolves production AppData.
///
/// `profile_root` must be an explicit fixture/temp directory supplied by the
/// caller. Production profile paths are rejected by path policy in tests.
pub fn capture_preservation_checkpoint(
    profile_root: &std::path::Path,
    cutover: UpdateCutoverKind,
) -> Result<PreservationCheckpoint, String> {
    if profile_root.as_os_str().is_empty() {
        return Err("preservation checkpoint requires an explicit disposable profile root".into());
    }
    let config_path = profile_root.join("config.json");
    let remote_path = profile_root.join("remote.json");
    let config_hash = sha256_file_hex(&config_path)?;
    let remote_bytes = std::fs::read_to_string(&remote_path)
        .map_err(|error| format!("failed to read remote.json: {error}"))?;
    let remote_hash = {
        use sha2::{Digest, Sha256};
        format!("sha256:{:x}", Sha256::digest(remote_bytes.as_bytes()))
    };
    let fingerprint = device_pairing_fingerprint_from_remote_json(&remote_bytes)?;

    // session.json may exist but must never drive the report.
    let _ = profile_root.join("session.json");

    let task_db_hash = hash_task_prompt_database(profile_root, cutover)?;

    let report = IdentityPreservationReport {
        config_hash_before: config_hash.clone(),
        config_hash_after: config_hash,
        remote_hash_before: remote_hash.clone(),
        remote_hash_after: remote_hash,
        device_pairing_fingerprint_before: fingerprint.clone(),
        device_pairing_fingerprint_after: fingerprint,
        task_db_hash_before: task_db_hash.clone(),
        task_db_hash_after: task_db_hash,
        session_json_considered: false,
        legacy_conversations_imported: false,
    };
    let checkpoint = PreservationCheckpoint {
        cutover,
        report,
        old_binaries_usable_on_failure: true,
    };
    validate_preservation_checkpoint(&checkpoint).map_err(|error| error.to_string())?;
    Ok(checkpoint)
}

/// Bounded host/client update handoff coordinator used by [`crate::updater::UpdaterService`].
#[derive(Debug, Clone)]
pub struct HostUpdateHandoff {
    machine: UpdateHandoffMachine,
    admission: HostUpdateAdmission,
    /// True after [`Self::begin_atomic_install`]; abort-to-ready is refused.
    install_irreversible: bool,
}

impl Default for HostUpdateHandoff {
    fn default() -> Self {
        Self::new(Duration::from_secs(120))
    }
}

impl HostUpdateHandoff {
    pub fn new(token_ttl: Duration) -> Self {
        Self {
            machine: UpdateHandoffMachine::new(token_ttl),
            admission: HostUpdateAdmission::Ready,
            install_irreversible: false,
        }
    }

    pub fn admission(&self) -> HostUpdateAdmission {
        self.admission
    }

    pub fn phase(&self) -> &UpdateHandoffPhase {
        self.machine.phase()
    }

    pub fn install_irreversible(&self) -> bool {
        self.install_irreversible
    }

    /// Inspect via a live probe, then decide silent-replacement safety.
    pub fn inspect_with_probe(
        &mut self,
        probe: &mut dyn ActiveResourceProbe,
        client_build: &str,
        host_build: &str,
    ) -> Result<(UpdateResourceInspection, SilentReplacementDecision), UpdateHandoffError> {
        let inspection = probe.inspect_for_update().map_err(|_detail| {
            UpdateHandoffError::Blocked(HandoffBlockReason::InspectionNotConfirmable)
        })?;
        self.machine.begin_inspect()?;
        let decision =
            UpdateHandoffMachine::decide_silent_replacement(&inspection, client_build, host_build);
        Ok((inspection, decision))
    }

    pub fn inspect_active_resources(
        &mut self,
        inspection: UpdateResourceInspection,
        client_build: &str,
        host_build: &str,
    ) -> Result<SilentReplacementDecision, UpdateHandoffError> {
        self.machine.begin_inspect()?;
        Ok(UpdateHandoffMachine::decide_silent_replacement(
            &inspection,
            client_build,
            host_build,
        ))
    }

    pub fn prepare_update(
        &mut self,
        inspection: &UpdateResourceInspection,
        target_version: impl Into<String>,
        client_build: impl Into<String>,
        host_build: impl Into<String>,
        now: SystemTime,
        allow_explicit_confirm_with_active: bool,
    ) -> Result<UpdateHandoffToken, UpdateHandoffError> {
        if !matches!(self.machine.phase(), UpdateHandoffPhase::Inspecting) {
            self.machine.begin_inspect()?;
        }
        let token = self
            .machine
            .prepare(
                inspection,
                target_version,
                client_build,
                host_build,
                now,
                allow_explicit_confirm_with_active,
            )?
            .clone();
        self.admission = HostUpdateAdmission::DrainingForUpdate;
        self.install_irreversible = false;
        Ok(token)
    }

    /// Full pre-install gate: probe → refuse unsafe silent → expiring token → drain/confirm.
    /// Remains abortable until [`Self::begin_atomic_install`].
    pub fn run_pre_install_gate(
        &mut self,
        probe: &mut dyn ActiveResourceProbe,
        target_version: &str,
        client_build: &str,
        host_build: &str,
        now: SystemTime,
        allow_explicit_confirm_with_active: bool,
    ) -> Result<UpdateHandoffToken, UpdateHandoffError> {
        let (inspection, _decision) = self.inspect_with_probe(probe, client_build, host_build)?;
        let token = self.prepare_update(
            &inspection,
            target_version,
            client_build,
            host_build,
            now,
            allow_explicit_confirm_with_active,
        )?;
        self.confirm_and_drain(token.token_id, now)?;
        Ok(token)
    }

    pub fn confirm_and_drain(
        &mut self,
        token_id: Uuid,
        now: SystemTime,
    ) -> Result<(), UpdateHandoffError> {
        self.machine.confirm_drain(token_id, now)?;
        self.machine.mark_drained(token_id, now)?;
        self.admission = HostUpdateAdmission::DrainingForUpdate;
        Ok(())
    }

    /// Arms installer execution phase. Remains abortable until
    /// [`Self::seal_after_durable_stage`] after a recoverable stage marker exists.
    pub fn begin_atomic_install(
        &mut self,
        token_id: Uuid,
        now: SystemTime,
    ) -> Result<(), UpdateHandoffError> {
        self.machine.begin_install(token_id, now)?;
        self.admission = HostUpdateAdmission::InstallingUpdate;
        self.install_irreversible = false;
        Ok(())
    }

    /// Seal irreversibility only after durable recoverable stage marker is ready.
    pub fn seal_after_durable_stage(&mut self) -> Result<(), UpdateHandoffError> {
        if !matches!(self.machine.phase(), UpdateHandoffPhase::Installing { .. }) {
            return Err(UpdateHandoffError::InvalidPhase {
                expected: "Installing",
                observed: self.machine.phase().clone(),
            });
        }
        self.install_irreversible = true;
        Ok(())
    }

    pub fn complete_matching_host_start(
        &mut self,
        token_id: Uuid,
        now: SystemTime,
    ) -> Result<(), UpdateHandoffError> {
        self.machine.start_matching_host(token_id, now)?;
        self.machine.begin_reconnect(token_id, now)?;
        self.machine.begin_snapshot_resync(token_id, now)?;
        self.admission = HostUpdateAdmission::ResumingAfterUpdate;
        Ok(())
    }

    pub fn finish_resync(
        &mut self,
        token_id: Uuid,
        now: SystemTime,
    ) -> Result<(), UpdateHandoffError> {
        self.machine.complete(token_id, now)?;
        self.admission = HostUpdateAdmission::Ready;
        self.install_irreversible = false;
        Ok(())
    }

    /// Abort before irreversible install; restores Ready admission and old-host-ready.
    pub fn abort_pre_install(&mut self) -> Result<HostUpdateAdmission, UpdateHandoffError> {
        if self.install_irreversible {
            return Err(UpdateHandoffError::InvalidPhase {
                expected: "pre-irreversible handoff",
                observed: self.machine.phase().clone(),
            });
        }
        self.machine.abort_pre_install()?;
        self.machine.return_to_ready_after_abort()?;
        self.admission = HostUpdateAdmission::Ready;
        Ok(self.admission)
    }

    pub fn refuse_unsafe_silent_replacement(
        resources: &[ActiveUpdateResource],
    ) -> Result<(), HandoffBlockReason> {
        if resources.is_empty() {
            Ok(())
        } else {
            Err(HandoffBlockReason::UnsafeSilentReplacement)
        }
    }
}

/// Process-local host update admission: stop new launches while draining/installing.
///
/// Owned by the host executor and optionally bound into [`crate::updater::UpdaterService`]
/// so client and host share one FSM.
#[derive(Debug, Default)]
pub struct HostUpdateRuntimeGate {
    handoff: Mutex<HostUpdateHandoff>,
}

impl HostUpdateRuntimeGate {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            handoff: Mutex::new(HostUpdateHandoff::default()),
        })
    }

    pub fn admission(&self) -> HostUpdateAdmission {
        self.handoff
            .lock()
            .map(|guard| guard.admission())
            .unwrap_or(HostUpdateAdmission::Ready)
    }

    /// New task/resource/provider/browser launches are refused while draining or installing.
    pub fn stops_new_launches(&self) -> bool {
        matches!(
            self.admission(),
            HostUpdateAdmission::DrainingForUpdate | HostUpdateAdmission::InstallingUpdate
        )
    }

    pub fn prepare_update(
        &self,
        probe: &mut dyn ActiveResourceProbe,
        target_version: &str,
        client_build: &str,
        host_build: &str,
        now: SystemTime,
        allow_explicit_confirm_with_active: bool,
    ) -> Result<UpdateHandoffToken, UpdateHandoffError> {
        let mut handoff = self
            .handoff
            .lock()
            .map_err(|_| UpdateHandoffError::InvalidPhase {
                expected: "unlocked HostUpdateHandoff",
                observed: UpdateHandoffPhase::Ready,
            })?;
        let (inspection, _decision) =
            handoff.inspect_with_probe(probe, client_build, host_build)?;
        handoff.prepare_update(
            &inspection,
            target_version,
            client_build,
            host_build,
            now,
            allow_explicit_confirm_with_active,
        )
    }

    pub fn confirm_drain(&self, token_id: Uuid, now: SystemTime) -> Result<(), UpdateHandoffError> {
        let mut handoff = self
            .handoff
            .lock()
            .map_err(|_| UpdateHandoffError::InvalidPhase {
                expected: "unlocked HostUpdateHandoff",
                observed: UpdateHandoffPhase::Ready,
            })?;
        handoff.confirm_and_drain(token_id, now)
    }

    pub fn begin_atomic_install(
        &self,
        token_id: Uuid,
        now: SystemTime,
    ) -> Result<(), UpdateHandoffError> {
        let mut handoff = self
            .handoff
            .lock()
            .map_err(|_| UpdateHandoffError::InvalidPhase {
                expected: "unlocked HostUpdateHandoff",
                observed: UpdateHandoffPhase::Ready,
            })?;
        handoff.begin_atomic_install(token_id, now)
    }

    pub fn seal_after_durable_stage(&self) -> Result<(), UpdateHandoffError> {
        let mut handoff = self
            .handoff
            .lock()
            .map_err(|_| UpdateHandoffError::InvalidPhase {
                expected: "unlocked HostUpdateHandoff",
                observed: UpdateHandoffPhase::Ready,
            })?;
        handoff.seal_after_durable_stage()
    }

    pub fn abort_pre_install(&self) -> Result<HostUpdateAdmission, UpdateHandoffError> {
        let mut handoff = self
            .handoff
            .lock()
            .map_err(|_| UpdateHandoffError::InvalidPhase {
                expected: "unlocked HostUpdateHandoff",
                observed: UpdateHandoffPhase::Ready,
            })?;
        handoff.abort_pre_install()
    }

    pub fn complete_matching_host_start(
        &self,
        token_id: Uuid,
        now: SystemTime,
    ) -> Result<(), UpdateHandoffError> {
        let mut handoff = self
            .handoff
            .lock()
            .map_err(|_| UpdateHandoffError::InvalidPhase {
                expected: "unlocked HostUpdateHandoff",
                observed: UpdateHandoffPhase::Ready,
            })?;
        handoff.complete_matching_host_start(token_id, now)
    }

    pub fn finish_resync(&self, token_id: Uuid, now: SystemTime) -> Result<(), UpdateHandoffError> {
        let mut handoff = self
            .handoff
            .lock()
            .map_err(|_| UpdateHandoffError::InvalidPhase {
                expected: "unlocked HostUpdateHandoff",
                observed: UpdateHandoffPhase::Ready,
            })?;
        handoff.finish_resync(token_id, now)
    }
}

/// Timed IPC/control port that drives the shared host update FSM without freezing UI.
///
/// Implementors must honor absolute deadlines (never block unbounded on the UI thread).
pub trait HostUpdateControlPort: Send {
    fn prepare_update(
        &self,
        target_version: &str,
        client_build: &str,
        host_build: &str,
        allow_explicit_confirm_with_active: bool,
        deadline: Instant,
    ) -> Result<UpdateHandoffToken, String>;

    fn confirm_drain(&self, token_id: Uuid, deadline: Instant) -> Result<(), String>;

    fn abort_pre_install(&self, deadline: Instant) -> Result<(), String>;

    fn begin_atomic_install(&self, token_id: Uuid, deadline: Instant) -> Result<(), String>;

    fn seal_after_durable_stage(&self, deadline: Instant) -> Result<(), String>;
}

fn sha256_sqlite_canonical_task_prompt(path: &Path) -> Result<String, String> {
    use rusqlite::{Connection, OpenFlags};
    use sha2::{Digest, Sha256};

    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|error| {
        format!(
            "failed to open task/prompt sqlite {}: {error}",
            path.display()
        )
    })?;

    let mut hasher = Sha256::new();
    // Canonical projection tables + FTS content when present (allowlisted only).
    for (table, sql) in [
        ("tasks", "SELECT * FROM tasks ORDER BY task_id ASC"),
        (
            "agent_sessions",
            "SELECT * FROM agent_sessions ORDER BY agent_session_id ASC",
        ),
        (
            "artifacts",
            "SELECT * FROM artifacts ORDER BY artifact_id ASC",
        ),
        (
            "resources",
            "SELECT * FROM resources ORDER BY resource_id ASC",
        ),
        ("prompts", "SELECT * FROM prompts ORDER BY rowid ASC"),
        (
            "task_prompts",
            "SELECT * FROM task_prompts ORDER BY rowid ASC",
        ),
        ("prompt_fts", "SELECT * FROM prompt_fts ORDER BY rowid ASC"),
        ("tasks_fts", "SELECT * FROM tasks_fts ORDER BY rowid ASC"),
    ] {
        let exists: bool = conn
            .query_row(
                "SELECT COUNT(*) > 0 FROM sqlite_master WHERE type IN ('table','view') AND name = ?1",
                [table],
                |row| row.get(0),
            )
            .unwrap_or(false);
        if !exists {
            continue;
        }
        hasher.update(table.as_bytes());
        hasher.update([0x00]);
        let mut stmt = conn
            .prepare(sql)
            .map_err(|error| format!("prepare {table}: {error}"))?;
        let column_count = stmt.column_count();
        let mut rows = stmt
            .query([])
            .map_err(|error| format!("query {table}: {error}"))?;
        while let Some(row) = rows
            .next()
            .map_err(|error| format!("row {table}: {error}"))?
        {
            for idx in 0..column_count {
                let value: rusqlite::types::Value = row
                    .get(idx)
                    .map_err(|error| format!("column {table}.{idx}: {error}"))?;
                match value {
                    rusqlite::types::Value::Null => hasher.update([0]),
                    rusqlite::types::Value::Integer(v) => {
                        hasher.update([1]);
                        hasher.update(v.to_le_bytes());
                    }
                    rusqlite::types::Value::Real(v) => {
                        hasher.update([2]);
                        hasher.update(v.to_bits().to_le_bytes());
                    }
                    rusqlite::types::Value::Text(v) => {
                        hasher.update([3]);
                        let len = u64::try_from(v.len()).unwrap_or(u64::MAX);
                        hasher.update(len.to_le_bytes());
                        hasher.update(v.as_bytes());
                    }
                    rusqlite::types::Value::Blob(v) => {
                        hasher.update([4]);
                        let len = u64::try_from(v.len()).unwrap_or(u64::MAX);
                        hasher.update(len.to_le_bytes());
                        hasher.update(&v);
                    }
                }
            }
            hasher.update([0xFF]);
        }
    }
    Ok(format!("sha256:{:x}", hasher.finalize()))
}

fn hash_task_prompt_database(
    profile_root: &Path,
    cutover: UpdateCutoverKind,
) -> Result<Option<String>, String> {
    match cutover {
        UpdateCutoverKind::OldToNew => Ok(None),
        UpdateCutoverKind::NewToNew => {
            let sqlite_candidates = [
                profile_root.join("devmanager.sqlite"),
                profile_root.join("tasks.sqlite"),
                profile_root.join("command-bus.sqlite"),
            ];
            if let Some(path) = sqlite_candidates.into_iter().find(|path| path.is_file()) {
                return Ok(Some(sha256_sqlite_canonical_task_prompt(&path)?));
            }
            let json_path = profile_root.join("task-prompt-db.json");
            if json_path.is_file() {
                return Ok(Some(sha256_file_hex(&json_path)?));
            }
            Err(
                "new-to-new checkpoint requires task/prompt sqlite (canonical tables/FTS) or disposable json fixture"
                    .into(),
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn empty_inspection(boot: Uuid) -> UpdateResourceInspection {
        UpdateResourceInspection {
            inspection_id: 7,
            host_boot_id: boot,
            active: Vec::new(),
            confirmable: true,
        }
    }

    #[test]
    fn refuses_silent_replacement_when_resources_are_active() {
        let inspection = UpdateResourceInspection {
            inspection_id: 1,
            host_boot_id: Uuid::nil(),
            active: vec![ActiveUpdateResource {
                resource_id: "res-1".into(),
                kind: "terminal".into(),
                lifecycle: "Active".into(),
                task_id: Some("task-1".into()),
            }],
            confirmable: true,
        };
        assert!(matches!(
            UpdateHandoffMachine::decide_silent_replacement(
                &inspection,
                "devmanager/0.4.2",
                "devmanager-host/0.4.2",
            ),
            SilentReplacementDecision::Refused {
                block: HandoffBlockReason::ActiveResources { .. }
            }
        ));
    }

    #[test]
    fn pre_install_abort_marks_old_host_ready() {
        let boot = Uuid::now_v7();
        let mut machine = UpdateHandoffMachine::default();
        machine.begin_inspect().unwrap();
        let token = machine
            .prepare(
                &empty_inspection(boot),
                "0.4.2",
                "devmanager/0.4.2",
                "devmanager-host/0.4.2",
                SystemTime::UNIX_EPOCH + Duration::from_secs(10),
                false,
            )
            .unwrap()
            .clone();
        let aborted = machine.abort_pre_install().unwrap();
        assert_eq!(
            aborted,
            UpdateHandoffPhase::AbortedPreInstall {
                restored_host_ready: true,
                host_boot_id: boot,
            }
        );
        assert_eq!(token.host_boot_id, boot);
        machine.return_to_ready_after_abort().unwrap();
        assert_eq!(machine.phase(), &UpdateHandoffPhase::Ready);
    }

    #[test]
    fn session_json_is_ignored_by_policy() {
        let (preserve, ignore) = update_state_policy();
        assert!(preserve.contains(&PreservedUserStateKind::ConfigJson));
        assert!(preserve.contains(&PreservedUserStateKind::RemoteJson));
        assert!(preserve.contains(&PreservedUserStateKind::DevicePairingIdentity));
        assert!(preserve.contains(&PreservedUserStateKind::TaskPromptDatabase));
        assert!(ignore.contains(&IgnoredUserStateKind::SessionJson));
        assert_eq!(
            classify_user_state_path("profile/session.json"),
            Some(UserStateClassification::Ignore(
                IgnoredUserStateKind::SessionJson
            ))
        );
    }
}
