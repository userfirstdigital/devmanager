use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use sha2::{Digest, Sha256};

use crate::connect::{
    bind_device_credential, validate_device_credential, ActionId, BrowserDeviceDto,
    BrowserPrivateStorage, ConnectRole, CredentialLocation, CredentialVault, DeviceId,
    DeviceKeyProof, DeviceKind, HostKeyProof, IdentityCommand, IdentityError, IdentityLimitField,
    IdentityOp, IdentityPersistence, InMemoryIdentityPersistence, IsolatedRemoteStore,
    LoadedRemoteDocument, MachineBinding, PairingPurpose, PermissionDecision, PermissionEvaluator,
    PermissionRequest, RegisterDevice, RepairDevice, MAX_IDENTITY_ARRAY_ITEMS,
    MAX_IDENTITY_DEVICES, MAX_IDENTITY_MAP_ENTRIES, MAX_IDENTITY_NESTING,
    MAX_IDENTITY_PHYSICAL_BYTES, MAX_LABEL_BYTES,
};
use crate::domain::id::CommandId;

const LEGACY_REMOTE_JSON: &str =
    include_str!("../../tests/fixtures/connect/identity/legacy-remote.json");
const FIXTURE_NATIVE_PAIRING: &str = "H4K7M2NP";
const FIXTURE_WEB_PAIRING: &str = "X3Y8Z2QW";
const UPGRADE_BUILDS: [u32; 5] = [88, 91, 94, 100, 109];

struct Harness {
    store: IsolatedRemoteStore<InMemoryIdentityPersistence>,
    binding: MachineBinding,
    vault: FakeVault,
}

impl Harness {
    fn from_legacy() -> Self {
        let persistence = InMemoryIdentityPersistence::from_bytes(LEGACY_REMOTE_JSON.as_bytes())
            .expect("seed sanitized legacy remote.json");
        let store = IsolatedRemoteStore::new(persistence).expect("open isolated store");
        let binding = MachineBinding::new("fixture-machine-a");
        let vault = FakeVault::bind(&binding);
        Self {
            store,
            binding,
            vault,
        }
    }

    fn load(&mut self) -> LoadedRemoteDocument {
        self.store
            .load(&self.binding, &self.vault)
            .expect("load document")
    }

    fn execute(
        &mut self,
        expected_revision: u64,
        op: IdentityOp,
    ) -> crate::connect::IdentityReceipt {
        self.store
            .execute(
                &self.binding,
                &mut self.vault,
                IdentityCommand {
                    command_id: CommandId::new(),
                    expected_revision,
                    op,
                },
            )
            .expect("execute identity command")
    }
}

#[derive(Clone)]
struct HostSlot {
    host_id: crate::connect::HostPublicId,
    generation: u64,
    fingerprint: String,
}

#[derive(Clone)]
struct DeviceSlot {
    fingerprint: String,
    kind: DeviceKind,
}

struct FakeVault {
    secret: [u8; 32],
    host: Option<HostSlot>,
    pending_host: Option<HostSlot>,
    devices: BTreeMap<DeviceId, DeviceSlot>,
    pending_device_repairs: BTreeMap<DeviceId, DeviceSlot>,
    fail_next_commit: bool,
    constant_device_fingerprint: bool,
    invalid_host_proof: bool,
    fail_host_rollback: bool,
    fail_device_rollback: bool,
    fail_next_abort: bool,
}

impl FakeVault {
    fn bind(_binding: &MachineBinding) -> Self {
        let mut secret = [0_u8; 32];
        getrandom::fill(&mut secret).expect("test vault secret");
        Self {
            secret,
            host: None,
            pending_host: None,
            devices: BTreeMap::new(),
            pending_device_repairs: BTreeMap::new(),
            fail_next_commit: false,
            constant_device_fingerprint: false,
            invalid_host_proof: false,
            fail_host_rollback: false,
            fail_device_rollback: false,
            fail_next_abort: false,
        }
    }

    fn empty() -> Self {
        Self::bind(&MachineBinding::new("unused"))
    }

    fn snapshot(&self) -> Self {
        Self {
            secret: self.secret,
            host: self.host.clone(),
            pending_host: None,
            devices: self.devices.clone(),
            pending_device_repairs: BTreeMap::new(),
            fail_next_commit: self.fail_next_commit,
            constant_device_fingerprint: self.constant_device_fingerprint,
            invalid_host_proof: self.invalid_host_proof,
            fail_host_rollback: self.fail_host_rollback,
            fail_device_rollback: self.fail_device_rollback,
            fail_next_abort: self.fail_next_abort,
        }
    }

    fn fail_next_commit(&mut self) {
        self.fail_next_commit = true;
    }

    fn use_constant_device_fingerprint(&mut self) {
        self.constant_device_fingerprint = true;
    }

    fn use_invalid_host_proof(&mut self) {
        self.invalid_host_proof = true;
    }

    fn fail_host_rollback(&mut self) {
        self.fail_host_rollback = true;
    }

    fn fail_device_rollback(&mut self) {
        self.fail_device_rollback = true;
    }

    fn fail_next_abort(&mut self) {
        self.fail_next_abort = true;
    }

    fn fingerprint(&self, label: &[u8], generation: u64) -> String {
        let mut hasher = Sha256::new();
        hasher.update(self.secret);
        hasher.update(label);
        hasher.update(generation.to_be_bytes());
        hex_encode(&hasher.finalize())
    }
}

impl CredentialVault for FakeVault {
    fn establish_host(
        &mut self,
        host_id: crate::connect::HostPublicId,
    ) -> Result<HostKeyProof, IdentityError> {
        if self.invalid_host_proof {
            self.host = Some(HostSlot {
                host_id,
                generation: 1,
                fingerprint: String::new(),
            });
            return Ok(HostKeyProof::from_parts_for_test(host_id, 1, String::new()));
        }
        let fingerprint = self.fingerprint(host_id.as_bytes(), 1);
        self.host = Some(HostSlot {
            host_id,
            generation: 1,
            fingerprint: fingerprint.clone(),
        });
        Ok(HostKeyProof::from_parts_for_test(host_id, 1, fingerprint))
    }

    fn rollback_host_establishment(
        &mut self,
        host_id: crate::connect::HostPublicId,
        proof: &HostKeyProof,
    ) -> Result<(), IdentityError> {
        if self.fail_host_rollback {
            self.fail_host_rollback = false;
            return Err(IdentityError::PersistFailed);
        }
        if self.host.as_ref().is_some_and(|slot| {
            slot.host_id == host_id
                && slot.generation == proof.generation()
                && slot.fingerprint == proof.fingerprint()
        }) {
            self.host = None;
        }
        Ok(())
    }

    fn prepare_host_rotation(
        &mut self,
        host_id: crate::connect::HostPublicId,
    ) -> Result<HostKeyProof, IdentityError> {
        let generation = self
            .host
            .as_ref()
            .filter(|slot| slot.host_id == host_id)
            .map(|slot| {
                slot.generation
                    .checked_add(1)
                    .ok_or(IdentityError::Overflow)
            })
            .transpose()?
            .unwrap_or(1);
        let fingerprint = self.fingerprint(host_id.as_bytes(), generation);
        let slot = HostSlot {
            host_id,
            generation,
            fingerprint: fingerprint.clone(),
        };
        self.pending_host = Some(slot);
        Ok(HostKeyProof::from_parts_for_test(
            host_id,
            generation,
            fingerprint,
        ))
    }

    fn commit_host_rotation(&mut self) -> Result<(), IdentityError> {
        if self.fail_next_commit {
            self.fail_next_commit = false;
            return Err(IdentityError::PersistFailed);
        }
        if let Some(pending) = self.pending_host.take() {
            self.host = Some(pending);
        }
        Ok(())
    }

    fn abort_host_rotation(&mut self) -> Result<(), IdentityError> {
        if self.fail_next_abort {
            self.fail_next_abort = false;
            return Err(IdentityError::HostRotationCleanupFailed);
        }
        self.pending_host = None;
        Ok(())
    }

    fn discard_uncommitted_host(
        &mut self,
        host_id: crate::connect::HostPublicId,
    ) -> Result<(), IdentityError> {
        if self.fail_host_rollback {
            self.fail_host_rollback = false;
            return Err(IdentityError::PersistFailed);
        }
        if self
            .host
            .as_ref()
            .is_some_and(|slot| slot.host_id == host_id)
        {
            self.host = None;
        }
        Ok(())
    }

    fn discard_uncommitted_device(&mut self, device_id: DeviceId) -> Result<(), IdentityError> {
        if self.fail_device_rollback {
            self.fail_device_rollback = false;
            return Err(IdentityError::PersistFailed);
        }
        self.devices.remove(&device_id);
        Ok(())
    }

    fn verify_host(
        &self,
        host_id: crate::connect::HostPublicId,
        proof: &HostKeyProof,
    ) -> Result<(), IdentityError> {
        let slot = self
            .host
            .as_ref()
            .ok_or(IdentityError::MissingCredentialProof)?;
        if slot.host_id != host_id {
            return Err(IdentityError::MissingCredentialProof);
        }
        if proof.host_public_id() != host_id {
            return Err(IdentityError::MissingCredentialProof);
        }
        if slot.generation != proof.generation() {
            return Err(IdentityError::WrongCredentialGeneration);
        }
        if slot.fingerprint != proof.fingerprint() {
            return Err(IdentityError::MissingCredentialProof);
        }
        Ok(())
    }

    fn establish_device(
        &mut self,
        device_id: DeviceId,
        kind: DeviceKind,
    ) -> Result<DeviceKeyProof, IdentityError> {
        let fingerprint = if self.constant_device_fingerprint {
            self.fingerprint(b"constant-device", 1)
        } else {
            self.fingerprint(device_id.as_bytes(), 1)
        };
        self.devices.insert(
            device_id,
            DeviceSlot {
                fingerprint: fingerprint.clone(),
                kind,
            },
        );
        Ok(DeviceKeyProof::from_parts_for_test(
            device_id,
            kind,
            fingerprint,
        ))
    }

    fn rollback_device_establishment(
        &mut self,
        device_id: DeviceId,
        proof: &DeviceKeyProof,
    ) -> Result<(), IdentityError> {
        if self.fail_device_rollback {
            self.fail_device_rollback = false;
            return Err(IdentityError::PersistFailed);
        }
        if self.devices.get(&device_id).is_some_and(|slot| {
            slot.kind == proof.kind() && slot.fingerprint == proof.fingerprint()
        }) {
            self.devices.remove(&device_id);
        }
        Ok(())
    }

    fn verify_device(
        &self,
        device_id: DeviceId,
        proof: &DeviceKeyProof,
    ) -> Result<(), IdentityError> {
        let slot = self
            .devices
            .get(&device_id)
            .ok_or(IdentityError::MissingCredentialProof)?;
        if slot.kind != proof.kind() || slot.fingerprint != proof.fingerprint() {
            return Err(IdentityError::MissingCredentialProof);
        }
        if proof.device_id() != device_id {
            return Err(IdentityError::MissingCredentialProof);
        }
        Ok(())
    }

