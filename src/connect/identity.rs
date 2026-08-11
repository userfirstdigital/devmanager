//! Durable Connect host/device identity and pairing.
//!
//! Dependency-safe contract: no production `remote.json`, no OS vault, and no
//! synthesized key references. Pairing is owner-device pairing only.

use std::fmt;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::{Uuid, Variant};

use crate::domain::id::CommandId;

pub const CONNECT_IDENTITY_SCHEMA_VERSION: u16 = 1;
pub const IDENTITY_CODEC_VERSION: u16 = 1;
pub const MAX_IDENTITY_PHYSICAL_BYTES: usize = 64 * 1024;
pub const MAX_IDENTITY_NESTING: u32 = 8;
pub const MAX_IDENTITY_MAP_ENTRIES: u32 = 32;
pub const MAX_IDENTITY_ARRAY_ITEMS: u32 = 32;
pub const MAX_IDENTITY_DEVICES: u32 = 16;
pub const MAX_LABEL_BYTES: usize = 64;
pub const MAX_ID_BYTES: usize = 64;
pub const MAX_FINGERPRINT_BYTES: usize = 64;
pub const PAIRING_CODE_LEN: usize = 8;
pub const MAX_IDENTITY_RECEIPTS: usize = 32;

pub(crate) const PAIRING_TOKEN_ALPHABET: &[u8; 32] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdentityLimitField {
    PhysicalBytes,
    Nesting,
    MapEntries,
    ArrayItems,
    Devices,
    Label,
    Id,
    Fingerprint,
    PairingCode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdentityError {
    ProductionStoreForbidden,
    LimitExceeded { field: IdentityLimitField },
    DuplicateField,
    UnknownField,
    CopiedProfile,
    Corrupt,
    MissingCredentialProof,
    WrongCredentialGeneration,
    RevisionConflict,
    NotEnabled,
    AlreadyEnabled,
    UnknownDevice,
    DuplicateDevice,
    DuplicateReceipt,
    InvalidDevice,
    Overflow,
    PersistFailed,
    CommandConflict,
    TransitionRollbackFailed,
    TransitionPending,
    HostRotationCleanupFailed,
}

impl fmt::Display for IdentityError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::ProductionStoreForbidden => "production identity store is unavailable",
            Self::LimitExceeded { .. } => "identity document exceeded a bounded codec limit",
            Self::DuplicateField => "identity document contains a duplicate field",
            Self::UnknownField => "identity document contains an unknown field",
            Self::CopiedProfile => "identity document is bound to a different machine profile",
            Self::Corrupt => "identity document is corrupt",
            Self::MissingCredentialProof => "required credential proof is missing",
            Self::WrongCredentialGeneration => "credential proof generation does not match",
            Self::RevisionConflict => "identity revision conflict",
            Self::NotEnabled => "Connect identity has not been enabled",
            Self::AlreadyEnabled => "Connect identity is already enabled",
            Self::UnknownDevice => "unknown paired device",
            Self::DuplicateDevice => "device identity is not unique",
            Self::DuplicateReceipt => "identity receipt command id is not unique",
            Self::InvalidDevice => "device record is inconsistent",
            Self::Overflow => "identity counter overflow",
            Self::PersistFailed => "identity persistence failed",
            Self::CommandConflict => "command id was reused with a different payload",
            Self::TransitionRollbackFailed => "identity transition rollback failed",
            Self::TransitionPending => "identity transition is pending explicit recovery",
            Self::HostRotationCleanupFailed => "host rotation cleanup failed and can be retried",
        })
    }
}

impl std::error::Error for IdentityError {}

#[derive(Clone, PartialEq, Eq)]
pub struct MachineBinding {
    id: String,
}

impl MachineBinding {
    pub fn new(id: impl Into<String>) -> Self {
        Self { id: id.into() }
    }

    pub fn binding_hash(&self) -> String {
        hex_encode(&Sha256::digest(self.id.as_bytes()))
    }
}

impl fmt::Debug for MachineBinding {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("MachineBinding(redacted)")
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct PairingCode(String);

impl PairingCode {
    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }

    pub(crate) fn parse_valid(raw: &str) -> Result<Self, IdentityError> {
        if raw.len() != PAIRING_CODE_LEN {
            return Err(IdentityError::LimitExceeded {
                field: IdentityLimitField::PairingCode,
            });
        }
        if !raw
            .bytes()
            .all(|byte| PAIRING_TOKEN_ALPHABET.contains(&byte))
        {
            return Err(IdentityError::Corrupt);
        }
        Ok(Self(raw.to_string()))
    }
}

impl fmt::Debug for PairingCode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("PairingCode(redacted)")
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(transparent)]
pub struct HostPublicId(Uuid);

impl HostPublicId {
    pub fn as_bytes(&self) -> &[u8; 16] {
        self.0.as_bytes()
    }

    pub(crate) fn new() -> Self {
        Self(Uuid::now_v7())
    }

    pub(crate) fn parse(raw: &str) -> Result<Self, IdentityError> {
        parse_uuid_v7(raw).map(Self)
    }
}

impl<'de> Deserialize<'de> for HostPublicId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let raw = String::deserialize(deserializer)?;
        Self::parse(&raw).map_err(serde::de::Error::custom)
    }
}

impl fmt::Debug for HostPublicId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("HostPublicId(redacted)")
    }
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct DeviceId(Uuid);

impl DeviceId {
    pub fn as_bytes(&self) -> &[u8; 16] {
        self.0.as_bytes()
    }

    pub(crate) fn new() -> Self {
        Self(Uuid::now_v7())
    }

    pub(crate) fn parse(raw: &str) -> Result<Self, IdentityError> {
        parse_uuid_v7(raw).map(Self)
    }
}

impl<'de> Deserialize<'de> for DeviceId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let raw = String::deserialize(deserializer)?;
        Self::parse(&raw).map_err(serde::de::Error::custom)
    }
}

