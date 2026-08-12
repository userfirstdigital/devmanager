//! Versioned bounded codec for isolated Connect identity documents.

use std::collections::BTreeSet;
use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::identity::{
    hex_encode, validate_fingerprint, BrowserDeviceDto, ConnectIdentity, CredentialLocation,
    DeviceId, DeviceKind, DeviceRecord, HostIdentityRotation, HostPublicId, IdentityError,
    IdentityLimitField, IdentityReceipt, IdentitySetup, KeyReference, PairingCode, PairingPurpose,
    PendingIdentityTransition, PendingIdentityTransitionKind, PendingRevocationJournal,
    CONNECT_IDENTITY_SCHEMA_VERSION,
    IDENTITY_CODEC_VERSION, MAX_FINGERPRINT_BYTES, MAX_IDENTITY_ARRAY_ITEMS, MAX_IDENTITY_DEVICES,
    MAX_IDENTITY_MAP_ENTRIES, MAX_IDENTITY_NESTING, MAX_IDENTITY_PHYSICAL_BYTES,
    MAX_IDENTITY_RECEIPTS, MAX_ID_BYTES, MAX_LABEL_BYTES, PAIRING_CODE_LEN,
};
use crate::domain::id::CommandId;

#[derive(Clone, Default)]
pub(crate) struct IdentityDocument {
    pub revision: u64,
    /// Physical persistence CAS epoch. Distinct from `revision`, which is
    /// the logical identity command cursor. Pending markers advance this
    /// epoch without consuming a logical revision.
    pub cas_epoch: u64,
    pub connect_host_build: Option<u32>,
    // HOLD: legacy host.pairingToken, host.web.pairingToken, host.serverId,
    // live `/pair?t=`, and ClientAuth::PairToken remain remote pairing
    // authority until an explicit removal cutover. Identity-core must not
    // claim these fields are replaced.
    pub native_pairing_token: Option<PairingCode>,
    pub web_pairing_token: Option<PairingCode>,
    pub host_server_id: Option<String>,
    pub known_hosts: Vec<Value>,
    pub identity: Option<ConnectIdentity>,
    pub receipts: Vec<IdentityReceipt>,
    pub pending_transition: Option<PendingIdentityTransition>,
    pub pending_revocation: Option<PendingRevocationJournal>,
    pub requires_explicit_reestablish: bool,
}

impl fmt::Debug for IdentityDocument {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("IdentityDocument")
            .field("revision", &self.revision)
            .field("known_host_count", &self.known_hosts.len())
            .field("has_identity", &self.identity.is_some())
            .field("receipt_count", &self.receipts.len())
            .field("has_pending_transition", &self.pending_transition.is_some())
            .field("has_pending_revocation", &self.pending_revocation.is_some())
            .field(
                "requires_explicit_reestablish",
                &self.requires_explicit_reestablish,
            )
            .finish()
    }
}

#[derive(Default, Serialize, Deserialize)]
// Legacy `remote.json` carries unrelated host/web fields. Versioned documents
// are checked by `validate_versioned_wire_fields` before this compatibility
// shape is deserialized.
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct WireDocument {
    #[serde(default)]
    host: Option<WireHost>,
    #[serde(default)]
    known_hosts: Option<Vec<Value>>,
    #[serde(default)]
    connect_host_build: Option<u32>,
    #[serde(default)]
    connect_codec_version: Option<u16>,
    #[serde(default)]
    connect_revision: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    connect_cas_epoch: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    connect_receipts: Option<Vec<WireReceipt>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    connect_identity: Option<WireIdentity>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    connect_pending_transition: Option<WirePendingTransition>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    connect_pending_revocation: Option<WireRevocationJournal>,
    #[serde(default)]
    connect_requires_explicit_reestablish: bool,
}

#[derive(Default, Serialize, Deserialize)]
// HOLD: pairingToken / web.pairingToken / serverId are live remote
// compatibility fields, not Connect identity authority. Removal is a
// separate cutover; do not treat decode-here as replacement.
#[serde(rename_all = "camelCase")]
struct WireHost {
    #[serde(default)]
    pairing_token: Option<String>,
    #[serde(default)]
    server_id: Option<String>,
    #[serde(default)]
    web: Option<WireWeb>,
}