    fn prepare_device_repair(
        &mut self,
        device_id: DeviceId,
        kind: DeviceKind,
    ) -> Result<DeviceKeyProof, IdentityError> {
        let previous = self
            .devices
            .get(&device_id)
            .cloned()
            .ok_or(IdentityError::MissingCredentialProof)?;
        self.pending_device_repairs.insert(device_id, previous);
        let fingerprint = self.fingerprint(device_id.as_bytes(), 2);
        self.devices.insert(
            device_id,
            DeviceSlot {
                fingerprint: fingerprint.clone(),
                kind,
            },
        );
        Ok(DeviceKeyProof::from_parts_for_test(
            device_id,
            kind,
            fingerprint,
        ))
    }

    fn commit_device_repair(&mut self, device_id: DeviceId) -> Result<(), IdentityError> {
        if self.fail_next_commit {
            self.fail_next_commit = false;
            return Err(IdentityError::PersistFailed);
        }
        self.pending_device_repairs.remove(&device_id);
        Ok(())
    }

    fn device_repair_committed(
        &self,
        device_id: DeviceId,
        proof: &DeviceKeyProof,
    ) -> Result<bool, IdentityError> {
        if self.pending_device_repairs.contains_key(&device_id) {
            return Ok(false);
        }
        self.verify_device(device_id, proof).map(|_| true)
    }

    fn rollback_device_repair(
        &mut self,
        device_id: DeviceId,
        _proof: &DeviceKeyProof,
    ) -> Result<(), IdentityError> {
        if self.fail_device_rollback {
            self.fail_device_rollback = false;
            return Err(IdentityError::PersistFailed);
        }
        if let Some(previous) = self.pending_device_repairs.remove(&device_id) {
            self.devices.insert(device_id, previous);
        }
        Ok(())
    }

    fn abort_device_repair(&mut self, device_id: DeviceId) -> Result<(), IdentityError> {
        if let Some(previous) = self.pending_device_repairs.remove(&device_id) {
            self.devices.insert(device_id, previous);
        }
        Ok(())
    }
}

#[derive(Clone)]
struct ScriptedPersistence {
    inner: Arc<Mutex<ScriptedInner>>,
}

struct ScriptedInner {
    bytes: Option<Vec<u8>>,
    revision: u64,
    fail_on_write: Option<usize>,
    panic_on_write: Option<usize>,
    successful_writes: usize,
    claimed_len: Option<usize>,
}

impl std::fmt::Debug for ScriptedPersistence {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("ScriptedPersistence(redacted)")
    }
}

impl std::fmt::Debug for ScriptedInner {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ScriptedInner")
            .field("revision", &self.revision)
            .field("has_bytes", &self.bytes.is_some())
            .field("successful_writes", &self.successful_writes)
            .field("claimed_len", &self.claimed_len)
            .finish()
    }
}

impl ScriptedPersistence {
    fn new(bytes: &[u8]) -> Self {
        Self {
            inner: Arc::new(Mutex::new(ScriptedInner {
                bytes: Some(bytes.to_vec()),
                revision: 0,
                fail_on_write: None,
                panic_on_write: None,
                successful_writes: 0,
                claimed_len: None,
            })),
        }
    }

    fn fail_after_marker(&self) {
        let mut inner = self.inner.lock().expect("scripted lock");
        inner.fail_on_write = Some(inner.successful_writes + 2);
    }

    fn panic_after_marker(&self) {
        let mut inner = self.inner.lock().expect("scripted lock");
        inner.panic_on_write = Some(inner.successful_writes + 2);
    }

    fn fail_after_cas(&self) {
        let mut inner = self.inner.lock().expect("scripted lock");
        inner.fail_on_write = Some(inner.successful_writes + 3);
    }

    fn clear_faults(&self) {
        let mut inner = self.inner.lock().expect("scripted lock");
        inner.fail_on_write = None;
        inner.panic_on_write = None;
    }

    fn claim_len(&self, len: usize) {
        self.inner.lock().expect("scripted lock").claimed_len = Some(len);
    }

    fn snapshot_bytes(&self) -> Option<Vec<u8>> {
        self.inner.lock().expect("scripted lock").bytes.clone()
    }
}

impl IdentityPersistence for ScriptedPersistence {
    fn current_revision(&self) -> u64 {
        self.inner.lock().expect("scripted lock").revision
    }

    fn read_bounded(&self, max_bytes: usize) -> Result<Option<Vec<u8>>, IdentityError> {
        let inner = self.inner.lock().expect("scripted lock");
        if inner.claimed_len.unwrap_or(0) > max_bytes {
            return Err(IdentityError::LimitExceeded {
                field: IdentityLimitField::PhysicalBytes,
            });
        }
        match &inner.bytes {
            None => Ok(None),
            Some(bytes) if bytes.len() > max_bytes => Err(IdentityError::LimitExceeded {
                field: IdentityLimitField::PhysicalBytes,
            }),
            Some(bytes) => Ok(Some(bytes.clone())),
        }
    }

    fn compare_and_swap(
        &mut self,
        expected_revision: u64,
        bytes: &[u8],
    ) -> Result<u64, IdentityError> {
        let mut inner = self.inner.lock().expect("scripted lock");
        let write_number = inner.successful_writes + 1;
        if inner.panic_on_write == Some(write_number) {
            drop(inner);
            panic!("scripted crash after durable transition marker");
        }
        if inner.fail_on_write == Some(write_number) {
            inner.fail_on_write = None;
            return Err(IdentityError::PersistFailed);
        }
        if inner.revision != expected_revision {
            return Err(IdentityError::RevisionConflict);
        }
        let next_revision = inner
            .revision
            .checked_add(1)
            .ok_or(IdentityError::Overflow)?;
        inner.bytes = Some(bytes.to_vec());
        inner.revision = next_revision;
        inner.successful_writes = write_number;
        Ok(inner.revision)
    }

    fn replace_pending(
        &mut self,
        expected_revision: u64,
        bytes: &[u8],
    ) -> Result<u64, IdentityError> {
        let mut inner = self.inner.lock().expect("scripted lock");
        let write_number = inner.successful_writes + 1;
        if inner.panic_on_write == Some(write_number) {
            drop(inner);
            panic!("scripted crash while writing transition marker");
        }
        if inner.fail_on_write == Some(write_number) {
            inner.fail_on_write = None;
            return Err(IdentityError::PersistFailed);
        }
        if inner.revision != expected_revision {
            return Err(IdentityError::RevisionConflict);
        }
        let next_revision = inner
            .revision
            .checked_add(1)
            .ok_or(IdentityError::Overflow)?;
        inner.bytes = Some(bytes.to_vec());
        inner.revision = next_revision;
        inner.successful_writes = write_number;
        Ok(inner.revision)
    }
}

#[test]
fn load_preserves_legacy_pairing_codes_across_version_round_trips() {
    let mut harness = Harness::from_legacy();
    let document = harness.load();
    assert_eq!(
        document.native_pairing_token(),
        Some(FIXTURE_NATIVE_PAIRING)
    );
    assert_eq!(document.web_pairing_token(), Some(FIXTURE_WEB_PAIRING));
    assert!(document.identity().is_none());
    assert_eq!(document.revision(), 0);

    let mut revision = 0;
    for build in UPGRADE_BUILDS {
        harness.execute(revision, IdentityOp::NoteHostBuild { build });
        revision += 1;
        let document = harness.load();
        assert_eq!(document.last_seen_host_build(), Some(build));
        assert_eq!(
            document.native_pairing_token(),
            Some(FIXTURE_NATIVE_PAIRING)
        );
        assert_eq!(document.web_pairing_token(), Some(FIXTURE_WEB_PAIRING));
        assert!(document.identity().is_none());
        assert_eq!(document.revision(), revision);
    }

    let persisted = harness
        .store
        .persistence()
        .snapshot_bytes()
        .expect("persisted bytes");
    let persisted = String::from_utf8(persisted).unwrap();
    assert!(persisted.contains(FIXTURE_NATIVE_PAIRING));
    assert!(persisted.contains(FIXTURE_WEB_PAIRING));
    assert!(persisted.contains("fixture-host-server-id"));
    assert!(persisted.contains("knownHosts"));
    assert!(!persisted.contains("connectIdentity"));
    assert!(!persisted.contains("SANITIZED-NOT-A-PRIVATE-KEY"));
}

#[test]
fn explicit_enable_is_the_only_identity_creation_path() {
    let mut harness = Harness::from_legacy();
    let setup = harness.execute(
        0,
        IdentityOp::Enable {
            host_build: 100,
            now_epoch_ms: 1_700_000_004_000,
        },
    );
    let setup = setup.setup().expect("enable setup");
    assert_eq!(setup.pairing_code.as_str(), FIXTURE_WEB_PAIRING);
    assert_eq!(setup.pairing_purpose, PairingPurpose::OwnerDevice);
    assert!(setup.task_invite_id.is_none());
    assert_eq!(setup.host_key.location(), CredentialLocation::OsHostVault);
    assert_eq!(setup.host_key.fingerprint().len(), 64);

    let native = harness.execute(
        1,
        IdentityOp::RegisterDevice(RegisterDevice {
            kind: DeviceKind::Native,
            label: "Office desktop".to_string(),
            legacy_client_id: Some("fixture-native-client".to_string()),
            browser: None,
        }),
    );
    let browser = harness.execute(
        2,
        IdentityOp::RegisterDevice(RegisterDevice {
            kind: DeviceKind::Browser,
            label: "Safari iPhone".to_string(),
            legacy_client_id: Some("fixture-web-client".to_string()),
            browser: Some(BrowserDeviceDto {
                browser_install_id: "fixture-browser-install".to_string(),
                nickname: Some("Kitchen phone".to_string()),
                private_identity_storage: BrowserPrivateStorage::WebCryptoNonExportableIndexedDb,
                cleared_storage_requires_visible_repair: true,
            }),
        }),
    );

    let document = harness.load();
    let identity = document.identity().expect("identity after explicit setup");
    assert_eq!(identity.pairing_code().as_str(), FIXTURE_WEB_PAIRING);
    assert_eq!(identity.host_public_id(), setup.host_public_id);
    assert_eq!(
        identity.host_key().fingerprint(),
        setup.host_key.fingerprint()
    );
    let native_id = native.registered_device().unwrap().device_id;
    let browser_id = browser.registered_device().unwrap().device_id;
    assert_eq!(identity.device(native_id).unwrap().kind, DeviceKind::Native);
    let browser_record = identity.device(browser_id).unwrap();
    assert_eq!(browser_record.kind, DeviceKind::Browser);
    assert_eq!(
        browser_record.public_key.location(),
        CredentialLocation::BrowserWebCryptoCapability
    );
    assert_ne!(
        browser_record.public_key.fingerprint(),
        sha256_hex(&format!(
            "device-browser:{}",
            uuid::Uuid::from_bytes(*browser_id.as_bytes())
        ))
    );
    assert!(
        browser_record
            .browser
            .as_ref()
            .unwrap()
            .cleared_storage_requires_visible_repair
    );
}