impl fmt::Debug for DeviceId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("DeviceId")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PairingPurpose {
    OwnerDevice,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum DeviceKind {
    Native,
    Browser,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum BrowserPrivateStorage {
    WebCryptoNonExportableIndexedDb,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum CredentialLocation {
    OsHostVault,
    OsDeviceVault,
    BrowserWebCryptoCapability,
}

#[derive(Clone, PartialEq, Eq)]
pub struct KeyReference {
    pub(crate) location: CredentialLocation,
    pub(crate) fingerprint: String,
    pub(crate) generation: Option<u64>,
}

impl KeyReference {
    pub fn location(&self) -> CredentialLocation {
        self.location
    }

    pub fn fingerprint(&self) -> &str {
        &self.fingerprint
    }

    pub fn generation(&self) -> Option<u64> {
        self.generation
    }

    pub(crate) fn from_host_proof(proof: &HostKeyProof) -> Result<Self, IdentityError> {
        validate_fingerprint(&proof.fingerprint)?;
        if proof.generation == 0 {
            return Err(IdentityError::Corrupt);
        }
        Ok(Self {
            location: CredentialLocation::OsHostVault,
            fingerprint: proof.fingerprint.clone(),
            generation: Some(proof.generation),
        })
    }

    pub(crate) fn from_device_proof(proof: &DeviceKeyProof) -> Result<Self, IdentityError> {
        Ok(Self {
            location: match proof.kind {
                DeviceKind::Native => CredentialLocation::OsDeviceVault,
                DeviceKind::Browser => CredentialLocation::BrowserWebCryptoCapability,
            },
            fingerprint: validate_fingerprint(&proof.fingerprint)?.to_string(),
            generation: None,
        })
    }
}

impl fmt::Debug for KeyReference {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("KeyReference")
            .field("location", &self.location)
            .field("fingerprint", &"redacted")
            .finish()
    }
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BrowserDeviceDto {
    pub browser_install_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nickname: Option<String>,
    pub private_identity_storage: BrowserPrivateStorage,
    pub cleared_storage_requires_visible_repair: bool,
}

impl fmt::Debug for BrowserDeviceDto {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("BrowserDeviceDto")
            .field("private_identity_storage", &self.private_identity_storage)
            .field(
                "cleared_storage_requires_visible_repair",
                &self.cleared_storage_requires_visible_repair,
            )
            .finish()
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct DeviceRecord {
    pub device_id: DeviceId,
    pub kind: DeviceKind,
    pub(crate) label: String,
    pub(crate) legacy_client_id: Option<String>,
    pub public_key: KeyReference,
    pub revoked: bool,
    pub(crate) revoked_at_epoch_ms: Option<u64>,
    pub requires_re_pair: bool,
    pub browser: Option<BrowserDeviceDto>,
}

impl fmt::Debug for DeviceRecord {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DeviceRecord")
            .field("device_id", &self.device_id)
            .field("kind", &self.kind)
            .field("revoked", &self.revoked)
            .field("requires_re_pair", &self.requires_re_pair)
            .finish()
    }
}

impl DeviceRecord {
    pub fn label(&self) -> &str {
        &self.label
    }
}

#[derive(Clone)]
pub struct ConnectIdentity {
    pub(crate) schema_version: u16,
    pub(crate) host_public_id: HostPublicId,
    pub(crate) host_key: KeyReference,
    pub(crate) pairing_code: PairingCode,
    pub(crate) pairing_code_generation: u64,
    pub(crate) pairing_purpose: PairingPurpose,
    pub(crate) profile_binding_hash: String,
    pub(crate) last_seen_host_build: Option<u32>,
    pub(crate) created_at_epoch_ms: u64,
    pub(crate) devices: Vec<DeviceRecord>,
}

impl ConnectIdentity {
    pub fn pairing_code(&self) -> &PairingCode {
        &self.pairing_code
    }

    pub fn host_public_id(&self) -> HostPublicId {
        self.host_public_id
    }

    pub fn host_key(&self) -> &KeyReference {
        &self.host_key
    }

    pub fn device(&self, device_id: DeviceId) -> Option<&DeviceRecord> {
        self.devices
            .iter()
            .find(|device| device.device_id == device_id)
    }

    pub fn devices(&self) -> &[DeviceRecord] {
        &self.devices
    }

    pub fn pairing_code_generation(&self) -> u64 {
        self.pairing_code_generation
    }

    pub fn pairing_purpose(&self) -> PairingPurpose {
        self.pairing_purpose
    }

    pub fn task_invite_id(&self) -> Option<&str> {
        None
    }

    pub(crate) fn validate_structure(&self) -> Result<(), IdentityError> {
        if self.schema_version != CONNECT_IDENTITY_SCHEMA_VERSION {
            return Err(IdentityError::Corrupt);
        }
        PairingCode::parse_valid(self.pairing_code.as_str())?;
        if self.pairing_code_generation == 0 {
            return Err(IdentityError::Corrupt);
        }
        validate_fingerprint(&self.host_key.fingerprint)?;
        if self.host_key.location != CredentialLocation::OsHostVault {
            return Err(IdentityError::Corrupt);
        }
        let generation = self.host_key.generation.unwrap_or(0);
        if generation == 0 {
            return Err(IdentityError::Corrupt);
        }
        if !is_hex_sha256(&self.profile_binding_hash) {
            return Err(IdentityError::Corrupt);
        }
        if self.devices.len() > MAX_IDENTITY_DEVICES as usize {
            return Err(IdentityError::LimitExceeded {
                field: IdentityLimitField::Devices,
            });
        }
        validate_unique_devices(&self.devices)?;
        for device in &self.devices {
            validate_device(device)?;
        }
        Ok(())
    }
}

impl fmt::Debug for ConnectIdentity {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ConnectIdentity")
            .field("schema_version", &self.schema_version)
            .field("device_count", &self.devices.len())
            .finish()
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct IdentitySetup {
    pub host_public_id: HostPublicId,
    pub host_key: KeyReference,
    pub(crate) pairing_code: PairingCode,
    pub pairing_purpose: PairingPurpose,
    pub task_invite_id: Option<String>,
}

impl fmt::Debug for IdentitySetup {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("IdentitySetup")
            .field("pairing_purpose", &self.pairing_purpose)
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostIdentityRotation {
    pub all_devices_require_repair: bool,
    pub affected_device_count: usize,
}

#[derive(Clone)]
pub struct RegisterDevice {
    pub kind: DeviceKind,
    pub label: String,
    pub legacy_client_id: Option<String>,
    pub browser: Option<BrowserDeviceDto>,
}

#[derive(Clone)]
pub struct IdentityCommand {
    pub command_id: CommandId,
    pub expected_revision: u64,
    pub op: IdentityOp,
}

impl IdentityCommand {
    pub(crate) fn payload_digest(&self) -> [u8; 32] {
        let mut digest = Sha256::new();
        digest.update(b"DevManagerConnectIdentityCommand/v1\0");
        digest.update(self.expected_revision.to_be_bytes());
        match &self.op {
            IdentityOp::NoteHostBuild { build } => {
                digest.update([0]);
                digest.update(build.to_be_bytes());
            }
            IdentityOp::Enable {
                host_build,
                now_epoch_ms,
            } => {
                digest.update([1]);
                digest.update(host_build.to_be_bytes());
                digest.update(now_epoch_ms.to_be_bytes());
            }
            IdentityOp::RegisterDevice(request) => {
                digest.update([2]);
                digest.update([match request.kind {
                    DeviceKind::Native => 0,
                    DeviceKind::Browser => 1,
                }]);
                digest_string(&mut digest, &request.label);
                digest_option_string(&mut digest, request.legacy_client_id.as_deref());
                match &request.browser {
                    None => {
                        digest.update([0]);
                    }
                    Some(browser) => {
                        digest.update([1]);
                        digest_string(&mut digest, &browser.browser_install_id);
                        digest_option_string(&mut digest, browser.nickname.as_deref());
                        digest.update([match browser.private_identity_storage {
                            BrowserPrivateStorage::WebCryptoNonExportableIndexedDb => 0,
                        }]);
                        digest.update([u8::from(browser.cleared_storage_requires_visible_repair)]);
                    }
                }
            }
            IdentityOp::RotatePairingCode { now_epoch_ms } => {
                digest.update([3]);
                digest.update(now_epoch_ms.to_be_bytes());
            }
            IdentityOp::RotateHostIdentity { now_epoch_ms } => {
                digest.update([4]);
                digest.update(now_epoch_ms.to_be_bytes());
            }
            IdentityOp::RevokeDevice {
                device_id,
                now_epoch_ms,
            } => {
                digest.update([5]);
                digest.update(device_id.as_bytes());
                digest.update(now_epoch_ms.to_be_bytes());
            }
            IdentityOp::RevokeAllDevices { now_epoch_ms } => {
                digest.update([6]);
                digest.update(now_epoch_ms.to_be_bytes());
            }
        }
        digest.finalize().into()
    }
}

#[derive(Clone)]
pub enum IdentityOp {
    NoteHostBuild {
        build: u32,
    },
    Enable {
        host_build: u32,
        now_epoch_ms: u64,
    },
    RegisterDevice(RegisterDevice),
    RotatePairingCode {
        now_epoch_ms: u64,
    },
    RotateHostIdentity {
        now_epoch_ms: u64,
    },
    RevokeDevice {
        device_id: DeviceId,
        now_epoch_ms: u64,
    },
    RevokeAllDevices {
        now_epoch_ms: u64,
    },
}

/// Durable marker written before a vault mutation. Public ids name vault
/// slots that abandon/retry must reconcile; a restart must not pretend an
/// interrupted vault operation committed.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) struct PendingIdentityTransition {
    pub(crate) command_id: CommandId,
    pub(crate) command_digest: [u8; 32],
    pub(crate) kind: PendingIdentityTransitionKind,
    pub(crate) host_public_id: Option<HostPublicId>,
    pub(crate) device_id: Option<DeviceId>,
}

impl fmt::Debug for PendingIdentityTransition {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PendingIdentityTransition")
            .field("kind", &self.kind)
            .finish()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PendingIdentityTransitionKind {
    Enable,
    RegisterDevice,
    RotateHostIdentity,
}

impl PendingIdentityTransitionKind {
    pub(crate) fn from_operation(operation: &IdentityOp) -> Option<Self> {
        match operation {
            IdentityOp::Enable { .. } => Some(Self::Enable),
            IdentityOp::RegisterDevice(_) => Some(Self::RegisterDevice),
            IdentityOp::RotateHostIdentity { .. } => Some(Self::RotateHostIdentity),
            IdentityOp::NoteHostBuild { .. }
            | IdentityOp::RotatePairingCode { .. }
            | IdentityOp::RevokeDevice { .. }
            | IdentityOp::RevokeAllDevices { .. } => None,
        }
    }
}

#[derive(Clone)]
pub struct IdentityReceipt {
    pub(crate) command_id: CommandId,
    pub(crate) revision: u64,
    pub(crate) setup: Option<IdentitySetup>,
    pub(crate) registered_device: Option<DeviceRecord>,
    pub(crate) pairing_code: Option<PairingCode>,
    pub(crate) host_rotation: Option<HostIdentityRotation>,
    pub(crate) command_digest: Option<[u8; 32]>,
}

impl IdentityReceipt {
    pub fn command_id(&self) -> CommandId {
        self.command_id
    }

    pub fn revision(&self) -> u64 {
        self.revision
    }

    pub fn setup(&self) -> Option<&IdentitySetup> {
        self.setup.as_ref()
    }

    pub fn registered_device(&self) -> Option<&DeviceRecord> {
        self.registered_device.as_ref()
    }

    pub fn pairing_code(&self) -> Option<&PairingCode> {
        self.pairing_code.as_ref()
    }

    pub fn host_rotation(&self) -> Option<&HostIdentityRotation> {
        self.host_rotation.as_ref()
    }
}

impl fmt::Debug for IdentityReceipt {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("IdentityReceipt")
            .field("revision", &self.revision)
            .finish()
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct HostKeyProof {
    generation: u64,
    fingerprint: String,
    _seal: HostProofSeal,
}

#[derive(Clone, PartialEq, Eq)]
struct HostProofSeal;

impl HostKeyProof {
    pub(crate) fn from_parts(generation: u64, fingerprint: impl Into<String>) -> Self {
        Self {
            generation,
            fingerprint: fingerprint.into(),
            _seal: HostProofSeal,
        }
    }

    /// Construct a proof for an in-crate test harness.
    #[cfg(test)]
    pub(crate) fn from_parts_for_test(generation: u64, fingerprint: impl Into<String>) -> Self {
        Self::from_parts(generation, fingerprint)
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn fingerprint(&self) -> &str {
        &self.fingerprint
    }
}

impl fmt::Debug for HostKeyProof {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("HostKeyProof")
            .field("generation", &self.generation)
            .field("fingerprint", &"redacted")
            .finish()
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct DeviceKeyProof {
    kind: DeviceKind,
    fingerprint: String,
    _seal: DeviceProofSeal,
}

#[derive(Clone, PartialEq, Eq)]
struct DeviceProofSeal;

impl DeviceKeyProof {
    pub(crate) fn from_parts(kind: DeviceKind, fingerprint: impl Into<String>) -> Self {
        Self {
            kind,
            fingerprint: fingerprint.into(),
            _seal: DeviceProofSeal,
        }
    }

    /// Construct a proof for an in-crate test harness.
    #[cfg(test)]
    pub(crate) fn from_parts_for_test(kind: DeviceKind, fingerprint: impl Into<String>) -> Self {
        Self::from_parts(kind, fingerprint)
    }

    pub fn kind(&self) -> DeviceKind {
        self.kind
    }

    pub fn fingerprint(&self) -> &str {
        &self.fingerprint
    }
}

impl fmt::Debug for DeviceKeyProof {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DeviceKeyProof")
            .field("kind", &self.kind)
            .field("fingerprint", &"redacted")
            .finish()
    }
}

/// Opaque proof that a connection presented a current registered,
/// non-revoked, host-bound device credential for one session epoch.
///
/// HOLD: real connection/session wiring and OS vault verification stay
/// outside this slice. Callers must mint this only via
/// `bind_device_credential`; a raw `DeviceId` is never enough.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct DeviceCredentialProof {
    device_id: DeviceId,
    session_epoch: u64,
    host_generation: u64,
    _seal: DeviceCredentialSeal,
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct DeviceCredentialSeal;

impl DeviceCredentialProof {
    pub fn device_id(self) -> DeviceId {
        self.device_id
    }

    pub fn session_epoch(self) -> u64 {
        self.session_epoch
    }

    pub fn host_generation(self) -> u64 {
        self.host_generation
    }
}

impl fmt::Debug for DeviceCredentialProof {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DeviceCredentialProof")
            .field("session_epoch", &self.session_epoch)
            .field("host_generation", &self.host_generation)
            .finish()
    }
}

/// Bind a PairedOwner credential after verifying the device is current,
/// registered, non-revoked, and host-bound in the injected vault.
pub fn bind_device_credential<V: CredentialVault>(
    identity: &ConnectIdentity,
    binding: &MachineBinding,
    vault: &V,
    device_id: DeviceId,
    session_epoch: u64,
) -> Result<DeviceCredentialProof, IdentityError> {
    if session_epoch == 0 {
        return Err(IdentityError::MissingCredentialProof);
    }
    identity.validate_structure()?;
    if identity.profile_binding_hash != binding.binding_hash() {
        return Err(IdentityError::CopiedProfile);
    }
    let host_generation = identity.host_key.generation.unwrap_or(0);
    if host_generation == 0 {
        return Err(IdentityError::Corrupt);
    }
    let host_proof =
        HostKeyProof::from_parts(host_generation, identity.host_key.fingerprint.clone());
    vault.verify_host(identity.host_public_id, &host_proof)?;
    let device = identity
        .device(device_id)
        .ok_or(IdentityError::UnknownDevice)?;
    if device.revoked || device.requires_re_pair {
        return Err(IdentityError::UnknownDevice);
    }
    let device_proof =
        DeviceKeyProof::from_parts(device.kind, device.public_key.fingerprint().to_string());
    vault.verify_device(device.device_id, &device_proof)?;
    Ok(DeviceCredentialProof {
        device_id,
        session_epoch,
        host_generation,
        _seal: DeviceCredentialSeal,
    })
}

/// Vault authority seam.
///
/// HOLD: no OS DPAPI / WebCrypto / Portal / relay adapter is wired here.
/// `IsolatedRemoteStore::new` stays `ProductionStoreForbidden` in release
/// until that reviewed adapter exists. Tests use an in-crate fake only.
pub trait CredentialVault {
    /// On error, no credential may have been established. Repeating an
    /// interrupted establishment for the same public ID must return the same
    /// credential rather than rotate or mint a second slot.
    fn establish_host(&mut self, host_id: HostPublicId) -> Result<HostKeyProof, IdentityError>;
    fn rollback_host_establishment(
        &mut self,
        host_id: HostPublicId,
        proof: &HostKeyProof,
    ) -> Result<(), IdentityError>;
    /// On error, the active credential remains unchanged and any pending
    /// rotation can be discarded with `abort_host_rotation`.
    fn prepare_host_rotation(
        &mut self,
        host_id: HostPublicId,
    ) -> Result<HostKeyProof, IdentityError>;
    /// Commit a prepared host rotation. After a successful commit, a
    /// matching retry must be a no-op so crash recovery can settle.
    fn commit_host_rotation(&mut self) -> Result<(), IdentityError>;
    /// Abort a prepared host rotation. Failure is typed and retryable;
    /// implementations must leave the pending slot in place on error.
    /// HOLD: OS vault abort remains unwired.
    fn abort_host_rotation(&mut self) -> Result<(), IdentityError>;
    /// Discard an uncommitted host slot by public id. Absence is success.
    fn discard_uncommitted_host(&mut self, host_id: HostPublicId) -> Result<(), IdentityError>;
    /// Discard an uncommitted device slot by public id. Absence is success.
    fn discard_uncommitted_device(&mut self, device_id: DeviceId) -> Result<(), IdentityError>;
    fn verify_host(&self, host_id: HostPublicId, proof: &HostKeyProof)
        -> Result<(), IdentityError>;
    /// On error, no device credential may have been established. Repeating an
    /// interrupted establishment for the same device ID must be idempotent.
    fn establish_device(
        &mut self,
        device_id: DeviceId,
        kind: DeviceKind,
    ) -> Result<DeviceKeyProof, IdentityError>;
    fn rollback_device_establishment(
        &mut self,
        device_id: DeviceId,
        proof: &DeviceKeyProof,
    ) -> Result<(), IdentityError>;
    fn verify_device(
        &self,
        device_id: DeviceId,
        proof: &DeviceKeyProof,
    ) -> Result<(), IdentityError>;
}

fn digest_string(digest: &mut Sha256, value: &str) {
    digest.update((value.len() as u64).to_be_bytes());
    digest.update(value.as_bytes());
}

fn digest_option_string(digest: &mut Sha256, value: Option<&str>) {
    match value {
        None => digest.update([0]),
        Some(value) => {
            digest.update([1]);
            digest_string(digest, value);
        }
    }
}

pub(crate) fn generate_pairing_code() -> Result<PairingCode, IdentityError> {
    let mut bytes = [0_u8; PAIRING_CODE_LEN];
    getrandom::fill(&mut bytes).map_err(|_| IdentityError::Corrupt)?;
    let code: String = bytes
        .iter()
        .map(|byte| {
            char::from(PAIRING_TOKEN_ALPHABET[(*byte as usize) % PAIRING_TOKEN_ALPHABET.len()])
        })
        .collect();
    PairingCode::parse_valid(&code)
}

pub(crate) fn seed_pairing_code(
    web_token: Option<&str>,
    native_token: Option<&str>,
) -> Result<PairingCode, IdentityError> {
    if let Some(code) = web_token.and_then(|token| PairingCode::parse_valid(token).ok()) {
        return Ok(code);
    }
    if let Some(code) = native_token.and_then(|token| PairingCode::parse_valid(token).ok()) {
        return Ok(code);
    }
    generate_pairing_code()
}

pub(crate) fn rotate_pairing_until_changed(
    current: &PairingCode,
) -> Result<PairingCode, IdentityError> {
    for _ in 0..8 {
        let next = generate_pairing_code()?;
        if next.as_str() != current.as_str() {
            return Ok(next);
        }
    }
    let mut bytes = current.as_str().as_bytes().to_vec();
    let last = bytes.last_mut().ok_or(IdentityError::Corrupt)?;
    let current_index = PAIRING_TOKEN_ALPHABET
        .iter()
        .position(|candidate| *candidate == *last)
        .unwrap_or(0);
    *last = PAIRING_TOKEN_ALPHABET[(current_index + 1) % PAIRING_TOKEN_ALPHABET.len()];
    PairingCode::parse_valid(std::str::from_utf8(&bytes).map_err(|_| IdentityError::Corrupt)?)
}

pub(crate) fn validate_fingerprint(raw: &str) -> Result<&str, IdentityError> {
    if raw.len() != MAX_FINGERPRINT_BYTES {
        return Err(if raw.is_empty() {
            IdentityError::Corrupt
        } else {
            IdentityError::LimitExceeded {
                field: IdentityLimitField::Fingerprint,
            }
        });
    }
    if !is_hex_sha256(raw) {
        return Err(IdentityError::Corrupt);
    }
    Ok(raw)
}

fn validate_device(device: &DeviceRecord) -> Result<(), IdentityError> {
    validate_fingerprint(device.public_key.fingerprint())?;
    if device.public_key.generation().is_some() {
        return Err(IdentityError::InvalidDevice);
    }
    if let Some(legacy_client_id) = device.legacy_client_id.as_deref() {
        if legacy_client_id.is_empty() || contains_control(legacy_client_id) {
            return Err(IdentityError::InvalidDevice);
        }
        if legacy_client_id.len() > MAX_ID_BYTES {
            return Err(IdentityError::LimitExceeded {
                field: IdentityLimitField::Id,
            });
        }
    }
    if device.revoked != device.revoked_at_epoch_ms.is_some() {
        return Err(IdentityError::InvalidDevice);
    }
    if let Some(browser) = device.browser.as_ref() {
        if contains_control(&browser.browser_install_id) {
            return Err(IdentityError::InvalidDevice);
        }
        if browser.browser_install_id.len() > MAX_ID_BYTES {
            return Err(IdentityError::LimitExceeded {
                field: IdentityLimitField::Id,
            });
        }
        if browser
            .nickname
            .as_deref()
            .is_some_and(|nickname| nickname.len() > MAX_LABEL_BYTES)
        {
            return Err(IdentityError::LimitExceeded {
                field: IdentityLimitField::Label,
            });
        }
        if browser.nickname.as_deref().is_some_and(contains_control) {
            return Err(IdentityError::InvalidDevice);
        }
    }
    match (
        device.kind,
        device.browser.as_ref(),
        device.public_key.location,
    ) {
        (DeviceKind::Native, None, CredentialLocation::OsDeviceVault) => {}
        (DeviceKind::Browser, Some(browser), CredentialLocation::BrowserWebCryptoCapability)
            if browser.private_identity_storage
                == BrowserPrivateStorage::WebCryptoNonExportableIndexedDb
                && browser.cleared_storage_requires_visible_repair
                && !browser.browser_install_id.is_empty()
                && browser.browser_install_id.len() <= MAX_ID_BYTES
                && !browser
                    .nickname
                    .as_deref()
                    .is_some_and(|nickname| nickname.len() > MAX_LABEL_BYTES) => {}
        _ => return Err(IdentityError::InvalidDevice),
    }
    if contains_control(&device.label) {
        return Err(IdentityError::InvalidDevice);
    }
    if device.label.len() > MAX_LABEL_BYTES {
        return Err(IdentityError::LimitExceeded {
            field: IdentityLimitField::Label,
        });
    }
    Ok(())
}

fn contains_control(value: &str) -> bool {
    value.chars().any(char::is_control)
}

pub(crate) fn validate_device_record(device: &DeviceRecord) -> Result<(), IdentityError> {
    validate_device(device)
}

fn validate_unique_devices(devices: &[DeviceRecord]) -> Result<(), IdentityError> {
    for (index, device) in devices.iter().enumerate() {
        if devices[..index]
            .iter()
            .any(|existing| existing.device_id == device.device_id)
        {
            return Err(IdentityError::DuplicateDevice);
        }
        if let Some(legacy) = device.legacy_client_id.as_deref() {
            if devices[..index]
                .iter()
                .any(|existing| existing.legacy_client_id.as_deref() == Some(legacy))
            {
                return Err(IdentityError::DuplicateDevice);
            }
        }
        if devices[..index]
            .iter()
            .any(|existing| existing.public_key.fingerprint() == device.public_key.fingerprint())
        {
            return Err(IdentityError::DuplicateDevice);
        }
        if let Some(install) = device
            .browser
            .as_ref()
            .map(|browser| browser.browser_install_id.as_str())
        {
            if devices[..index].iter().any(|existing| {
                existing
                    .browser
                    .as_ref()
                    .is_some_and(|browser| browser.browser_install_id == install)
            }) {
                return Err(IdentityError::DuplicateDevice);
            }
        }
    }
    Ok(())
}

fn parse_uuid_v7(raw: &str) -> Result<Uuid, IdentityError> {
    if raw.len() > MAX_ID_BYTES {
        return Err(IdentityError::LimitExceeded {
            field: IdentityLimitField::Id,
        });
    }
    let uuid = Uuid::parse_str(raw).map_err(|_| IdentityError::Corrupt)?;
    if uuid.get_version_num() != 7 || uuid.get_variant() != Variant::RFC4122 {
        return Err(IdentityError::Corrupt);
    }
    Ok(uuid)
}

fn is_hex_sha256(raw: &str) -> bool {
    raw.len() == 64
        && raw
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
}

pub(crate) fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}