#[derive(Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct WireWeb {
    #[serde(default)]
    pairing_token: Option<String>,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct WireIdentity {
    schema_version: u16,
    host_public_id: String,
    host_key: WireKey,
    pairing_code: String,
    pairing_code_generation: u64,
    pairing_purpose: PairingPurpose,
    profile_binding_hash: String,
    #[serde(default)]
    last_seen_host_build: Option<u32>,
    created_at_epoch_ms: u64,
    #[serde(default)]
    devices: Vec<WireDevice>,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct WireKey {
    location: CredentialLocation,
    fingerprint: String,
    #[serde(default)]
    generation: Option<u64>,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct WireDevice {
    device_id: String,
    kind: DeviceKind,
    #[serde(default)]
    label: Option<String>,
    #[serde(default)]
    legacy_client_id: Option<String>,
    public_key: WireKey,
    revoked: bool,
    #[serde(default)]
    revoked_at_epoch_ms: Option<u64>,
    requires_re_pair: bool,
    #[serde(default)]
    browser: Option<BrowserDeviceDto>,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct WireReceipt {
    command_id: String,
    revision: u64,
    #[serde(default)]
    kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    command_digest: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    setup: Option<WireSetup>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    registered_device: Option<WireDevice>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pairing_code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    host_rotation: Option<WireHostRotation>,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct WireSetup {
    host_public_id: String,
    host_key: WireKey,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pairing_code: Option<String>,
    pairing_purpose: PairingPurpose,
    #[serde(default)]
    task_invite_id: Option<String>,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct WireHostRotation {
    all_devices_require_repair: bool,
    affected_device_count: usize,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct WirePendingTransition {
    command_id: String,
    command_digest: String,
    kind: String,
    transition_nonce: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    claim_owner: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    claim_expires_at_epoch_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    claim_logical_revision: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    host_public_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    device_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    previous_identity: Option<WireIdentity>,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct WireRevocationJournal {
    command_id: String,
    command_digest: String,
    revoke_all: bool,
    entries: Vec<WireRevocationEntry>,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct WireRevocationEntry {
    device_id: String,
    revocation_epoch: u64,
}

const CONNECT_IDENTITY_KEYS: &[&str] = &[
    "schemaVersion",
    "hostPublicId",
    "hostKey",
    "pairingCode",
    "pairingCodeGeneration",
    "pairingPurpose",
    "profileBindingHash",
    "lastSeenHostBuild",
    "createdAtEpochMs",
    "devices",
];

pub(crate) fn decode_identity_bytes(bytes: &[u8]) -> Result<IdentityDocument, IdentityError> {
    if bytes.len() > MAX_IDENTITY_PHYSICAL_BYTES {
        return Err(IdentityError::LimitExceeded {
            field: IdentityLimitField::PhysicalBytes,
        });
    }
    if bytes.iter().all(|byte| byte.is_ascii_whitespace()) {
        return Err(IdentityError::Corrupt);
    }
    scan_bounded_json(bytes)?;
    let value: Value = serde_json::from_slice(bytes).map_err(|_| IdentityError::Corrupt)?;
    let versioned = is_versioned_identity_document(&value);
    if versioned {
        validate_versioned_wire_fields(&value)?;
    }
    let wire: WireDocument = serde_json::from_value(value).map_err(|_| IdentityError::Corrupt)?;
    wire_to_document(wire, versioned)
}

fn is_versioned_identity_document(value: &Value) -> bool {
    let Value::Object(fields) = value else {
        return false;
    };
    [
        "connectIdentity",
        "connectReceipts",
        "connectHostBuild",
        "connectCodecVersion",
        "connectRevision",
        "connectCasEpoch",
        "connectRequiresExplicitReestablish",
        "connectPendingTransition",
        "connectPendingRevocation",
    ]
    .iter()
    .any(|key| fields.contains_key(*key))
}

fn validate_versioned_wire_fields(value: &Value) -> Result<(), IdentityError> {
    let Value::Object(fields) = value else {
        return Err(IdentityError::Corrupt);
    };
    const ROOT_KEYS: &[&str] = &[
        "host",
        "knownHosts",
        "connectHostBuild",
        "connectCodecVersion",
        "connectRevision",
        "connectCasEpoch",
        "connectReceipts",
        "connectIdentity",
        "connectPendingTransition",
        "connectPendingRevocation",
        "connectRequiresExplicitReestablish",
    ];
    reject_unknown_keys(fields, ROOT_KEYS)?;
    if let Some(Value::Object(host)) = fields.get("host") {
        reject_unknown_keys(host, &["pairingToken", "serverId", "web"])?;
        if let Some(Value::Object(web)) = host.get("web") {
            reject_unknown_keys(web, &["pairingToken"])?;
        }
    }
    Ok(())
}

fn reject_unknown_keys(
    fields: &serde_json::Map<String, Value>,
    allowed: &[&str],
) -> Result<(), IdentityError> {
    if fields
        .keys()
        .any(|key| !allowed.iter().any(|candidate| *candidate == key))
    {
        return Err(IdentityError::UnknownField);
    }
    Ok(())
}

pub(crate) fn encode_identity_document(
    document: &IdentityDocument,
) -> Result<Vec<u8>, IdentityError> {
    let wire = document_to_wire(document)?;
    let encoded = serde_json::to_vec(&wire).map_err(|_| IdentityError::Corrupt)?;
    if encoded.len() > MAX_IDENTITY_PHYSICAL_BYTES {
        return Err(IdentityError::LimitExceeded {
            field: IdentityLimitField::PhysicalBytes,
        });
    }
    Ok(encoded)
}

fn wire_to_document(
    wire: WireDocument,
    versioned: bool,
) -> Result<IdentityDocument, IdentityError> {
    let revision = wire.connect_revision.unwrap_or(0);
    if versioned && wire.connect_codec_version != Some(IDENTITY_CODEC_VERSION) {
        return Err(IdentityError::Corrupt);
    }
    let host = wire.host.unwrap_or_default();
    let web = host.web.unwrap_or_default();
    if let Some(devices) = wire
        .connect_identity
        .as_ref()
        .map(|identity| identity.devices.len())
    {
        if devices > MAX_IDENTITY_DEVICES as usize {
            return Err(IdentityError::LimitExceeded {
                field: IdentityLimitField::Devices,
            });
        }
    }
    let identity = wire
        .connect_identity
        .map(connect_identity_from_wire)
        .transpose()?;
    if identity.is_some() && wire.connect_requires_explicit_reestablish {
        return Err(IdentityError::Corrupt);
    }
    if let Some(identity) = &identity {
        identity.validate_structure()?;
    }
    let receipts = wire
        .connect_receipts
        .unwrap_or_default()
        .into_iter()
        .map(receipt_from_wire)
        .collect::<Result<Vec<_>, _>>()?;
    let pending_transition = wire
        .connect_pending_transition
        .map(pending_transition_from_wire)
        .transpose()?;
    if let Some(pending) = &pending_transition {
        validate_pending_transition(pending)?;
    }
    let pending_revocation = wire
        .connect_pending_revocation
        .map(revocation_journal_from_wire)
        .transpose()?;
    if pending_transition.is_some() && pending_revocation.is_some() {
        return Err(IdentityError::Corrupt);
    }
    let mut receipt_ids = BTreeSet::new();
    for receipt in &receipts {
        if !receipt_ids.insert(receipt.command_id()) {
            return Err(IdentityError::DuplicateReceipt);
        }
    }
    if let Some(version) = wire.connect_codec_version {
        if version != IDENTITY_CODEC_VERSION {
            return Err(IdentityError::Corrupt);
        }
    }
    if receipts.len() > MAX_IDENTITY_RECEIPTS {
        return Err(IdentityError::LimitExceeded {
            field: IdentityLimitField::ArrayItems,
        });
    }
    if receipts.iter().any(|receipt| receipt.revision > revision) {
        return Err(IdentityError::Corrupt);
    }
    let known_hosts = wire.known_hosts.unwrap_or_default();
    for value in &known_hosts {
        validate_known_host_value(value, 1)?;
    }
    Ok(IdentityDocument {
        revision,
        cas_epoch: wire.connect_cas_epoch.unwrap_or(0),
        connect_host_build: wire.connect_host_build,
        native_pairing_token: optional_pairing(host.pairing_token.as_deref())?,
        web_pairing_token: optional_pairing(web.pairing_token.as_deref())?,
        host_server_id: bounded_id(host.server_id)?,
        known_hosts,
        identity,
        receipts,
        pending_transition,
        pending_revocation,
        requires_explicit_reestablish: wire.connect_requires_explicit_reestablish,
    })
}

fn document_to_wire(document: &IdentityDocument) -> Result<WireDocument, IdentityError> {
    if document.known_hosts.len() > MAX_IDENTITY_ARRAY_ITEMS as usize {
        return Err(IdentityError::LimitExceeded {
            field: IdentityLimitField::ArrayItems,
        });
    }
    for value in &document.known_hosts {
        validate_known_host_value(value, 1)?;
    }
    if document.receipts.len() > MAX_IDENTITY_RECEIPTS {
        return Err(IdentityError::LimitExceeded {
            field: IdentityLimitField::ArrayItems,
        });
    }
    let mut receipt_ids = BTreeSet::new();
    for receipt in &document.receipts {
        if !receipt_ids.insert(receipt.command_id()) {
            return Err(IdentityError::DuplicateReceipt);
        }
    }
    if document
        .host_server_id
        .as_deref()
        .is_some_and(|server_id| server_id.is_empty())
    {
        return Err(IdentityError::Corrupt);
    }
    if document
        .host_server_id
        .as_deref()
        .is_some_and(|server_id| server_id.len() > MAX_ID_BYTES)
    {
        return Err(IdentityError::LimitExceeded {
            field: IdentityLimitField::Id,
        });
    }
    if document
        .host_server_id
        .as_deref()
        .is_some_and(|server_id| server_id.chars().any(char::is_control))
    {
        return Err(IdentityError::Corrupt);
    }
    if document.requires_explicit_reestablish && document.identity.is_some() {
        return Err(IdentityError::Corrupt);
    }
    if let Some(identity) = &document.identity {
        identity.validate_structure()?;
    }
    if let Some(pending) = &document.pending_transition {
        validate_pending_transition(pending)?;
    }
    if let Some(journal) = &document.pending_revocation {
        validate_revocation_journal(journal)?;
    }
    if document.pending_transition.is_some() && document.pending_revocation.is_some() {
        return Err(IdentityError::Corrupt);
    }
    Ok(WireDocument {
        host: Some(WireHost {
            pairing_token: document
                .native_pairing_token
                .as_ref()
                .map(|code| code.as_str().to_string()),
            server_id: document.host_server_id.clone(),
            web: Some(WireWeb {
                pairing_token: document
                    .web_pairing_token
                    .as_ref()
                    .map(|code| code.as_str().to_string()),
            }),
        }),
        known_hosts: Some(document.known_hosts.clone()),
        connect_host_build: document.connect_host_build,
        connect_codec_version: Some(IDENTITY_CODEC_VERSION),
        connect_revision: Some(document.revision),
        connect_cas_epoch: if document.cas_epoch == 0 {
            None
        } else {
            Some(document.cas_epoch)
        },
        connect_receipts: Some(
            document
                .receipts
                .iter()
                .map(receipt_to_wire)
                .collect::<Result<Vec<_>, _>>()?,
        ),
        connect_identity: document.identity.as_ref().map(connect_identity_to_wire),
        connect_pending_transition: document
            .pending_transition
            .as_ref()
            .map(pending_transition_to_wire),
        connect_pending_revocation: document
            .pending_revocation
            .as_ref()
            .map(revocation_journal_to_wire),
        connect_requires_explicit_reestablish: document.requires_explicit_reestablish,
    })
}

fn connect_identity_from_wire(wire: WireIdentity) -> Result<ConnectIdentity, IdentityError> {
    if wire.schema_version != CONNECT_IDENTITY_SCHEMA_VERSION {
        return Err(IdentityError::Corrupt);
    }
    let devices = wire
        .devices
        .into_iter()
        .map(device_from_wire)
        .collect::<Result<Vec<_>, _>>()?;
    let host_key = key_from_wire(wire.host_key, CredentialLocation::OsHostVault, true)?;
    Ok(ConnectIdentity {
        schema_version: wire.schema_version,
        host_public_id: HostPublicId::parse(&wire.host_public_id)?,
        host_key,
        pairing_code: PairingCode::parse_valid(&wire.pairing_code)?,
        pairing_code_generation: wire.pairing_code_generation,
        pairing_purpose: wire.pairing_purpose,
        profile_binding_hash: wire.profile_binding_hash,
        last_seen_host_build: wire.last_seen_host_build,
        created_at_epoch_ms: wire.created_at_epoch_ms,
        devices,
    })
}

fn connect_identity_to_wire(identity: &ConnectIdentity) -> WireIdentity {
    WireIdentity {
        schema_version: identity.schema_version,
        host_public_id: uuid_string(identity.host_public_id.as_bytes()),
        host_key: key_to_wire(&identity.host_key),
        pairing_code: identity.pairing_code.as_str().to_string(),
        pairing_code_generation: identity.pairing_code_generation,
        pairing_purpose: identity.pairing_purpose,
        profile_binding_hash: identity.profile_binding_hash.clone(),
        last_seen_host_build: identity.last_seen_host_build,
        created_at_epoch_ms: identity.created_at_epoch_ms,
        devices: identity.devices.iter().map(device_to_wire).collect(),
    }
}

fn key_from_wire(
    wire: WireKey,
    expected_location: CredentialLocation,
    require_generation: bool,
) -> Result<KeyReference, IdentityError> {
    if wire.location != expected_location {
        return Err(IdentityError::Corrupt);
    }
    let fingerprint = validate_fingerprint(&wire.fingerprint)?.to_string();
    if require_generation && wire.generation.unwrap_or(0) == 0 {
        return Err(IdentityError::Corrupt);
    }
    if !require_generation && wire.generation.is_some() {
        return Err(IdentityError::InvalidDevice);
    }
    Ok(KeyReference {
        location: wire.location,
        fingerprint,
        generation: wire.generation,
    })
}

fn key_to_wire(key: &KeyReference) -> WireKey {
    WireKey {
        location: key.location(),
        fingerprint: key.fingerprint().to_string(),
        generation: key.generation(),
    }
}

fn device_from_wire(wire: WireDevice) -> Result<DeviceRecord, IdentityError> {
    if let Some(label) = &wire.label {
        if label.len() > MAX_LABEL_BYTES {
            return Err(IdentityError::LimitExceeded {
                field: IdentityLimitField::Label,
            });
        }
    }
    let device = DeviceRecord {
        device_id: DeviceId::parse(&wire.device_id)?,
        kind: wire.kind,
        label: wire.label.unwrap_or_default(),
        legacy_client_id: bounded_device_id(wire.legacy_client_id)?,
        public_key: KeyReference {
            location: wire.public_key.location,
            fingerprint: validate_fingerprint(&wire.public_key.fingerprint)?.to_string(),
            generation: wire.public_key.generation,
        },
        revoked: wire.revoked,
        revoked_at_epoch_ms: wire.revoked_at_epoch_ms,
        requires_re_pair: wire.requires_re_pair,
        browser: wire.browser,
    };
    super::identity::validate_device_record(&device)?;
    Ok(device)
}

fn device_to_wire(device: &DeviceRecord) -> WireDevice {
    WireDevice {
        device_id: uuid_string(device.device_id.as_bytes()),
        kind: device.kind,
        label: Some(device.label.clone()),
        legacy_client_id: device.legacy_client_id.clone(),
        public_key: WireKey {
            location: device.public_key.location(),
            fingerprint: device.public_key.fingerprint().to_string(),
            generation: device.public_key.generation(),
        },
        revoked: device.revoked,
        revoked_at_epoch_ms: device.revoked_at_epoch_ms,
        requires_re_pair: device.requires_re_pair,
        browser: device.browser.clone(),
    }
}

fn receipt_from_wire(wire: WireReceipt) -> Result<IdentityReceipt, IdentityError> {
    if wire.kind != "identity" {
        return Err(IdentityError::Corrupt);
    }
    let command_digest = wire
        .command_digest
        .as_deref()
        .ok_or(IdentityError::Corrupt)
        .and_then(parse_digest)?;
    let setup = wire.setup.map(setup_from_wire).transpose()?;
    let registered_device = wire.registered_device.map(device_from_wire).transpose()?;
    let pairing_code = wire
        .pairing_code
        .as_deref()
        .map(PairingCode::parse_valid)
        .transpose()?;
    let host_rotation = wire
        .host_rotation
        .map(host_rotation_from_wire)
        .transpose()?;
    validate_receipt_payload(
        setup.as_ref(),
        registered_device.as_ref(),
        pairing_code.as_ref(),
        host_rotation.as_ref(),
    )?;
    Ok(IdentityReceipt {
        command_id: CommandId::parse(&wire.command_id).map_err(|_| IdentityError::Corrupt)?,
        revision: wire.revision,
        setup,
        registered_device,
        pairing_code,
        host_rotation,
        command_digest: Some(command_digest),
    })
}

fn receipt_to_wire(receipt: &IdentityReceipt) -> Result<WireReceipt, IdentityError> {
    validate_receipt_payload(
        receipt.setup.as_ref(),
        receipt.registered_device.as_ref(),
        receipt.pairing_code.as_ref(),
        receipt.host_rotation.as_ref(),
    )?;
    Ok(WireReceipt {
        command_id: receipt.command_id.to_string(),
        revision: receipt.revision,
        kind: "identity".to_string(),
        command_digest: receipt
            .command_digest
            .as_ref()
            .map(|digest| hex_encode(digest)),
        setup: receipt.setup.as_ref().map(setup_to_wire),
        registered_device: receipt.registered_device.as_ref().map(device_to_wire),
        pairing_code: receipt
            .pairing_code
            .as_ref()
            .map(|code| code.as_str().to_string()),
        host_rotation: receipt.host_rotation.as_ref().map(host_rotation_to_wire),
    })
}

fn setup_to_wire(setup: &IdentitySetup) -> WireSetup {
    WireSetup {
        host_public_id: uuid_string(setup.host_public_id.as_bytes()),
        host_key: key_to_wire(&setup.host_key),
        pairing_code: Some(setup.pairing_code.as_str().to_string()),
        pairing_purpose: setup.pairing_purpose,
        task_invite_id: setup.task_invite_id.clone(),
    }
}

fn setup_from_wire(wire: WireSetup) -> Result<IdentitySetup, IdentityError> {
    let host_public_id = HostPublicId::parse(&wire.host_public_id)?;
    let host_key = key_from_wire(wire.host_key, CredentialLocation::OsHostVault, true)?;
    let pairing_code = match wire.pairing_code.as_deref() {
        Some(raw) => PairingCode::parse_valid(raw)?,
        None => {
            return Err(IdentityError::Corrupt);
        }
    };
    if let Some(task_invite_id) = &wire.task_invite_id {
        validate_bounded_text(task_invite_id, MAX_ID_BYTES, IdentityLimitField::Id)?;
    }
    Ok(IdentitySetup {
        host_public_id,
        host_key,
        pairing_code,
        pairing_purpose: wire.pairing_purpose,
        task_invite_id: wire.task_invite_id,
    })
}

fn host_rotation_from_wire(wire: WireHostRotation) -> Result<HostIdentityRotation, IdentityError> {
    if wire.affected_device_count > MAX_IDENTITY_DEVICES as usize {
        return Err(IdentityError::LimitExceeded {
            field: IdentityLimitField::Devices,
        });
    }
    if !wire.all_devices_require_repair && wire.affected_device_count != 0 {
        return Err(IdentityError::Corrupt);
    }
    Ok(HostIdentityRotation {
        all_devices_require_repair: wire.all_devices_require_repair,
        affected_device_count: wire.affected_device_count,
    })
}

fn host_rotation_to_wire(rotation: &HostIdentityRotation) -> WireHostRotation {
    WireHostRotation {
        all_devices_require_repair: rotation.all_devices_require_repair,
        affected_device_count: rotation.affected_device_count,
    }
}

fn validate_receipt_payload(
    setup: Option<&IdentitySetup>,
    registered_device: Option<&DeviceRecord>,
    pairing_code: Option<&PairingCode>,
    host_rotation: Option<&HostIdentityRotation>,
) -> Result<(), IdentityError> {
    if setup.is_some() && registered_device.is_some()
        || setup.is_some() && host_rotation.is_some()
        || registered_device.is_some() && pairing_code.is_some()
        || registered_device.is_some() && host_rotation.is_some()
        || pairing_code.is_some() && host_rotation.is_some()
    {
        return Err(IdentityError::Corrupt);
    }
    if let Some(setup) = setup {
        validate_bounded_text(
            setup.pairing_code.as_str(),
            PAIRING_CODE_LEN,
            IdentityLimitField::PairingCode,
        )?;
        if setup.host_key.location() != CredentialLocation::OsHostVault
            || setup.host_key.generation().is_none()
        {
            return Err(IdentityError::Corrupt);
        }
        validate_fingerprint(setup.host_key.fingerprint())?;
        if pairing_code.is_some_and(|code| code.as_str() != setup.pairing_code.as_str()) {
            return Err(IdentityError::Corrupt);
        }
    }
    if let Some(device) = registered_device {
        super::identity::validate_device_record(device)?;
    }
    if let Some(code) = pairing_code {
        PairingCode::parse_valid(code.as_str())?;
    }
    if let Some(rotation) = host_rotation {
        if rotation.affected_device_count > MAX_IDENTITY_DEVICES as usize
            || (!rotation.all_devices_require_repair && rotation.affected_device_count != 0)
        {
            return Err(IdentityError::Corrupt);
        }
    }
    Ok(())
}

fn pending_transition_from_wire(
    wire: WirePendingTransition,
) -> Result<PendingIdentityTransition, IdentityError> {
    let kind = match wire.kind.as_str() {
        "enable" => PendingIdentityTransitionKind::Enable,
        "registerDevice" => PendingIdentityTransitionKind::RegisterDevice,
        "repairDevice" => PendingIdentityTransitionKind::RepairDevice,
        "rotateHostIdentity" => PendingIdentityTransitionKind::RotateHostIdentity,
        _ => return Err(IdentityError::Corrupt),
    };
    Ok(PendingIdentityTransition {
        command_id: CommandId::parse(&wire.command_id).map_err(|_| IdentityError::Corrupt)?,
        command_digest: parse_digest(&wire.command_digest)?,
        kind,
        transition_nonce: parse_nonce(&wire.transition_nonce)?,
        claim_owner: wire
            .claim_owner
            .as_deref()
            .map(parse_nonce)
            .transpose()?,
        claim_expires_at_epoch_ms: wire.claim_expires_at_epoch_ms,
        claim_logical_revision: wire.claim_logical_revision,
        host_public_id: wire
            .host_public_id
            .as_deref()
            .map(HostPublicId::parse)
            .transpose()?,
        device_id: wire.device_id.as_deref().map(DeviceId::parse).transpose()?,
        previous_identity: wire
            .previous_identity
            .map(connect_identity_from_wire)
            .transpose()?
            .map(Box::new),
    })
}

fn pending_transition_to_wire(pending: &PendingIdentityTransition) -> WirePendingTransition {
    WirePendingTransition {
        command_id: pending.command_id.to_string(),
        command_digest: hex_encode(&pending.command_digest),
        kind: match pending.kind {
            PendingIdentityTransitionKind::Enable => "enable",
            PendingIdentityTransitionKind::RegisterDevice => "registerDevice",
            PendingIdentityTransitionKind::RepairDevice => "repairDevice",
            PendingIdentityTransitionKind::RotateHostIdentity => "rotateHostIdentity",
        }
        .to_string(),
        transition_nonce: hex_encode(&pending.transition_nonce),
        claim_owner: pending.claim_owner.map(|owner| hex_encode(&owner)),
        claim_expires_at_epoch_ms: pending.claim_expires_at_epoch_ms,
        claim_logical_revision: pending.claim_logical_revision,
        host_public_id: pending.host_public_id.map(|id| uuid_string(id.as_bytes())),
        device_id: pending.device_id.map(|id| uuid_string(id.as_bytes())),
        previous_identity: pending
            .previous_identity
            .as_deref()
            .map(connect_identity_to_wire),
    }
}

fn validate_pending_transition(pending: &PendingIdentityTransition) -> Result<(), IdentityError> {
    if pending.command_digest == [0; 32] {
        return Err(IdentityError::Corrupt);
    }
    if pending.transition_nonce == [0; 16] {
        return Err(IdentityError::Corrupt);
    }
    if pending.claim_owner == Some([0; 16]) {
        return Err(IdentityError::Corrupt);
    }
    if pending.claim_owner.is_some()
        != (pending.claim_expires_at_epoch_ms.is_some()
            && pending.claim_logical_revision.is_some())
    {
        return Err(IdentityError::Corrupt);
    }
    if pending
        .claim_expires_at_epoch_ms
        .is_some_and(|expires| expires == 0)
    {
        return Err(IdentityError::Corrupt);
    }
    match pending.kind {
        PendingIdentityTransitionKind::Enable
            if pending.host_public_id.is_some()
                && pending.device_id.is_none()
                && pending.previous_identity.is_none() => {}
        PendingIdentityTransitionKind::RegisterDevice
            if pending.host_public_id.is_none()
                && pending.device_id.is_some()
                && pending.previous_identity.is_none() => {}
        PendingIdentityTransitionKind::RepairDevice
            if pending.host_public_id.is_none()
                && pending.device_id.is_some()
                && pending.previous_identity.is_some() =>
        {
            let Some(previous) = pending.previous_identity.as_deref() else {
                return Err(IdentityError::Corrupt);
            };
            previous.validate_structure()?;
            let device_id = pending.device_id.ok_or(IdentityError::Corrupt)?;
            let device = previous.device(device_id).ok_or(IdentityError::Corrupt)?;
            if device.revoked || !device.requires_re_pair {
                return Err(IdentityError::Corrupt);
            }
        }
        PendingIdentityTransitionKind::RotateHostIdentity
            if pending.host_public_id.is_none()
                && pending.device_id.is_none()
                && pending.previous_identity.is_some() =>
        {
            let Some(previous) = pending.previous_identity.as_deref() else {
                return Err(IdentityError::Corrupt);
            };
            previous.validate_structure()?;
        }
        _ => return Err(IdentityError::Corrupt),
    }
    Ok(())
}

fn revocation_journal_from_wire(
    wire: WireRevocationJournal,
) -> Result<PendingRevocationJournal, IdentityError> {
    let entries = wire
        .entries
        .into_iter()
        .map(|entry| {
            Ok((
                DeviceId::parse(&entry.device_id)?,
                entry.revocation_epoch,
            ))
        })
        .collect::<Result<Vec<_>, IdentityError>>()?;
    let journal = PendingRevocationJournal {
        command_id: CommandId::parse(&wire.command_id).map_err(|_| IdentityError::Corrupt)?,
        command_digest: parse_digest(&wire.command_digest)?,
        revoke_all: wire.revoke_all,
        entries,
    };
    validate_revocation_journal(&journal)?;
    Ok(journal)
}

fn revocation_journal_to_wire(journal: &PendingRevocationJournal) -> WireRevocationJournal {
    WireRevocationJournal {
        command_id: journal.command_id.to_string(),
        command_digest: hex_encode(&journal.command_digest),
        revoke_all: journal.revoke_all,
        entries: journal
            .entries
            .iter()
            .map(|(device_id, revocation_epoch)| WireRevocationEntry {
                device_id: uuid_string(device_id.as_bytes()),
                revocation_epoch: *revocation_epoch,
            })
            .collect(),
    }
}

fn validate_revocation_journal(
    journal: &PendingRevocationJournal,
) -> Result<(), IdentityError> {
    if journal.command_digest == [0; 32]
        || journal.entries.is_empty()
        || journal.entries.len() > MAX_IDENTITY_DEVICES as usize
    {
        return Err(IdentityError::Corrupt);
    }
    let mut ids = BTreeSet::new();
    for (device_id, _) in &journal.entries {
        if !ids.insert(*device_id) {
            return Err(IdentityError::DuplicateDevice);
        }
    }
    if !journal.revoke_all && journal.entries.len() != 1 {
        return Err(IdentityError::Corrupt);
    }
    Ok(())
}

fn validate_bounded_text(
    value: &str,
    max_bytes: usize,
    limit: IdentityLimitField,
) -> Result<(), IdentityError> {
    if value.is_empty() || value.len() > max_bytes {
        return Err(if value.is_empty() {
            IdentityError::Corrupt
        } else {
            IdentityError::LimitExceeded { field: limit }
        });
    }
    if value.chars().any(char::is_control) {
        return Err(IdentityError::Corrupt);
    }
    Ok(())
}

fn parse_digest(raw: &str) -> Result<[u8; 32], IdentityError> {
    if raw.len() != 64
        || !raw
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
    {
        return Err(IdentityError::Corrupt);
    }
    let mut digest = [0_u8; 32];
    for (index, slot) in digest.iter_mut().enumerate() {
        let high = raw.as_bytes()[index * 2];
        let low = raw.as_bytes()[index * 2 + 1];
        *slot = (hex_value(high)? << 4) | hex_value(low)?;
    }
    Ok(digest)
}

fn parse_nonce(raw: &str) -> Result<[u8; 16], IdentityError> {
    if raw.len() != 32
        || !raw
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
    {
        return Err(IdentityError::Corrupt);
    }
    let mut nonce = [0_u8; 16];
    for (index, slot) in nonce.iter_mut().enumerate() {
        *slot = (hex_value(raw.as_bytes()[index * 2])? << 4)
            | hex_value(raw.as_bytes()[index * 2 + 1])?;
    }
    if nonce == [0; 16] {
        return Err(IdentityError::Corrupt);
    }
    Ok(nonce)
}

fn hex_value(byte: u8) -> Result<u8, IdentityError> {
    match byte {
        b'0'..=b'9' => Ok(byte - b'0'),
        b'a'..=b'f' => Ok(byte - b'a' + 10),
        b'A'..=b'F' => Ok(byte - b'A' + 10),
        _ => Err(IdentityError::Corrupt),
    }
}

fn optional_pairing(raw: Option<&str>) -> Result<Option<PairingCode>, IdentityError> {
    match raw {
        None | Some("") => Ok(None),
        Some(raw) => PairingCode::parse_valid(raw).map(Some),
    }
}

fn bounded_id(raw: Option<String>) -> Result<Option<String>, IdentityError> {
    match raw {
        None => Ok(None),
        Some(value) if value.is_empty() => Ok(None),
        Some(value) if value.chars().any(char::is_control) => Err(IdentityError::Corrupt),
        Some(value) if value.len() > MAX_ID_BYTES => Err(IdentityError::LimitExceeded {
            field: IdentityLimitField::Id,
        }),
        Some(value) => Ok(Some(value)),
    }
}

fn bounded_device_id(raw: Option<String>) -> Result<Option<String>, IdentityError> {
    match raw {
        None => Ok(None),
        Some(value) if value.is_empty() => Err(IdentityError::InvalidDevice),
        Some(value) if value.chars().any(char::is_control) => Err(IdentityError::InvalidDevice),
        Some(value) if value.len() > MAX_ID_BYTES => Err(IdentityError::LimitExceeded {
            field: IdentityLimitField::Id,
        }),
        Some(value) => Ok(Some(value)),
    }
}

fn validate_known_host_value(value: &Value, depth: u32) -> Result<(), IdentityError> {
    if depth > MAX_IDENTITY_NESTING {
        return Err(IdentityError::LimitExceeded {
            field: IdentityLimitField::Nesting,
        });
    }
    match value {
        Value::Array(values) => {
            if values.len() > MAX_IDENTITY_ARRAY_ITEMS as usize {
                return Err(IdentityError::LimitExceeded {
                    field: IdentityLimitField::ArrayItems,
                });
            }
            for value in values {
                validate_known_host_value(value, depth + 1)?;
            }
        }
        Value::Object(fields) => {
            if fields.len() > MAX_IDENTITY_MAP_ENTRIES as usize {
                return Err(IdentityError::LimitExceeded {
                    field: IdentityLimitField::MapEntries,
                });
            }
            for (key, value) in fields {
                enforce_string_cap(None, key)?;
                validate_known_host_value(value, depth + 1)?;
            }
        }
        Value::String(value) => {
            if value.len() > MAX_ID_BYTES {
                return Err(IdentityError::LimitExceeded {
                    field: IdentityLimitField::Id,
                });
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
    Ok(())
}

fn uuid_string(bytes: &[u8; 16]) -> String {
    uuid::Uuid::from_bytes(*bytes).to_string()
}

pub(crate) fn scan_bounded_json(bytes: &[u8]) -> Result<(), IdentityError> {
    let mut parser = JsonScanner {
        input: bytes,
        pos: 0,
    };
    parser.parse_value(1, None)?;
    parser.skip_ws();
    if parser.pos != parser.input.len() {
        return Err(IdentityError::Corrupt);
    }
    Ok(())
}

struct JsonScanner<'a> {
    input: &'a [u8],
    pos: usize,
}

impl<'a> JsonScanner<'a> {
    fn parse_value(&mut self, depth: u32, parent_key: Option<&str>) -> Result<(), IdentityError> {
        if depth > MAX_IDENTITY_NESTING {
            return Err(IdentityError::LimitExceeded {
                field: IdentityLimitField::Nesting,
            });
        }
        self.skip_ws();
        match self.peek() {
            Some(b'{') => self.parse_object(depth, parent_key),
            Some(b'[') => self.parse_array(depth, parent_key),
            Some(b'"') => {
                self.parse_string(parent_key)?;
                Ok(())
            }
            Some(b't') => self.consume_literal(b"true"),
            Some(b'f') => self.consume_literal(b"false"),
            Some(b'n') => self.consume_literal(b"null"),
            Some(b'-') | Some(b'0'..=b'9') => self.parse_number(),
            _ => Err(IdentityError::Corrupt),
        }
    }

    fn parse_object(&mut self, depth: u32, parent_key: Option<&str>) -> Result<(), IdentityError> {
        self.expect(b'{')?;
        self.skip_ws();
        let mut keys = BTreeSet::new();
        let mut entries = 0_u32;
        let identity_object = parent_key == Some("connectIdentity");
        if self.peek() == Some(b'}') {
            self.pos += 1;
            return Ok(());
        }
        loop {
            entries = entries.checked_add(1).ok_or(IdentityError::Overflow)?;
            if entries > MAX_IDENTITY_MAP_ENTRIES {
                return Err(IdentityError::LimitExceeded {
                    field: IdentityLimitField::MapEntries,
                });
            }
            let key = self.parse_string(None)?;
            if !keys.insert(key.clone()) {
                return Err(IdentityError::DuplicateField);
            }
            if identity_object && !CONNECT_IDENTITY_KEYS.contains(&key.as_str()) {
                return Err(IdentityError::UnknownField);
            }
            self.skip_ws();
            self.expect(b':')?;
            self.parse_value(depth + 1, Some(&key))?;
            self.skip_ws();
            match self.peek() {
                Some(b',') => {
                    self.pos += 1;
                    self.skip_ws();
                }
                Some(b'}') => {
                    self.pos += 1;
                    break;
                }
                _ => return Err(IdentityError::Corrupt),
            }
        }
        Ok(())
    }

    fn parse_array(&mut self, depth: u32, parent_key: Option<&str>) -> Result<(), IdentityError> {
        self.expect(b'[')?;
        self.skip_ws();
        if self.peek() == Some(b']') {
            self.pos += 1;
            return Ok(());
        }
        let mut items = 0_u32;
        let device_array = parent_key == Some("devices");
        loop {
            items = items.checked_add(1).ok_or(IdentityError::Overflow)?;
            if device_array && items > MAX_IDENTITY_DEVICES {
                return Err(IdentityError::LimitExceeded {
                    field: IdentityLimitField::Devices,
                });
            }
            if !device_array && items > MAX_IDENTITY_ARRAY_ITEMS {
                return Err(IdentityError::LimitExceeded {
                    field: IdentityLimitField::ArrayItems,
                });
            }
            self.parse_value(depth + 1, parent_key)?;
            self.skip_ws();
            match self.peek() {
                Some(b',') => {
                    self.pos += 1;
                    self.skip_ws();
                }
                Some(b']') => {
                    self.pos += 1;
                    break;
                }
                _ => return Err(IdentityError::Corrupt),
            }
        }
        Ok(())
    }

    fn parse_string(&mut self, parent_key: Option<&str>) -> Result<String, IdentityError> {
        self.expect(b'"')?;
        let start = self.pos - 1;
        while let Some(byte) = self.peek() {
            match byte {
                b'"' => {
                    self.pos += 1;
                    let raw = &self.input[start..self.pos];
                    let raw_text = std::str::from_utf8(raw).map_err(|_| IdentityError::Corrupt)?;
                    enforce_raw_string_cap(parent_key, raw_text)?;
                    let decoded: String =
                        serde_json::from_slice(raw).map_err(|_| IdentityError::Corrupt)?;
                    enforce_string_cap(parent_key, &decoded)?;
                    return Ok(decoded);
                }
                b'\\' => {
                    self.pos += 1;
                    match self.peek() {
                        Some(b'u') => {
                            self.pos += 1;
                            for _ in 0..4 {
                                if !matches!(
                                    self.peek(),
                                    Some(b'0'..=b'9' | b'a'..=b'f' | b'A'..=b'F')
                                ) {
                                    return Err(IdentityError::Corrupt);
                                }
                                self.pos += 1;
                            }
                        }
                        Some(b'"' | b'\\' | b'/' | b'b' | b'f' | b'n' | b'r' | b't') => {
                            self.pos += 1;
                        }
                        _ => return Err(IdentityError::Corrupt),
                    }
                }
                byte if byte < 0x20 => return Err(IdentityError::Corrupt),
                _ => {
                    self.pos += 1;
                }
            }
        }
        Err(IdentityError::Corrupt)
    }

    fn parse_number(&mut self) -> Result<(), IdentityError> {
        if self.peek() == Some(b'-') {
            self.pos += 1;
        }
        let mut digits = 0;
        while matches!(self.peek(), Some(b'0'..=b'9')) {
            digits += 1;
            self.pos += 1;
        }
        if digits == 0 {
            return Err(IdentityError::Corrupt);
        }
        Ok(())
    }

    fn consume_literal(&mut self, literal: &[u8]) -> Result<(), IdentityError> {
        if self.input[self.pos..].starts_with(literal) {
            self.pos += literal.len();
            Ok(())
        } else {
            Err(IdentityError::Corrupt)
        }
    }

    fn expect(&mut self, byte: u8) -> Result<(), IdentityError> {
        if self.peek() == Some(byte) {
            self.pos += 1;
            Ok(())
        } else {
            Err(IdentityError::Corrupt)
        }
    }

    fn peek(&self) -> Option<u8> {
        self.input.get(self.pos).copied()
    }

    fn skip_ws(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\n' | b'\r' | b'\t')) {
            self.pos += 1;
        }
    }
}

fn enforce_string_cap(key: Option<&str>, raw: &str) -> Result<(), IdentityError> {
    let (field, cap) = string_cap(key);
    if raw.len() > cap {
        return Err(IdentityError::LimitExceeded { field });
    }
    Ok(())
}

fn enforce_raw_string_cap(key: Option<&str>, raw: &str) -> Result<(), IdentityError> {
    let (field, cap) = string_cap(key);
    let content_len = raw.len().saturating_sub(2);
    // A JSON code point escape is six bytes, so this bounds the raw escaped
    // representation before serde allocates the decoded string.
    if content_len > cap.saturating_mul(6) {
        return Err(IdentityError::LimitExceeded { field });
    }
    Ok(())
}

fn string_cap(key: Option<&str>) -> (IdentityLimitField, usize) {
    match key {
        None => (IdentityLimitField::Id, MAX_ID_BYTES),
        Some("pairingToken") | Some("pairingCode") => {
            (IdentityLimitField::PairingCode, PAIRING_CODE_LEN)
        }
        Some("fingerprint") => (IdentityLimitField::Fingerprint, MAX_FINGERPRINT_BYTES),
        Some("label") | Some("nickname") => (IdentityLimitField::Label, MAX_LABEL_BYTES),
        // Legacy host/web fields are intentionally ignored by the typed
        // decoder. Keep their representation bounded by the whole document
        // cap so certificate/private-key PEM values can be discarded without
        // rejecting a real legacy profile.
        Some(_) => (
            IdentityLimitField::PhysicalBytes,
            MAX_IDENTITY_PHYSICAL_BYTES,
        ),
    }
}

pub(crate) fn empty_receipt(command_id: CommandId, revision: u64) -> IdentityReceipt {
    IdentityReceipt {
        command_id,
        revision,
        setup: None,
        registered_device: None,
        pairing_code: None,
        host_rotation: None,
        command_digest: None,
    }
}

pub(crate) fn enable_receipt(
    command_id: CommandId,
    revision: u64,
    setup: IdentitySetup,
) -> IdentityReceipt {
    IdentityReceipt {
        command_id,
        revision,
        pairing_code: Some(setup.pairing_code.clone()),
        setup: Some(setup),
        registered_device: None,
        host_rotation: None,
        command_digest: None,
    }
}

pub(crate) fn device_receipt(
    command_id: CommandId,
    revision: u64,
    device: DeviceRecord,
) -> IdentityReceipt {
    IdentityReceipt {
        command_id,
        revision,
        setup: None,
        registered_device: Some(device),
        pairing_code: None,
        host_rotation: None,
        command_digest: None,
    }
}

pub(crate) fn pairing_receipt(
    command_id: CommandId,
    revision: u64,
    pairing_code: PairingCode,
) -> IdentityReceipt {
    IdentityReceipt {
        command_id,
        revision,
        setup: None,
        registered_device: None,
        pairing_code: Some(pairing_code),
        host_rotation: None,
        command_digest: None,
    }
}

pub(crate) fn host_rotation_receipt(
    command_id: CommandId,
    revision: u64,
    rotation: HostIdentityRotation,
) -> IdentityReceipt {
    IdentityReceipt {
        command_id,
        revision,
        setup: None,
        registered_device: None,
        pairing_code: None,
        host_rotation: Some(rotation),
        command_digest: None,
    }
}