#[test]
fn pairing_code_rotation_is_future_pairing_only() {
    let mut harness = enabled_with_two_devices();
    let original_host = harness.load().identity().unwrap().host_public_id();
    let device_ids = harness
        .load()
        .identity()
        .unwrap()
        .devices()
        .iter()
        .map(|device| device.device_id)
        .collect::<Vec<_>>();

    let rotated = harness.execute(
        3,
        IdentityOp::RotatePairingCode {
            now_epoch_ms: 1_700_000_005_000,
        },
    );
    let new_code = rotated
        .pairing_code()
        .expect("rotated code")
        .as_str()
        .to_string();
    assert_ne!(new_code, FIXTURE_WEB_PAIRING);

    let document = harness.load();
    assert_eq!(document.web_pairing_token(), Some(new_code.as_str()));
    assert_eq!(
        document.native_pairing_token(),
        Some(FIXTURE_NATIVE_PAIRING)
    );
    let identity = document.identity().unwrap();
    assert_eq!(identity.pairing_code().as_str(), new_code);
    assert_eq!(identity.host_public_id(), original_host);
    assert_eq!(identity.pairing_code_generation(), 2);
    for device_id in device_ids {
        assert!(!identity.device(device_id).unwrap().revoked);
    }
}

#[test]
fn host_identity_rotation_requires_durable_transition() {
    let mut harness = enabled_with_two_devices();
    let previous = harness
        .load()
        .identity()
        .unwrap()
        .host_key()
        .fingerprint()
        .to_string();
    let warning = harness
        .execute(
            3,
            IdentityOp::RotateHostIdentity {
                now_epoch_ms: 1_700_000_006_000,
            },
        )
        .host_rotation()
        .cloned()
        .expect("host rotation");
    assert!(warning.all_devices_require_repair);
    assert_eq!(warning.affected_device_count, 2);
    let document = harness.load();
    assert_ne!(
        document.identity().unwrap().host_key().fingerprint(),
        previous
    );
    assert_eq!(
        document.identity().unwrap().pairing_code().as_str(),
        FIXTURE_WEB_PAIRING
    );
    assert!(document
        .identity()
        .unwrap()
        .devices()
        .iter()
        .all(|device| device.requires_re_pair));
}

#[test]
fn single_and_all_device_revocation_leave_pairing_code_in_place() {
    let mut harness = enabled_with_two_devices();
    let ids = harness
        .load()
        .identity()
        .unwrap()
        .devices()
        .iter()
        .map(|device| device.device_id)
        .collect::<Vec<_>>();
    harness.execute(
        3,
        IdentityOp::RevokeDevice {
            device_id: ids[0],
            now_epoch_ms: 1_700_000_007_000,
        },
    );
    let document = harness.load();
    assert!(document.identity().unwrap().device(ids[0]).unwrap().revoked);
    assert!(!document.identity().unwrap().device(ids[1]).unwrap().revoked);
    assert_eq!(
        document.identity().unwrap().pairing_code().as_str(),
        FIXTURE_WEB_PAIRING
    );

    harness.execute(
        4,
        IdentityOp::RevokeAllDevices {
            now_epoch_ms: 1_700_000_008_000,
        },
    );
    let document = harness.load();
    assert!(document
        .identity()
        .unwrap()
        .devices()
        .iter()
        .all(|device| device.revoked));
    assert_eq!(
        document.identity().unwrap().pairing_code().as_str(),
        FIXTURE_WEB_PAIRING
    );
}

#[test]
fn copied_profile_is_rejected_without_adopting_identity() {
    let mut harness = enabled_with_two_devices();
    let foreign_binding = MachineBinding::new("fixture-machine-b");
    let foreign_vault = FakeVault::bind(&foreign_binding);
    let error = harness
        .store
        .load(&foreign_binding, &foreign_vault)
        .expect_err("copied profile");
    assert!(matches!(error, IdentityError::CopiedProfile));
    assert!(!error.to_string().contains(FIXTURE_WEB_PAIRING));
    assert!(!format!("{error:?}").contains("fixture-machine-a"));
    assert!(!format!("{error:?}").contains("fixture-machine-b"));
}

#[test]
fn corrupt_identity_recovers_without_minting_a_replacement() {
    let mut harness = enabled_with_two_devices();
    let original_host = harness.load().identity().unwrap().host_public_id();
    let bytes = harness.store.persistence().snapshot_bytes().unwrap();
    let text = String::from_utf8(bytes).unwrap();
    let corrupted = text.replace(
        harness.load().identity().unwrap().host_key().fingerprint(),
        "",
    );
    harness
        .store
        .persistence_mut()
        .replace_bytes_for_test(corrupted.into_bytes())
        .expect("inject corrupt bytes");

    let error = harness
        .store
        .load(&harness.binding, &harness.vault)
        .expect_err("corrupt identity");
    assert_eq!(error, IdentityError::Corrupt);

    let recovered = harness
        .store
        .recover_corrupt(&harness.binding, &harness.vault)
        .expect("recover");
    assert!(recovered.requires_explicit_reestablish());
    assert!(recovered.identity().is_none());
    assert_eq!(
        recovered.native_pairing_token(),
        Some(FIXTURE_NATIVE_PAIRING)
    );
    assert_eq!(recovered.web_pairing_token(), Some(FIXTURE_WEB_PAIRING));
    assert_ne!(recovered.host_public_id_if_any(), Some(original_host));
    assert!(harness.load().identity().is_none());
}

#[test]
fn recovery_does_not_clear_identity_when_vault_proof_is_missing() {
    let mut harness = enabled_with_two_devices();
    let original_host = harness.load().identity().unwrap().host_public_id();
    let error = harness
        .store
        .recover_corrupt(&harness.binding, &FakeVault::empty())
        .expect_err("missing vault proof is not corruption");
    assert_eq!(error, IdentityError::MissingCredentialProof);
    assert_eq!(
        harness.load().identity().unwrap().host_public_id(),
        original_host
    );
}

#[test]
fn malformed_profile_can_be_marked_for_explicit_reestablish() {
    let mut harness = enabled_with_two_devices();
    harness
        .store
        .persistence_mut()
        .replace_bytes_for_test(br#"{"#.to_vec())
        .expect("inject malformed bytes");
    assert_eq!(
        harness
            .store
            .load(&harness.binding, &harness.vault)
            .expect_err("malformed identity"),
        IdentityError::Corrupt
    );
    let recovered = harness
        .store
        .recover_corrupt(&harness.binding, &harness.vault)
        .expect("persist pending marker");
    assert!(recovered.requires_explicit_reestablish());
    assert!(recovered.identity().is_none());
}

#[test]
fn bounded_codec_rejects_oversize_duplicate_and_unknown_identity_fields() {
    let oversize = vec![b' '; MAX_IDENTITY_PHYSICAL_BYTES + 1];
    let cases: Vec<(&[u8], IdentityError)> = vec![
        (
            &oversize,
            IdentityError::LimitExceeded {
                field: IdentityLimitField::PhysicalBytes,
            },
        ),
        (
            br#"{"host":{"pairingToken":"H4K7M2NP","pairingToken":"X3Y8Z2QW"}}"#,
            IdentityError::DuplicateField,
        ),
        (
            br#"{"connectIdentity":{"unexpected":true}}"#,
            IdentityError::UnknownField,
        ),
        (
            br#"{"knownHosts":[{"name":1,"\u006eame":2}]}"#,
            IdentityError::DuplicateField,
        ),
    ];
    for (bytes, expected) in cases {
        let persistence = InMemoryIdentityPersistence::from_bytes(bytes).unwrap_or_else(|_| {
            InMemoryIdentityPersistence::from_unchecked_oversize(bytes.to_vec())
        });
        let mut store = IsolatedRemoteStore::new(persistence).expect("store");
        let binding = MachineBinding::new("fixture-machine-a");
        let vault = FakeVault::empty();
        let error = store.load(&binding, &vault).expect_err("bounded reject");
        assert_eq!(error, expected, "bytes={}", String::from_utf8_lossy(bytes));
    }

    let too_many_devices = oversize_device_document(MAX_IDENTITY_DEVICES + 1);
    let persistence = InMemoryIdentityPersistence::from_bytes(too_many_devices.as_bytes()).unwrap();
    let mut store = IsolatedRemoteStore::new(persistence).expect("store");
    let error = store
        .load(
            &MachineBinding::new("fixture-machine-a"),
            &FakeVault::empty(),
        )
        .expect_err("device cap");
    assert_eq!(
        error,
        IdentityError::LimitExceeded {
            field: IdentityLimitField::Devices
        }
    );

    let long_label = "A".repeat(MAX_LABEL_BYTES + 1);
    let labeled = format!(
        r#"{{"connectIdentity":{{"schemaVersion":1,"devices":[{{"deviceId":"01234567-89ab-7cde-8f01-23456789abcd","kind":"native","label":"{long_label}"}}]}}}}"#
    );
    let persistence = InMemoryIdentityPersistence::from_bytes(labeled.as_bytes()).unwrap();
    let mut store = IsolatedRemoteStore::new(persistence).expect("store");
    let error = store
        .load(
            &MachineBinding::new("fixture-machine-a"),
            &FakeVault::empty(),
        )
        .expect_err("label cap");
    assert_eq!(
        error,
        IdentityError::LimitExceeded {
            field: IdentityLimitField::Label
        }
    );
}

#[test]
fn bounded_codec_rejects_nesting_and_collection_caps() {
    let mut nested = String::from("0");
    for _ in 0..=MAX_IDENTITY_NESTING {
        nested = format!("[{nested}]");
    }
    let document = format!(r#"{{"knownHosts":{nested}}}"#);
    let persistence = InMemoryIdentityPersistence::from_bytes(document.as_bytes()).unwrap();
    let mut store = IsolatedRemoteStore::new(persistence).expect("store");
    let error = store
        .load(
            &MachineBinding::new("fixture-machine-a"),
            &FakeVault::empty(),
        )
        .expect_err("nesting cap");
    assert_eq!(
        error,
        IdentityError::LimitExceeded {
            field: IdentityLimitField::Nesting
        }
    );

    let entries = (0..=MAX_IDENTITY_MAP_ENTRIES)
        .map(|index| format!(r#""k{index}":1"#))
        .collect::<Vec<_>>()
        .join(",");
    let document = format!(r#"{{"host":{{{entries}}}}}"#);
    let persistence = InMemoryIdentityPersistence::from_bytes(document.as_bytes()).unwrap();
    let mut store = IsolatedRemoteStore::new(persistence).expect("store");
    let error = store
        .load(
            &MachineBinding::new("fixture-machine-a"),
            &FakeVault::empty(),
        )
        .expect_err("map cap");
    assert_eq!(
        error,
        IdentityError::LimitExceeded {
            field: IdentityLimitField::MapEntries
        }
    );

    let items = vec!["0"; MAX_IDENTITY_ARRAY_ITEMS as usize + 1].join(",");
    let document = format!(r#"{{"knownHosts":[{items}]}}"#);
    let persistence = InMemoryIdentityPersistence::from_bytes(document.as_bytes()).unwrap();
    let mut store = IsolatedRemoteStore::new(persistence).expect("store");
    let error = store
        .load(
            &MachineBinding::new("fixture-machine-a"),
            &FakeVault::empty(),
        )
        .expect_err("array cap");
    assert_eq!(
        error,
        IdentityError::LimitExceeded {
            field: IdentityLimitField::ArrayItems
        }
    );
}

#[test]
fn persistence_bounds_and_rejects_stale_cas() {
    let persistence = ScriptedPersistence::new(br#"{}"#);
    persistence.claim_len(1_000_000_000);
    let mut store = IsolatedRemoteStore::new(persistence).expect("store");
    let error = store
        .load(
            &MachineBinding::new("fixture-machine-a"),
            &FakeVault::empty(),
        )
        .expect_err("billion-length claim");
    assert_eq!(
        error,
        IdentityError::LimitExceeded {
            field: IdentityLimitField::PhysicalBytes
        }
    );

    let persistence = ScriptedPersistence::new(LEGACY_REMOTE_JSON.as_bytes());
    let concurrent = persistence.clone();
    let mut store = IsolatedRemoteStore::new(persistence).expect("store");
    let mut other = IsolatedRemoteStore::new(concurrent).expect("reopen");
    let binding = MachineBinding::new("fixture-machine-a");
    let mut vault = FakeVault::bind(&binding);
    other
        .execute(
            &binding,
            &mut vault,
            IdentityCommand {
                command_id: CommandId::new(),
                expected_revision: 0,
                op: IdentityOp::NoteHostBuild { build: 91 },
            },
        )
        .expect("first writer");
    let error = store
        .execute(
            &binding,
            &mut vault,
            IdentityCommand {
                command_id: CommandId::new(),
                expected_revision: 0,
                op: IdentityOp::NoteHostBuild { build: 92 },
            },
        )
        .expect_err("stale cas");
    assert_eq!(error, IdentityError::RevisionConflict);
}

#[test]
fn host_rotation_aborts_vault_when_save_fails() {
    let persistence = ScriptedPersistence::new(LEGACY_REMOTE_JSON.as_bytes());
    let mut store = IsolatedRemoteStore::new(persistence.clone()).expect("store");
    let binding = MachineBinding::new("fixture-machine-a");
    let mut vault = FakeVault::bind(&binding);
    store
        .execute(
            &binding,
            &mut vault,
            IdentityCommand {
                command_id: CommandId::new(),
                expected_revision: 0,
                op: IdentityOp::Enable {
                    host_build: 100,
                    now_epoch_ms: 1,
                },
            },
        )
        .unwrap();
    let original = store
        .load(&binding, &vault)
        .unwrap()
        .identity()
        .unwrap()
        .host_key()
        .fingerprint()
        .to_string();
    persistence.fail_after_marker();
    let error = store
        .execute(
            &binding,
            &mut vault,
            IdentityCommand {
                command_id: CommandId::new(),
                expected_revision: 1,
                op: IdentityOp::RotateHostIdentity { now_epoch_ms: 2 },
            },
        )
        .expect_err("save failure");
    assert_eq!(error, IdentityError::PersistFailed);
    let document = store.load(&binding, &vault).unwrap();
    assert_eq!(
        document.identity().unwrap().host_key().fingerprint(),
        original
    );
    assert!(vault.pending_host.is_none());
}

#[test]
fn enable_rolls_back_host_vault_when_save_fails() {
    let persistence = ScriptedPersistence::new(LEGACY_REMOTE_JSON.as_bytes());
    let mut store = IsolatedRemoteStore::new(persistence.clone()).expect("store");
    let binding = MachineBinding::new("fixture-machine-a");
    let mut vault = FakeVault::bind(&binding);
    persistence.fail_after_marker();

    let error = store
        .execute(
            &binding,
            &mut vault,
            IdentityCommand {
                command_id: CommandId::new(),
                expected_revision: 0,
                op: IdentityOp::Enable {
                    host_build: 100,
                    now_epoch_ms: 1,
                },
            },
        )
        .expect_err("save failure");

    assert_eq!(error, IdentityError::PersistFailed);
    assert!(vault.host.is_none(), "orphan host key after failed enable");
    assert!(store
        .load(&binding, &vault)
        .expect("load legacy")
        .identity()
        .is_none());
}

#[test]
fn failed_vault_rollback_persists_explicit_reestablish_state() {
    let persistence = ScriptedPersistence::new(LEGACY_REMOTE_JSON.as_bytes());
    let mut store = IsolatedRemoteStore::new(persistence.clone()).expect("store");
    let binding = MachineBinding::new("fixture-machine-a");
    let mut vault = FakeVault::bind(&binding);
    vault.fail_host_rollback();
    persistence.fail_after_marker();

    let error = store
        .execute(
            &binding,
            &mut vault,
            IdentityCommand {
                command_id: CommandId::new(),
                expected_revision: 0,
                op: IdentityOp::Enable {
                    host_build: 100,
                    now_epoch_ms: 1,
                },
            },
        )
        .expect_err("rollback failure");

    assert_eq!(error, IdentityError::TransitionRollbackFailed);
    let recovered = store.load(&binding, &vault).expect("pending state");
    assert!(recovered.identity().is_none());
    assert!(recovered.requires_explicit_reestablish());
}

#[test]
fn register_device_rolls_back_device_vault_when_save_fails() {
    let persistence = ScriptedPersistence::new(LEGACY_REMOTE_JSON.as_bytes());
    let mut store = IsolatedRemoteStore::new(persistence.clone()).expect("store");
    let binding = MachineBinding::new("fixture-machine-a");
    let mut vault = FakeVault::bind(&binding);
    store
        .execute(
            &binding,
            &mut vault,
            IdentityCommand {
                command_id: CommandId::new(),
                expected_revision: 0,
                op: IdentityOp::Enable {
                    host_build: 100,
                    now_epoch_ms: 1,
                },
            },
        )
        .unwrap();
    persistence.fail_after_marker();

    let error = store
        .execute(
            &binding,
            &mut vault,
            IdentityCommand {
                command_id: CommandId::new(),
                expected_revision: 1,
                op: IdentityOp::RegisterDevice(RegisterDevice {
                    kind: DeviceKind::Native,
                    label: "rollback me".to_string(),
                    legacy_client_id: Some("rollback-client".to_string()),
                    browser: None,
                }),
            },
        )
        .expect_err("save failure");

    assert_eq!(error, IdentityError::PersistFailed);
    assert!(
        vault.devices.is_empty(),
        "orphan device key after failed save"
    );
    assert_eq!(
        store
            .load(&binding, &vault)
            .unwrap()
            .identity()
            .unwrap()
            .devices()
            .len(),
        0
    );
}

#[test]
fn failed_device_vault_rollback_persists_explicit_reestablish_state() {
    let persistence = ScriptedPersistence::new(LEGACY_REMOTE_JSON.as_bytes());
    let mut store = IsolatedRemoteStore::new(persistence.clone()).expect("store");
    let binding = MachineBinding::new("fixture-machine-a");
    let mut vault = FakeVault::bind(&binding);
    store
        .execute(
            &binding,
            &mut vault,
            IdentityCommand {
                command_id: CommandId::new(),
                expected_revision: 0,
                op: IdentityOp::Enable {
                    host_build: 100,
                    now_epoch_ms: 1,
                },
            },
        )
        .unwrap();
    vault.fail_device_rollback();
    persistence.fail_after_marker();

    let error = store
        .execute(
            &binding,
            &mut vault,
            IdentityCommand {
                command_id: CommandId::new(),
                expected_revision: 1,
                op: IdentityOp::RegisterDevice(RegisterDevice {
                    kind: DeviceKind::Native,
                    label: "rollback me".to_string(),
                    legacy_client_id: Some("rollback-client".to_string()),
                    browser: None,
                }),
            },
        )
        .expect_err("rollback failure");

    assert_eq!(error, IdentityError::TransitionRollbackFailed);
    let recovered = store.load(&binding, &vault).expect("pending state");
    assert!(
        recovered.identity().is_some(),
        "host identity survives device rollback"
    );
    assert!(recovered.has_pending_transition());
    let persisted = String::from_utf8(persistence.snapshot_bytes().unwrap()).unwrap();
    assert!(persisted.contains("connectIdentity"));
}

#[test]
fn rotate_host_recovers_commit_after_identity_cas() {
    let mut harness = enabled_with_two_devices();
    let previous = harness
        .load()
        .identity()
        .unwrap()
        .host_key()
        .fingerprint()
        .to_string();
    let command = IdentityCommand {
        command_id: CommandId::new(),
        expected_revision: 3,
        op: IdentityOp::RotateHostIdentity { now_epoch_ms: 2 },
    };
    harness.vault.fail_next_commit();

    let error = harness
        .store
        .execute(&harness.binding, &mut harness.vault, command.clone())
        .expect_err("commit failure after identity CAS");

    assert_eq!(error, IdentityError::PersistFailed);
    assert!(
        harness.vault.pending_host.is_some(),
        "vault pending rotation must remain so retry can commit"
    );
    assert!(
        harness.load().has_pending_transition(),
        "identity CAS must keep the rotate pending marker"
    );

    let receipt = harness
        .store
        .execute(&harness.binding, &mut harness.vault, command)
        .expect("retry commits the already-CAS'd host rotation");
    assert!(receipt.host_rotation().is_some());
    let settled = harness.load();
    assert!(!settled.has_pending_transition());
    assert_ne!(
        settled.identity().unwrap().host_key().fingerprint(),
        previous
    );
    assert!(harness.vault.pending_host.is_none());
}

#[test]
fn paired_owner_allows_dangerous_only_with_bound_device_credential_proof() {
    let mut harness = enabled_with_two_devices();
    let identity = harness.load().identity().unwrap().clone();
    let device_id = identity.devices()[0].device_id;
    let proof = bind_device_credential(&identity, &harness.binding, &harness.vault, device_id, 7)
        .expect("current registered host-bound device");
    let decision = PermissionEvaluator::owner_only().evaluate(PermissionRequest {
        role: ConnectRole::PairedOwner,
        task_id: None,
        action: ActionId::APPROVE_DANGEROUS,
        credential: Some(proof),
    });
    assert_eq!(decision, PermissionDecision::Allow);

    harness.execute(
        3,
        IdentityOp::RevokeDevice {
            device_id,
            now_epoch_ms: 9,
        },
    );
    let revoked = harness.load().identity().unwrap().clone();
    assert_eq!(
        bind_device_credential(&revoked, &harness.binding, &harness.vault, device_id, 8),
        Err(IdentityError::UnknownDevice)
    );
    assert_eq!(
        bind_device_credential(
            &identity,
            &harness.binding,
            &harness.vault,
            DeviceId::new(),
            8,
        ),
        Err(IdentityError::UnknownDevice)
    );
}

#[test]
fn stale_credentials_are_rejected_by_authoritative_identity_validation() {
    let mut harness = enabled_with_two_devices();
    let identity = harness.load().identity().unwrap().clone();
    let device_id = identity.devices()[0].device_id;
    let proof = bind_device_credential(&identity, &harness.binding, &harness.vault, device_id, 7)
        .expect("current registered host-bound device");

    let mut foreign = enabled_with_two_devices();
    let foreign_identity = foreign.load().identity().unwrap().clone();
    assert_eq!(
        validate_device_credential(
            &foreign_identity,
            &foreign.binding,
            &foreign.vault,
            &proof,
            7,
        ),
        Err(IdentityError::WrongCredentialGeneration)
    );

    harness.execute(
        3,
        IdentityOp::RevokeDevice {
            device_id,
            now_epoch_ms: 8,
        },
    );
    let revoked = harness.load().identity().unwrap().clone();
    assert_eq!(
        validate_device_credential(&revoked, &harness.binding, &harness.vault, &proof, 7,),
        Err(IdentityError::UnknownDevice)
    );
    assert_eq!(
        harness
            .store
            .validate_device_credential(&harness.binding, &harness.vault, &proof, 7,),
        Err(IdentityError::UnknownDevice)
    );
    assert_eq!(
        PermissionEvaluator::owner_only().evaluate_with_authority(
            PermissionRequest {
                role: ConnectRole::PairedOwner,
                task_id: None,
                action: ActionId::APPROVE_DANGEROUS,
                credential: Some(proof),
            },
            &revoked,
            &harness.binding,
            &harness.vault,
            7,
        ),
        PermissionDecision::Denied(crate::connect::PermissionDenyReason::DeviceCredentialRequired)
    );

    let mut rotated_harness = enabled_with_two_devices();
    let rotated_identity = rotated_harness.load().identity().unwrap().clone();
    let rotated_device_id = rotated_identity.devices()[0].device_id;
    let rotated_proof = bind_device_credential(
        &rotated_identity,
        &rotated_harness.binding,
        &rotated_harness.vault,
        rotated_device_id,
        7,
    )
    .expect("rotated proof starts current");
    rotated_harness.execute(3, IdentityOp::RotateHostIdentity { now_epoch_ms: 9 });
    let rotated = rotated_harness.load().identity().unwrap().clone();
    assert!(
        validate_device_credential(
            &rotated,
            &rotated_harness.binding,
            &rotated_harness.vault,
            &rotated_proof,
            7,
        )
        .is_err(),
        "host rotation must invalidate the old proof"
    );
}

#[test]
fn host_rotation_requires_devices_to_re_pair_before_new_credentials() {
    let mut harness = enabled_with_two_devices();
    let device_id = harness.load().identity().unwrap().devices()[0].device_id;

    harness.execute(3, IdentityOp::RotateHostIdentity { now_epoch_ms: 9 });
    let rotated = harness.load().identity().unwrap().clone();

    assert!(rotated.device(device_id).unwrap().requires_re_pair);
    assert_eq!(
        bind_device_credential(&rotated, &harness.binding, &harness.vault, device_id, 10,),
        Err(IdentityError::UnknownDevice)
    );
}

#[test]
fn persisted_browser_registration_rejects_cleared_storage_repair_false() {
    let harness = enabled_with_two_devices();
    let persisted = String::from_utf8(
        harness
            .store
            .persistence()
            .snapshot_bytes()
            .expect("persisted identity"),
    )
    .unwrap();
    assert!(persisted.contains("\"clearedStorageRequiresVisibleRepair\":true"));
    let tampered = persisted.replace(
        "\"clearedStorageRequiresVisibleRepair\":true",
        "\"clearedStorageRequiresVisibleRepair\":false",
    );
    let persistence = InMemoryIdentityPersistence::from_bytes(tampered.as_bytes()).unwrap();
    let mut store = IsolatedRemoteStore::new(persistence).unwrap();
    let error = store
        .load(&harness.binding, &harness.vault)
        .expect_err("persisted browser must require visible repair");
    assert_eq!(error, IdentityError::InvalidDevice);
}

#[test]
fn unavailable_browser_credential_degrades_only_that_device() {
    let mut harness = enabled_with_two_devices();
    let browser_id = harness
        .load()
        .identity()
        .unwrap()
        .devices()
        .iter()
        .find(|device| device.kind == DeviceKind::Browser)
        .unwrap()
        .device_id;
    harness.vault.devices.remove(&browser_id);

    let loaded = harness
        .store
        .load(&harness.binding, &harness.vault)
        .expect("host load must survive unavailable browser credential");
    let identity = loaded.identity().expect("host identity remains available");
    assert!(identity.device(browser_id).unwrap().requires_re_pair);
    assert!(identity
        .devices()
        .iter()
        .filter(|device| device.device_id != browser_id)
        .all(|device| !device.requires_re_pair));
}

#[test]
fn existing_empty_or_truncated_identity_file_requires_explicit_recovery() {
    for bytes in [b"".as_slice(), br#"{"#.as_slice()] {
        let persistence = InMemoryIdentityPersistence::from_bytes(bytes).expect("bounded file");
        let mut store = IsolatedRemoteStore::new(persistence).expect("store");
        assert_eq!(
            store
                .load(
                    &MachineBinding::new("fixture-machine-a"),
                    &FakeVault::empty(),
                )
                .expect_err("existing malformed file must not look like first run"),
            IdentityError::Corrupt
        );
    }
}

#[test]
fn abort_host_rotation_failure_is_typed_and_retryable() {
    let persistence = ScriptedPersistence::new(LEGACY_REMOTE_JSON.as_bytes());
    let mut store = IsolatedRemoteStore::new(persistence.clone()).unwrap();
    let binding = MachineBinding::new("fixture-machine-a");
    let mut vault = FakeVault::bind(&binding);
    store
        .execute(
            &binding,
            &mut vault,
            IdentityCommand {
                command_id: CommandId::new(),
                expected_revision: 0,
                op: IdentityOp::Enable {
                    host_build: 100,
                    now_epoch_ms: 1,
                },
            },
        )
        .unwrap();
    store
        .execute(
            &binding,
            &mut vault,
            IdentityCommand {
                command_id: CommandId::new(),
                expected_revision: 1,
                op: IdentityOp::RegisterDevice(RegisterDevice {
                    kind: DeviceKind::Native,
                    label: "desktop".to_string(),
                    legacy_client_id: Some("native".to_string()),
                    browser: None,
                }),
            },
        )
        .unwrap();
    vault.fail_next_commit();
    let rotate = IdentityCommand {
        command_id: CommandId::new(),
        expected_revision: 2,
        op: IdentityOp::RotateHostIdentity { now_epoch_ms: 3 },
    };
    assert_eq!(
        store
            .execute(&binding, &mut vault, rotate.clone())
            .expect_err("commit failure"),
        IdentityError::PersistFailed
    );
    assert!(store
        .load(&binding, &vault)
        .unwrap()
        .has_pending_transition());
    let epoch_before_claim = persistence.current_revision();
    vault.fail_next_abort();
    assert_eq!(
        store
            .abandon_pending_transition(&binding, &mut vault)
            .expect_err("abort cleanup failure"),
        IdentityError::HostRotationCleanupFailed
    );
    assert!(
        persistence.current_revision() > epoch_before_claim,
        "cleanup claim must consume a pending CAS epoch before abort"
    );
    assert!(store
        .load(&binding, &vault)
        .unwrap()
        .has_pending_transition());
    let abandoned = store
        .abandon_pending_transition(&binding, &mut vault)
        .expect("retryable abort");
    assert!(!abandoned.has_pending_transition());
}

#[test]
fn abandoning_register_preserves_committed_host_identity() {
    let persistence = ScriptedPersistence::new(LEGACY_REMOTE_JSON.as_bytes());
    let mut store = IsolatedRemoteStore::new(persistence.clone()).unwrap();
    let binding = MachineBinding::new("fixture-machine-a");
    let mut vault = FakeVault::bind(&binding);
    store
        .execute(
            &binding,
            &mut vault,
            IdentityCommand {
                command_id: CommandId::new(),
                expected_revision: 0,
                op: IdentityOp::Enable {
                    host_build: 100,
                    now_epoch_ms: 1,
                },
            },
        )
        .unwrap();
    let host_id = store
        .load(&binding, &vault)
        .unwrap()
        .identity()
        .unwrap()
        .host_public_id();
    persistence.panic_after_marker();
    let crashed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        store.execute(
            &binding,
            &mut vault,
            IdentityCommand {
                command_id: CommandId::new(),
                expected_revision: 1,
                op: IdentityOp::RegisterDevice(RegisterDevice {
                    kind: DeviceKind::Native,
                    label: "pending desktop".to_string(),
                    legacy_client_id: Some("pending-desktop".to_string()),
                    browser: None,
                }),
            },
        )
    }));
    assert!(crashed.is_err());

    let persisted = persistence
        .read_bounded(MAX_IDENTITY_PHYSICAL_BYTES)
        .unwrap()
        .unwrap();
    let reopened_persistence = InMemoryIdentityPersistence::from_bytes(&persisted).unwrap();
    let mut reopened = IsolatedRemoteStore::new(reopened_persistence).unwrap();
    let abandoned = reopened
        .abandon_pending_transition(&binding, &mut vault)
        .expect("abandon pending register");
    assert_eq!(abandoned.identity().unwrap().host_public_id(), host_id);
    assert!(abandoned.identity().unwrap().devices().is_empty());
    assert!(!abandoned.requires_explicit_reestablish());
}

#[test]
fn abandoning_rotate_restores_exact_previous_identity_snapshot() {
    let mut harness = enabled_with_two_devices();
    let previous = harness.load().identity().unwrap().clone();
    harness.vault.fail_next_commit();
    let error = harness
        .store
        .execute(
            &harness.binding,
            &mut harness.vault,
            IdentityCommand {
                command_id: CommandId::new(),
                expected_revision: 3,
                op: IdentityOp::RotateHostIdentity { now_epoch_ms: 9 },
            },
        )
        .expect_err("commit failure leaves pending rotate");
    assert_eq!(error, IdentityError::PersistFailed);

    let abandoned = harness
        .store
        .abandon_pending_transition(&harness.binding, &mut harness.vault)
        .expect("abandon pending rotate");
    let restored = abandoned.identity().expect("previous identity remains");
    assert_eq!(restored.host_public_id(), previous.host_public_id());
    assert_eq!(
        restored.host_key().fingerprint(),
        previous.host_key().fingerprint()
    );
    assert_eq!(restored.devices(), previous.devices());
    assert!(!restored
        .devices()
        .iter()
        .any(|device| device.requires_re_pair));
}

#[test]
fn abandoning_pre_cas_rotate_aborts_uncommitted_new_host_slot() {
    let persistence = ScriptedPersistence::new(LEGACY_REMOTE_JSON.as_bytes());
    let mut store = IsolatedRemoteStore::new(persistence.clone()).unwrap();
    let binding = MachineBinding::new("fixture-machine-a");
    let mut vault = FakeVault::bind(&binding);
    store
        .execute(
            &binding,
            &mut vault,
            IdentityCommand {
                command_id: CommandId::new(),
                expected_revision: 0,
                op: IdentityOp::Enable {
                    host_build: 100,
                    now_epoch_ms: 1,
                },
            },
        )
        .unwrap();
    let previous = store
        .load(&binding, &vault)
        .unwrap()
        .identity()
        .unwrap()
        .clone();
    persistence.panic_after_marker();
    let crashed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        store.execute(
            &binding,
            &mut vault,
            IdentityCommand {
                command_id: CommandId::new(),
                expected_revision: 1,
                op: IdentityOp::RotateHostIdentity { now_epoch_ms: 2 },
            },
        )
    }));
    assert!(crashed.is_err());
    assert!(vault.pending_host.is_some());
    persistence.clear_faults();

    let abandoned = store
        .abandon_pending_transition(&binding, &mut vault)
        .expect("abandon pre-CAS rotate");
    assert_eq!(abandoned.identity(), Some(&previous));
    assert!(vault.pending_host.is_none());
    assert!(!abandoned.has_pending_transition());
}

#[test]
fn abandoning_already_committed_rotate_preserves_new_identity_snapshot() {
    let persistence = ScriptedPersistence::new(LEGACY_REMOTE_JSON.as_bytes());
    let mut store = IsolatedRemoteStore::new(persistence.clone()).unwrap();
    let binding = MachineBinding::new("fixture-machine-a");
    let mut vault = FakeVault::bind(&binding);
    store
        .execute(
            &binding,
            &mut vault,
            IdentityCommand {
                command_id: CommandId::new(),
                expected_revision: 0,
                op: IdentityOp::Enable {
                    host_build: 100,
                    now_epoch_ms: 1,
                },
            },
        )
        .unwrap();
    let old_host = store
        .load(&binding, &vault)
        .unwrap()
        .identity()
        .unwrap()
        .host_key()
        .fingerprint()
        .to_string();
    persistence.fail_after_cas();
    let error = store
        .execute(
            &binding,
            &mut vault,
            IdentityCommand {
                command_id: CommandId::new(),
                expected_revision: 1,
                op: IdentityOp::RotateHostIdentity { now_epoch_ms: 2 },
            },
        )
        .expect_err("clear failure leaves the committed rotate marker");
    assert_eq!(error, IdentityError::PersistFailed);
    let new_host = vault.host.as_ref().unwrap().fingerprint.clone();
    assert_ne!(new_host, old_host);

    let abandoned = store
        .abandon_pending_transition(&binding, &mut vault)
        .expect("abandon must recognize the already committed vault slot");
    assert_eq!(
        abandoned.identity().unwrap().host_key().fingerprint(),
        new_host
    );
    assert!(!abandoned.has_pending_transition());
}

#[test]
fn abandoning_already_committed_repair_preserves_new_device_snapshot() {
    let persistence = ScriptedPersistence::new(LEGACY_REMOTE_JSON.as_bytes());
    let mut store = IsolatedRemoteStore::new(persistence.clone()).unwrap();
    let binding = MachineBinding::new("fixture-machine-a");
    let mut vault = FakeVault::bind(&binding);
    store
        .execute(
            &binding,
            &mut vault,
            IdentityCommand {
                command_id: CommandId::new(),
                expected_revision: 0,
                op: IdentityOp::Enable {
                    host_build: 100,
                    now_epoch_ms: 1,
                },
            },
        )
        .unwrap();
    store
        .execute(
            &binding,
            &mut vault,
            IdentityCommand {
                command_id: CommandId::new(),
                expected_revision: 1,
                op: IdentityOp::RegisterDevice(RegisterDevice {
                    kind: DeviceKind::Native,
                    label: "desktop".to_string(),
                    legacy_client_id: Some("desktop".to_string()),
                    browser: None,
                }),
            },
        )
        .unwrap();
    store
        .execute(
            &binding,
            &mut vault,
            IdentityCommand {
                command_id: CommandId::new(),
                expected_revision: 2,
                op: IdentityOp::RotateHostIdentity { now_epoch_ms: 2 },
            },
        )
        .unwrap();
    let before_repair = store
        .load(&binding, &vault)
        .unwrap()
        .identity()
        .unwrap()
        .clone();
    let device = before_repair.devices()[0].clone();
    persistence.fail_after_cas();
    let error = store
        .execute(
            &binding,
            &mut vault,
            IdentityCommand {
                command_id: CommandId::new(),
                expected_revision: 3,
                op: IdentityOp::RepairDevice(RepairDevice {
                    device_id: device.device_id,
                    kind: device.kind,
                    label: device.label().to_string(),
                    legacy_client_id: device.legacy_client_id.clone(),
                    browser: device.browser.clone(),
                }),
            },
        )
        .expect_err("clear failure leaves the committed repair marker");
    assert_eq!(error, IdentityError::PersistFailed);
    let abandoned = store
        .abandon_pending_transition(&binding, &mut vault)
        .expect("abandon must recognize the already committed device slot");
    let repaired = abandoned
        .identity()
        .unwrap()
        .device(device.device_id)
        .unwrap();
    assert_ne!(
        repaired.public_key.fingerprint(),
        device.public_key.fingerprint()
    );
    assert!(!repaired.requires_re_pair);
    assert!(!abandoned.has_pending_transition());
}

#[test]
fn authenticated_repair_replaces_key_without_changing_device_id() {
    let mut harness = enabled_with_two_devices();
    let device = harness.load().identity().unwrap().devices()[0].clone();
    harness.execute(3, IdentityOp::RotateHostIdentity { now_epoch_ms: 9 });
    let repaired = harness.execute(
        4,
        IdentityOp::RepairDevice(RepairDevice {
            device_id: device.device_id,
            kind: device.kind,
            label: device.label().to_string(),
            legacy_client_id: device.legacy_client_id.clone(),
            browser: device.browser.clone(),
        }),
    );
    let repaired = repaired.registered_device().expect("repaired device");
    assert_eq!(repaired.device_id, device.device_id);
    assert_ne!(
        repaired.public_key.fingerprint(),
        device.public_key.fingerprint()
    );
    assert!(!repaired.requires_re_pair);
}

#[test]
fn abandoning_repair_restores_exact_previous_device_snapshot() {
    let mut harness = enabled_with_two_devices();
    harness.execute(3, IdentityOp::RotateHostIdentity { now_epoch_ms: 9 });
    let before_repair = harness.load().identity().unwrap().clone();
    let device = before_repair.devices()[0].clone();
    let repair = IdentityCommand {
        command_id: CommandId::new(),
        expected_revision: 4,
        op: IdentityOp::RepairDevice(RepairDevice {
            device_id: device.device_id,
            kind: device.kind,
            label: device.label().to_string(),
            legacy_client_id: device.legacy_client_id.clone(),
            browser: device.browser.clone(),
        }),
    };
    harness.vault.fail_next_commit();
    assert_eq!(
        harness
            .store
            .execute(&harness.binding, &mut harness.vault, repair)
            .expect_err("repair commit failure leaves a durable marker"),
        IdentityError::PersistFailed
    );

    let abandoned = harness
        .store
        .abandon_pending_transition(&harness.binding, &mut harness.vault)
        .expect("abandon repair");
    let restored = abandoned.identity().unwrap();
    assert_eq!(restored.devices(), before_repair.devices());
    assert_eq!(
        harness
            .vault
            .devices
            .get(&device.device_id)
            .unwrap()
            .fingerprint,
        device.public_key.fingerprint()
    );
    assert!(!abandoned.has_pending_transition());
}

#[test]
fn pending_transition_nonce_changes_and_is_persisted() {
    let persistence = ScriptedPersistence::new(LEGACY_REMOTE_JSON.as_bytes());
    persistence.panic_after_marker();
    let mut store = IsolatedRemoteStore::new(persistence.clone()).unwrap();
    let binding = MachineBinding::new("fixture-machine-a");
    let mut vault = FakeVault::bind(&binding);
    let command = IdentityCommand {
        command_id: CommandId::new(),
        expected_revision: 0,
        op: IdentityOp::Enable {
            host_build: 100,
            now_epoch_ms: 1,
        },
    };
    let crashed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        store.execute(&binding, &mut vault, command)
    }));
    assert!(crashed.is_err());
    let first = String::from_utf8(persistence.snapshot_bytes().unwrap()).unwrap();
    assert!(first.contains("transitionNonce"));
}

#[test]
fn stale_transition_nonce_cannot_claim_a_newer_marker() {
    let persistence = ScriptedPersistence::new(LEGACY_REMOTE_JSON.as_bytes());
    let mut store = IsolatedRemoteStore::new(persistence.clone()).unwrap();
    let binding = MachineBinding::new("fixture-machine-a");
    let mut vault = FakeVault::bind(&binding);
    persistence.panic_after_marker();
    let enable = IdentityCommand {
        command_id: CommandId::new(),
        expected_revision: 0,
        op: IdentityOp::Enable {
            host_build: 100,
            now_epoch_ms: 1,
        },
    };
    let crashed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        store.execute(&binding, &mut vault, enable)
    }));
    assert!(crashed.is_err());
    let pending_a = store
        .pending_transition_for_test()
        .unwrap()
        .expect("first marker");
    persistence.clear_faults();
    store
        .abandon_pending_transition(&binding, &mut vault)
        .expect("first marker abandoned");

    persistence.panic_after_marker();
    let crashed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        store.execute(
            &binding,
            &mut vault,
            IdentityCommand {
                command_id: CommandId::new(),
                expected_revision: 1,
                op: IdentityOp::Enable {
                    host_build: 101,
                    now_epoch_ms: 2,
                },
            },
        )
    }));
    assert!(crashed.is_err());
    let pending_b = store
        .pending_transition_for_test()
        .unwrap()
        .expect("second marker");
    assert_ne!(
        pending_a.transition_nonce, pending_b.transition_nonce,
        "each transition must use a fresh opaque nonce"
    );
    assert_eq!(
        store
            .claim_pending_transition_for_test(&pending_a)
            .expect_err("stale marker must not claim newer marker"),
        IdentityError::RevisionConflict
    );
}

#[test]
fn host_vault_retry_and_abandon_serialize_on_pending_cas() {
    let persistence = ScriptedPersistence::new(LEGACY_REMOTE_JSON.as_bytes());
    let mut store = IsolatedRemoteStore::new(persistence.clone()).unwrap();
    let binding = MachineBinding::new("fixture-machine-a");
    let mut vault = FakeVault::bind(&binding);
    store
        .execute(
            &binding,
            &mut vault,
            IdentityCommand {
                command_id: CommandId::new(),
                expected_revision: 0,
                op: IdentityOp::Enable {
                    host_build: 100,
                    now_epoch_ms: 1,
                },
            },
        )
        .unwrap();
    vault.fail_next_commit();
    let rotate = IdentityCommand {
        command_id: CommandId::new(),
        expected_revision: 1,
        op: IdentityOp::RotateHostIdentity { now_epoch_ms: 2 },
    };
    assert_eq!(
        store
            .execute(&binding, &mut vault, rotate.clone())
            .expect_err("commit failure"),
        IdentityError::PersistFailed
    );
    drop(store);
    let store_a = IsolatedRemoteStore::new(persistence.clone()).unwrap();
    let store_b = IsolatedRemoteStore::new(persistence.clone()).unwrap();
    let binding_a = binding.clone();
    let binding_b = binding.clone();
    let mut vault_a = vault;
    let mut vault_b = FakeVault::empty();
    let (tx_a, rx_a) = std::sync::mpsc::channel();
    let (tx_b, rx_b) = std::sync::mpsc::channel();
    std::thread::scope(|scope| {
        scope.spawn(move || {
            let mut store = store_a;
            tx_a.send(store.abandon_pending_transition(&binding_a, &mut vault_a))
                .ok();
        });
        scope.spawn(move || {
            let mut store = store_b;
            tx_b.send(store.abandon_pending_transition(&binding_b, &mut vault_b))
                .ok();
        });
    });
    let first = rx_a.recv().expect("first abandon");
    let second = rx_b.recv().expect("second abandon");
    let winners = [&first, &second]
        .iter()
        .filter(|result| result.is_ok())
        .count();
    assert_eq!(winners, 1, "exactly one pending CAS owner may settle");
    assert!(
        matches!(
            first,
            Err(IdentityError::RevisionConflict) | Err(IdentityError::TransitionPending)
        ) || matches!(
            second,
            Err(IdentityError::RevisionConflict) | Err(IdentityError::TransitionPending)
        ),
        "the loser must fail closed on the same pending CAS"
    );
}

#[test]
fn invalid_registration_metadata_is_rejected_before_vault_mutation() {
    let mut harness = Harness::from_legacy();
    harness.execute(
        0,
        IdentityOp::Enable {
            host_build: 100,
            now_epoch_ms: 1,
        },
    );
    let error = harness
        .store
        .execute(
            &harness.binding,
            &mut harness.vault,
            IdentityCommand {
                command_id: CommandId::new(),
                expected_revision: 1,
                op: IdentityOp::RegisterDevice(RegisterDevice {
                    kind: DeviceKind::Browser,
                    label: "browser".to_string(),
                    legacy_client_id: Some("legacy".to_string()),
                    browser: Some(BrowserDeviceDto {
                        browser_install_id: String::new(),
                        nickname: Some("nickname".to_string()),
                        private_identity_storage:
                            BrowserPrivateStorage::WebCryptoNonExportableIndexedDb,
                        cleared_storage_requires_visible_repair: true,
                    }),
                }),
            },
        )
        .expect_err("invalid browser install id");
    assert_eq!(error, IdentityError::InvalidDevice);
    assert!(harness.vault.devices.is_empty());
}

#[test]
fn invalid_host_proof_is_rejected_and_rolled_back_before_persist() {
    let mut harness = Harness::from_legacy();
    harness.vault.use_invalid_host_proof();
    let error = harness
        .store
        .execute(
            &harness.binding,
            &mut harness.vault,
            IdentityCommand {
                command_id: CommandId::new(),
                expected_revision: 0,
                op: IdentityOp::Enable {
                    host_build: 100,
                    now_epoch_ms: 1,
                },
            },
        )
        .expect_err("invalid host proof");
    assert_eq!(error, IdentityError::Corrupt);
    assert!(harness.vault.host.is_none());
    assert!(harness
        .store
        .load(&harness.binding, &harness.vault)
        .unwrap()
        .identity()
        .is_none());
}

#[test]
fn non_rotation_failure_does_not_attempt_host_rotation_abort() {
    let mut harness = Harness::from_legacy();
    harness.vault.use_invalid_host_proof();
    harness.vault.fail_next_abort();

    let error = harness
        .store
        .execute(
            &harness.binding,
            &mut harness.vault,
            IdentityCommand {
                command_id: CommandId::new(),
                expected_revision: 0,
                op: IdentityOp::Enable {
                    host_build: 100,
                    now_epoch_ms: 1,
                },
            },
        )
        .expect_err("invalid host proof");

    assert_eq!(error, IdentityError::Corrupt);
    assert!(harness.vault.host.is_none());
}

#[test]
fn duplicate_device_fingerprint_is_rejected() {
    let mut harness = Harness::from_legacy();
    harness.execute(
        0,
        IdentityOp::Enable {
            host_build: 100,
            now_epoch_ms: 1,
        },
    );
    harness.vault.use_constant_device_fingerprint();
    harness.execute(
        1,
        IdentityOp::RegisterDevice(RegisterDevice {
            kind: DeviceKind::Native,
            label: "one".to_string(),
            legacy_client_id: Some("one".to_string()),
            browser: None,
        }),
    );
    let error = harness
        .store
        .execute(
            &harness.binding,
            &mut harness.vault,
            IdentityCommand {
                command_id: CommandId::new(),
                expected_revision: 2,
                op: IdentityOp::RegisterDevice(RegisterDevice {
                    kind: DeviceKind::Native,
                    label: "two".to_string(),
                    legacy_client_id: Some("two".to_string()),
                    browser: None,
                }),
            },
        )
        .expect_err("duplicate fingerprint");
    assert_eq!(error, IdentityError::DuplicateDevice);
}

#[test]
fn labels_and_debug_persistence_are_preserved_and_redacted() {
    let mut harness = Harness::from_legacy();
    harness.execute(
        0,
        IdentityOp::Enable {
            host_build: 100,
            now_epoch_ms: 1,
        },
    );
    let receipt = harness.execute(
        1,
        IdentityOp::RegisterDevice(RegisterDevice {
            kind: DeviceKind::Native,
            label: "Office desktop".to_string(),
            legacy_client_id: Some("office".to_string()),
            browser: None,
        }),
    );
    let device_id = receipt.registered_device().unwrap().device_id;
    assert_eq!(
        harness
            .load()
            .identity()
            .unwrap()
            .device(device_id)
            .unwrap()
            .label(),
        "Office desktop"
    );
    let debug = format!("{:?}", harness.store);
    assert!(!debug.contains(FIXTURE_WEB_PAIRING));
    assert!(!debug.contains("bytes:"));
}

#[test]
fn load_requires_current_vault_proof_after_restart_or_wrong_generation() {
    let mut harness = enabled_with_two_devices();
    let empty = FakeVault::empty();
    let error = harness
        .store
        .load(&harness.binding, &empty)
        .expect_err("missing proof");
    assert_eq!(error, IdentityError::MissingCredentialProof);

    let stale = harness.vault.snapshot();
    harness.execute(3, IdentityOp::RotateHostIdentity { now_epoch_ms: 9 });
    let error = harness
        .store
        .load(&harness.binding, &stale)
        .expect_err("wrong generation");
    assert_eq!(error, IdentityError::WrongCredentialGeneration);
}

#[test]
fn repeated_command_id_requires_the_same_payload() {
    let mut harness = Harness::from_legacy();
    let command_id = CommandId::new();
    let command = IdentityCommand {
        command_id,
        expected_revision: 0,
        op: IdentityOp::NoteHostBuild { build: 91 },
    };
    let first = harness
        .store
        .execute(&harness.binding, &mut harness.vault, command.clone())
        .unwrap();
    let replay = harness
        .store
        .execute(&harness.binding, &mut harness.vault, command.clone())
        .unwrap();
    assert_eq!(replay.revision(), first.revision());
    let reopened_persistence = InMemoryIdentityPersistence::from_bytes(
        &harness.store.persistence().snapshot_bytes().unwrap(),
    )
    .unwrap();
    let mut reopened = IsolatedRemoteStore::new(reopened_persistence).unwrap();
    let mut reopened_vault = FakeVault::empty();
    let durable_replay = reopened
        .execute(&harness.binding, &mut reopened_vault, command.clone())
        .unwrap();
    assert_eq!(durable_replay.revision(), first.revision());
    let error = harness
        .store
        .execute(
            &harness.binding,
            &mut harness.vault,
            IdentityCommand {
                command_id,
                expected_revision: 99,
                op: IdentityOp::NoteHostBuild { build: 109 },
            },
        )
        .expect_err("conflicting command payload");
    assert_eq!(error, IdentityError::CommandConflict);
    assert_eq!(first.revision(), 1);
    assert_eq!(harness.load().last_seen_host_build(), Some(91));
}

#[test]
fn repeated_command_replays_result_payload_after_reopen() {
    let mut harness = Harness::from_legacy();
    let enable_command = IdentityCommand {
        command_id: CommandId::new(),
        expected_revision: 0,
        op: IdentityOp::Enable {
            host_build: 100,
            now_epoch_ms: 1,
        },
    };
    let first_setup = harness
        .store
        .execute(&harness.binding, &mut harness.vault, enable_command.clone())
        .unwrap();
    let setup = first_setup.setup().unwrap();
    let persisted = harness.store.persistence().snapshot_bytes().unwrap();
    let reopened_persistence = InMemoryIdentityPersistence::from_bytes(&persisted).unwrap();
    let mut reopened = IsolatedRemoteStore::new(reopened_persistence).unwrap();
    let mut reopened_vault = harness.vault.snapshot();
    let replayed_setup = reopened
        .execute(&harness.binding, &mut reopened_vault, enable_command)
        .unwrap();
    let replayed = replayed_setup.setup().unwrap();
    assert_eq!(replayed.host_public_id, setup.host_public_id);
    assert_eq!(
        replayed.host_key.fingerprint(),
        setup.host_key.fingerprint()
    );
    assert_eq!(replayed.pairing_code.as_str(), setup.pairing_code.as_str());

    let device_command = IdentityCommand {
        command_id: CommandId::new(),
        expected_revision: 1,
        op: IdentityOp::RegisterDevice(RegisterDevice {
            kind: DeviceKind::Native,
            label: "reopen device".to_string(),
            legacy_client_id: Some("reopen-client".to_string()),
            browser: None,
        }),
    };
    let first_device_receipt = harness
        .store
        .execute(&harness.binding, &mut harness.vault, device_command.clone())
        .unwrap();
    let first_device = first_device_receipt.registered_device().unwrap();
    let persisted = harness.store.persistence().snapshot_bytes().unwrap();
    let reopened_persistence = InMemoryIdentityPersistence::from_bytes(&persisted).unwrap();
    let mut reopened = IsolatedRemoteStore::new(reopened_persistence).unwrap();
    let mut reopened_vault = harness.vault.snapshot();
    let replayed_device = reopened
        .execute(&harness.binding, &mut reopened_vault, device_command)
        .unwrap()
        .registered_device()
        .cloned()
        .unwrap();
    assert_eq!(replayed_device.device_id, first_device.device_id);
    assert_eq!(replayed_device.label(), first_device.label());
    assert_eq!(
        replayed_device.public_key.fingerprint(),
        first_device.public_key.fingerprint()
    );

    let pairing_command = IdentityCommand {
        command_id: CommandId::new(),
        expected_revision: 2,
        op: IdentityOp::RotatePairingCode { now_epoch_ms: 3 },
    };
    let first_pairing = harness
        .store
        .execute(
            &harness.binding,
            &mut harness.vault,
            pairing_command.clone(),
        )
        .unwrap();
    let first_pairing_code = first_pairing.pairing_code().unwrap().as_str().to_string();
    let persisted = harness.store.persistence().snapshot_bytes().unwrap();
    let reopened_persistence = InMemoryIdentityPersistence::from_bytes(&persisted).unwrap();
    let mut reopened = IsolatedRemoteStore::new(reopened_persistence).unwrap();
    let mut reopened_vault = harness.vault.snapshot();
    let replayed_pairing = reopened
        .execute(&harness.binding, &mut reopened_vault, pairing_command)
        .unwrap();
    assert_eq!(
        replayed_pairing.pairing_code().unwrap().as_str(),
        first_pairing_code.as_str()
    );
}

#[test]
fn enable_receipt_replay_keeps_original_pairing_after_later_rotation() {
    let mut harness = Harness::from_legacy();
    let enable_command = IdentityCommand {
        command_id: CommandId::new(),
        expected_revision: 0,
        op: IdentityOp::Enable {
            host_build: 100,
            now_epoch_ms: 1,
        },
    };
    let first = harness
        .store
        .execute(&harness.binding, &mut harness.vault, enable_command.clone())
        .unwrap();
    let original = first.setup().unwrap().pairing_code.as_str().to_string();
    harness.execute(1, IdentityOp::RotatePairingCode { now_epoch_ms: 2 });
    let current = harness
        .load()
        .identity()
        .unwrap()
        .pairing_code()
        .as_str()
        .to_string();
    assert_ne!(current, original);

    let replayed = harness
        .store
        .execute(&harness.binding, &mut harness.vault, enable_command)
        .unwrap();
    assert_eq!(
        replayed.setup().unwrap().pairing_code.as_str(),
        original.as_str()
    );
    assert_eq!(replayed.pairing_code().unwrap().as_str(), original.as_str());
}

#[test]
fn in_memory_cas_is_atomic_on_overflow_and_rejects_aba() {
    let mut persistence = InMemoryIdentityPersistence::default();
    persistence
        .replace_pending(0, b"before")
        .expect("seed bytes");
    persistence.set_revision_for_test(u64::MAX);
    let before = persistence.snapshot_bytes();
    assert_eq!(
        persistence.compare_and_swap(u64::MAX, b"after"),
        Err(IdentityError::Overflow)
    );
    assert_eq!(persistence.snapshot_bytes(), before);

    let mut persistence = InMemoryIdentityPersistence::default();
    persistence.compare_and_swap(0, b"A").unwrap();
    persistence.compare_and_swap(1, b"B").unwrap();
    assert_eq!(
        persistence.compare_and_swap(0, b"A"),
        Err(IdentityError::RevisionConflict)
    );

    let persistence = ScriptedPersistence::new(br#"{}"#);
    let mut first_writer = persistence.clone();
    let mut stale_writer = persistence.clone();
    first_writer.replace_pending(0, b"marker").unwrap();
    assert_eq!(
        stale_writer.replace_pending(0, b"stale"),
        Err(IdentityError::RevisionConflict)
    );
}

#[test]
fn crash_after_vault_establish_reopens_as_pending_not_committed() {
    let persistence = ScriptedPersistence::new(LEGACY_REMOTE_JSON.as_bytes());
    persistence.panic_after_marker();
    let mut store = IsolatedRemoteStore::new(persistence.clone()).unwrap();
    let binding = MachineBinding::new("fixture-machine-a");
    let mut vault = FakeVault::bind(&binding);
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        store.execute(
            &binding,
            &mut vault,
            IdentityCommand {
                command_id: CommandId::new(),
                expected_revision: 0,
                op: IdentityOp::Enable {
                    host_build: 100,
                    now_epoch_ms: 1,
                },
            },
        )
    }));
    assert!(result.is_err(), "scripted crash must interrupt before CAS");
    assert!(vault.host.is_some(), "vault mutation occurred after marker");

    let persisted = persistence
        .read_bounded(MAX_IDENTITY_PHYSICAL_BYTES)
        .unwrap()
        .unwrap();
    let reopened_persistence = InMemoryIdentityPersistence::from_bytes(&persisted).unwrap();
    let mut reopened = IsolatedRemoteStore::new(reopened_persistence).unwrap();
    let loaded = reopened
        .load(&binding, &FakeVault::empty())
        .expect("pending state is readable without trusting a vault proof");
    assert!(loaded.has_pending_transition());
    assert!(loaded.identity().is_none());
    let abandoned = reopened
        .abandon_pending_transition(&binding, &mut vault)
        .expect("explicit pending recovery");
    assert!(!abandoned.has_pending_transition());
    assert!(abandoned.requires_explicit_reestablish());
    assert!(abandoned.identity().is_none());
    let mut recovery_vault = FakeVault::empty();
    reopened
        .execute(
            &binding,
            &mut recovery_vault,
            IdentityCommand {
                command_id: CommandId::new(),
                expected_revision: 1,
                op: IdentityOp::Enable {
                    host_build: 100,
                    now_epoch_ms: 2,
                },
            },
        )
        .expect("explicit setup after pending recovery");
}

#[test]
fn abandon_pending_enable_rolls_back_already_created_host_slot() {
    let persistence = ScriptedPersistence::new(LEGACY_REMOTE_JSON.as_bytes());
    persistence.panic_after_marker();
    let mut store = IsolatedRemoteStore::new(persistence.clone()).unwrap();
    let binding = MachineBinding::new("fixture-machine-a");
    let mut vault = FakeVault::bind(&binding);
    let crashed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        store.execute(
            &binding,
            &mut vault,
            IdentityCommand {
                command_id: CommandId::new(),
                expected_revision: 0,
                op: IdentityOp::Enable {
                    host_build: 100,
                    now_epoch_ms: 1,
                },
            },
        )
    }));
    assert!(crashed.is_err(), "scripted crash must interrupt before CAS");
    assert!(vault.host.is_some(), "vault mutation occurred after marker");

    let persisted = persistence
        .read_bounded(MAX_IDENTITY_PHYSICAL_BYTES)
        .unwrap()
        .unwrap();
    let reopened_persistence = InMemoryIdentityPersistence::from_bytes(&persisted).unwrap();
    let mut reopened = IsolatedRemoteStore::new(reopened_persistence).unwrap();
    let abandoned = reopened
        .abandon_pending_transition(&binding, &mut vault)
        .expect("explicit pending recovery");
    assert!(!abandoned.has_pending_transition());
    assert!(abandoned.requires_explicit_reestablish());
    assert!(
        vault.host.is_none(),
        "abandon must rollback the already-created host slot"
    );
}

#[test]
fn pending_marker_cas_epoch_survives_reopen() {
    let persistence = ScriptedPersistence::new(LEGACY_REMOTE_JSON.as_bytes());
    persistence.panic_after_marker();
    let mut store = IsolatedRemoteStore::new(persistence.clone()).unwrap();
    let binding = MachineBinding::new("fixture-machine-a");
    let mut vault = FakeVault::bind(&binding);
    let crashed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        store.execute(
            &binding,
            &mut vault,
            IdentityCommand {
                command_id: CommandId::new(),
                expected_revision: 0,
                op: IdentityOp::Enable {
                    host_build: 100,
                    now_epoch_ms: 1,
                },
            },
        )
    }));
    assert!(crashed.is_err(), "scripted crash must interrupt before CAS");

    let persisted = persistence
        .read_bounded(MAX_IDENTITY_PHYSICAL_BYTES)
        .unwrap()
        .unwrap();
    let mut reopened = InMemoryIdentityPersistence::from_bytes(&persisted).unwrap();
    assert_ne!(
        reopened.current_revision(),
        0,
        "physical CAS epoch must survive reopen"
    );
    assert_eq!(
        reopened.compare_and_swap(0, b"stale"),
        Err(IdentityError::RevisionConflict)
    );
}

#[test]
fn enable_retries_same_command_after_crash_between_establish_and_cas() {
    let persistence = ScriptedPersistence::new(LEGACY_REMOTE_JSON.as_bytes());
    persistence.panic_after_marker();
    let mut store = IsolatedRemoteStore::new(persistence.clone()).unwrap();
    let binding = MachineBinding::new("fixture-machine-a");
    let mut vault = FakeVault::bind(&binding);
    let command = IdentityCommand {
        command_id: CommandId::new(),
        expected_revision: 0,
        op: IdentityOp::Enable {
            host_build: 100,
            now_epoch_ms: 1,
        },
    };
    let crashed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        store.execute(&binding, &mut vault, command.clone())
    }));
    assert!(crashed.is_err(), "scripted crash must interrupt before CAS");
    let established = vault.host.clone().expect("host slot after establish");

    let persisted = persistence
        .read_bounded(MAX_IDENTITY_PHYSICAL_BYTES)
        .unwrap()
        .unwrap();
    let reopened_persistence = InMemoryIdentityPersistence::from_bytes(&persisted).unwrap();
    let mut reopened = IsolatedRemoteStore::new(reopened_persistence).unwrap();
    let receipt = reopened
        .execute(&binding, &mut vault, command)
        .expect("matching retry settles the durable pending enable");
    let setup = receipt.setup().expect("enable setup");
    assert_eq!(setup.host_public_id, established.host_id);
    let loaded = reopened.load(&binding, &vault).expect("settled load");
    assert!(!loaded.has_pending_transition());
    assert_eq!(
        loaded
            .identity()
            .expect("settled identity")
            .host_public_id(),
        established.host_id
    );
}

#[test]
fn errors_and_debug_do_not_reveal_secrets() {
    let mut harness = enabled_with_two_devices();
    let document = harness.load();
    let identity = document.identity().unwrap();
    let leaked = [
        FIXTURE_WEB_PAIRING,
        FIXTURE_NATIVE_PAIRING,
        "fixture-machine-a",
        "Kitchen phone",
        "Office desktop",
        "devmanager-connect-host",
    ];
    for text in [
        format!("{identity:?}"),
        format!("{:?}", IdentityError::CopiedProfile),
        format!("{}", IdentityError::CopiedProfile),
        format!("{:?}", IdentityError::Corrupt),
    ] {
        for secret in leaked {
            assert!(!text.contains(secret), "{text} leaked {secret}");
        }
    }

    let mut setup_harness = Harness::from_legacy();
    let setup_receipt = setup_harness.execute(
        0,
        IdentityOp::Enable {
            host_build: 100,
            now_epoch_ms: 1,
        },
    );
    assert!(!format!("{setup_receipt:?}").contains(FIXTURE_WEB_PAIRING));
    assert!(!format!("{:?}", setup_receipt.setup().unwrap()).contains(FIXTURE_WEB_PAIRING));
    assert!(!format!("{:?}", setup_receipt.pairing_code().unwrap()).contains(FIXTURE_WEB_PAIRING));
}

fn enabled_with_two_devices() -> Harness {
    let mut harness = Harness::from_legacy();
    harness.execute(
        0,
        IdentityOp::Enable {
            host_build: 100,
            now_epoch_ms: 1_700_000_004_000,
        },
    );
    harness.execute(
        1,
        IdentityOp::RegisterDevice(RegisterDevice {
            kind: DeviceKind::Native,
            label: "Office desktop".to_string(),
            legacy_client_id: Some("fixture-native-client".to_string()),
            browser: None,
        }),
    );
    harness.execute(
        2,
        IdentityOp::RegisterDevice(RegisterDevice {
            kind: DeviceKind::Browser,
            label: "Safari iPhone".to_string(),
            legacy_client_id: Some("fixture-web-client".to_string()),
            browser: Some(BrowserDeviceDto {
                browser_install_id: "fixture-browser-install".to_string(),
                nickname: Some("Kitchen phone".to_string()),
                private_identity_storage: BrowserPrivateStorage::WebCryptoNonExportableIndexedDb,
                cleared_storage_requires_visible_repair: true,
            }),
        }),
    );
    harness
}

fn oversize_device_document(count: u32) -> String {
    let devices = (0..count)
        .map(|index| {
            format!(
                r#"{{"deviceId":"01234567-89ab-7cde-8f01-23456789ab{:02x}","kind":"native","label":"d{index}"}}"#,
                index
            )
        })
        .collect::<Vec<_>>()
        .join(",");
    format!(
        r#"{{"connectCodecVersion":1,"connectIdentity":{{"schemaVersion":1,"devices":[{devices}]}}}}"#
    )
}

fn sha256_hex(input: &str) -> String {
    hex_encode(&Sha256::digest(input.as_bytes()))
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}
