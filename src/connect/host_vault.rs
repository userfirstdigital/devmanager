//! Host Windows DPAPI Connect credential vault plus public-only device registration.
//!
//! Host Noise static custody stays under exact host-id + transition-nonce
//! filenames. Device enrollment stores only the authenticated device public key
//! (never a host copy of the device private key). Host rotation and device
//! repair remain [`IdentityError::UnsupportedOperation`].

use std::fs::{self, File, OpenOptions};
use std::io::Read;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;
use zeroize::{Zeroize, Zeroizing};

use super::crypto::ConnectNoiseCustody;
use super::identity::{
    generate_transition_nonce, hex_encode, validate_fingerprint, ConnectIdentity, CredentialVault,
    DeviceEstablishmentHandle, DeviceId, DeviceKeyProof, DeviceKind, DeviceRepairHandle,
    HostEstablishmentHandle, HostKeyProof, HostPublicId, HostRotationHandle, IdentityError,
    IdentityLimitField, KeyReference, MachineBinding, MAX_FINGERPRINT_BYTES,
    MAX_IDENTITY_PHYSICAL_BYTES,
};
use super::identity_store::{protect_noise_private, unprotect_noise_private, OsNoiseCustodyError};
use crate::protocol::{AuthenticatedPeer, NoiseStaticPublicKey};

const HOST_VAULT_VERSION: u16 = 1;
const DEVICE_VAULT_VERSION: u16 = 1;
const HOST_VAULT_PURPOSE: &[u8] = b"DevManagerConnect/v1/host-vault\0";
const HOST_BINDING_DOMAIN: &[u8] = b"DevManagerConnectHostBinding/v1\0";
/// Stable pipe/lock profile name used by the packaged production host binary.
pub(crate) const PRODUCTION_HOST_PROFILE: &str = "production";
const HOST_VAULT_GENERATION: u64 = 1;
const NOISE_STATIC_KEY_BYTES: usize = 32;
const MAX_CIPHERTEXT_BYTES: usize = 8 * 1024;
const MAX_PUBLIC_HEX_BYTES: usize = NOISE_STATIC_KEY_BYTES * 2;
const MAX_NONCE_HEX_BYTES: usize = 32;
const MAX_HOST_HEX_BYTES: usize = 32;
const MAX_DEVICE_HEX_BYTES: usize = 32;
const MAX_VAULT_DIR_ENTRIES: usize = 256;
const VAULT_LOCK_FILE_NAME: &str = "vault-mutex.lock";

/// Derive a machine binding from a validated named profile.
///
/// Uses `profile_fingerprint_for_named_profile` (domain-tagged pipe fingerprint)
/// then wraps it with [`HOST_BINDING_DOMAIN`]. Does not invent hardware IDs and
/// does not use the raw fingerprint hex as the [`MachineBinding`] id.
pub fn derive_machine_binding(profile: &str) -> Result<MachineBinding, IdentityError> {
    let fingerprint = crate::host::profile_fingerprint_for_named_profile(profile)
        .map_err(|_| IdentityError::Corrupt)?;
    let mut digest = Sha256::new();
    digest.update(HOST_BINDING_DOMAIN);
    digest.update(fingerprint.as_bytes());
    Ok(MachineBinding::new(hex_encode(&digest.finalize())))
}

/// Resolve the host profile name for vault binding.
///
/// Prefer the active instance profile. When unset, use the packaged production
/// host profile `"production"` (same stable name as `devmanager-host`).
pub(crate) fn resolve_host_profile_for_binding() -> Result<String, IdentityError> {
    Ok(crate::persistence::app_instance_profile()
        .unwrap_or_else(|| PRODUCTION_HOST_PROFILE.to_string()))
}

/// OS-backed host credential vault. `open` is side-effect free until
/// [`CredentialVault::establish_host`] or an explicit write path runs.
pub struct OsConnectHostVault {
    root: PathBuf,
    binding: MachineBinding,
    /// Consumable authorization from a genuine device [`AuthenticatedPeer`].
    /// Required to mint a fresh device registration; exact slot replay does not
    /// re-consume it.
    pending_device_enrollment: Option<DeviceEnrollmentAuthorization>,
}

impl std::fmt::Debug for OsConnectHostVault {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("OsConnectHostVault")
            .field("root", &self.root)
            .field(
                "pending_device_enrollment",
                &self.pending_device_enrollment.is_some(),
            )
            .finish()
    }
}

/// One-shot proof that a finished Noise peer may seed device registration.
/// Holds the authenticated public key only — never a device private key.
#[derive(Clone, PartialEq, Eq)]
struct DeviceEnrollmentAuthorization {
    peer_public: NoiseStaticPublicKey,
    kind: DeviceKind,
}

/// Exclusive per-root vault mutex. Not an identity journal.
struct VaultRootLock {
    _lock_file: File,
    /// Held ancestor + root directory handles deny reparse replacement for the
    /// critical section (no DELETE sharing). Only the root allows write sharing
    /// required by Windows child-rename admission; mutations use its held handle.
    _directories: Vec<File>,
}

impl OsConnectHostVault {
    /// Bind a vault root and machine binding. Does not create directories or
    /// touch credentials until an establishing write.
    pub fn open(root: PathBuf, binding: MachineBinding) -> Result<Self, IdentityError> {
        if !root.is_absolute() || root.as_os_str().is_empty() {
            return Err(IdentityError::Corrupt);
        }
        validate_existing_ancestors_no_reparse(&root)?;
        if root.exists() {
            let meta = fs::symlink_metadata(&root).map_err(|_| IdentityError::PersistFailed)?;
            if !meta.is_dir() || metadata_is_reparse(&meta) {
                return Err(IdentityError::Corrupt);
            }
        }
        Ok(Self {
            root,
            binding,
            pending_device_enrollment: None,
        })
    }

    /// Authorize exactly one subsequent fresh [`CredentialVault::establish_device`]
    /// from a genuine completed Noise peer. Rejects host-kind peers. Never
    /// accepts a caller-supplied fingerprint alone as proof.
    pub(crate) fn authorize_device_enrollment(
        &mut self,
        peer: AuthenticatedPeer,
        kind: DeviceKind,
    ) -> Result<(), IdentityError> {
        if !peer.is_device() {
            return Err(IdentityError::InvalidDevice);
        }
        if self.pending_device_enrollment.is_some() {
            return Err(IdentityError::TransitionPending);
        }
        self.pending_device_enrollment = Some(DeviceEnrollmentAuthorization {
            peer_public: peer.static_public(),
            kind,
        });
        Ok(())
    }

    /// Load committed Noise custody for a durable identity. Fail-closed: never
    /// mints a key when the committed vault slot is missing or mismatched.
    pub(crate) fn load_host_noise(
        &self,
        identity: &ConnectIdentity,
    ) -> Result<ConnectNoiseCustody, IdentityError> {
        identity.validate_structure()?;
        if identity.profile_binding_hash() != self.binding.binding_hash() {
            return Err(IdentityError::CopiedProfile);
        }
        let generation = identity
            .host_key()
            .generation()
            .ok_or(IdentityError::Corrupt)?;
        if generation == 0 {
            return Err(IdentityError::Corrupt);
        }
        let Some(_lock) = self.try_lock_existing()? else {
            return Err(IdentityError::MissingCredentialProof);
        };
        let record = self
            .find_committed_for_identity(identity, generation)?
            .ok_or(IdentityError::MissingCredentialProof)?;
        let custody = self.decrypt_custody(&record)?;
        let fingerprint = public_fingerprint(&custody.public());
        if fingerprint != identity.host_key().fingerprint()
            || fingerprint != record.fingerprint
            || record.generation != generation
            || record.host_id_bytes()? != *identity.host_public_id().as_bytes()
        {
            return Err(IdentityError::WrongCredentialGeneration);
        }
        Ok(custody)
    }

    fn binding_hash(&self) -> String {
        self.binding.binding_hash()
    }

    fn slot_filename(host_id: HostPublicId, transition_nonce: [u8; 16]) -> String {
        format!(
            "h-{}-n-{}.json",
            hex_encode(host_id.as_bytes()),
            hex_encode(&transition_nonce)
        )
    }

    fn slot_path(
        &self,
        host_id: HostPublicId,
        transition_nonce: [u8; 16],
    ) -> Result<PathBuf, IdentityError> {
        let name = Self::slot_filename(host_id, transition_nonce);
        if !is_safe_slot_filename(&name) {
            return Err(IdentityError::Corrupt);
        }
        let path = self.root.join(&name);
        if path.parent() != Some(self.root.as_path()) {
            return Err(IdentityError::Corrupt);
        }
        if path.file_name().and_then(|value| value.to_str()) != Some(name.as_str()) {
            return Err(IdentityError::Corrupt);
        }
        Ok(path)
    }

    fn try_lock_existing(&self) -> Result<Option<VaultRootLock>, IdentityError> {
        if !self.root.exists() {
            return Ok(None);
        }
        Ok(Some(VaultRootLock::acquire(&self.root, false)?))
    }

    fn lock_for_write(&self) -> Result<VaultRootLock, IdentityError> {
        VaultRootLock::acquire(&self.root, true)
    }

    fn find_committed_for_identity(
        &self,
        identity: &ConnectIdentity,
        generation: u64,
    ) -> Result<Option<HostVaultRecord>, IdentityError> {
        let host_id = identity.host_public_id();
        let mut match_record = None;
        self.for_each_host_slot(host_id, |record| {
            if record.state != HostVaultState::Committed {
                return Ok(());
            }
            if record.binding_hash != self.binding_hash()
                || record.generation != generation
                || record.fingerprint != identity.host_key().fingerprint()
            {
                return Err(IdentityError::Corrupt);
            }
            // Decrypt before trusting metadata as a committed match.
            let custody = self.decrypt_custody(&record)?;
            if public_fingerprint(&custody.public()) != identity.host_key().fingerprint() {
                return Err(IdentityError::Corrupt);
            }
            if match_record.is_some() {
                return Err(IdentityError::Corrupt);
            }
            match_record = Some(record);
            Ok(())
        })?;
        Ok(match_record)
    }

    fn for_each_host_slot(
        &self,
        host_id: HostPublicId,
        mut visitor: impl FnMut(HostVaultRecord) -> Result<(), IdentityError>,
    ) -> Result<(), IdentityError> {
        let names = list_vault_entry_names_bounded(&self.root)?;
        let host_hex = hex_encode(host_id.as_bytes());
        let prefix = format!("h-{host_hex}-n-");
        for name in names {
            if name == VAULT_LOCK_FILE_NAME {
                continue;
            }
            if !is_safe_slot_filename(&name) {
                return Err(IdentityError::Corrupt);
            }
            if !name.starts_with(&prefix) {
                continue;
            }
            let path = self.root.join(&name);
            if path.parent() != Some(self.root.as_path()) {
                return Err(IdentityError::Corrupt);
            }
            let record = read_record_nofollow(&path)?;
            let expected = Self::slot_filename(record.host_public_id()?, record.nonce_bytes()?);
            if expected != name {
                return Err(IdentityError::Corrupt);
            }
            if record.host_id_bytes()? != *host_id.as_bytes() {
                return Err(IdentityError::Corrupt);
            }
            visitor(record)?;
        }
        Ok(())
    }

    fn load_slot(
        &self,
        host_id: HostPublicId,
        transition_nonce: [u8; 16],
    ) -> Result<Option<HostVaultRecord>, IdentityError> {
        let path = self.slot_path(host_id, transition_nonce)?;
        if !path.exists() {
            return Ok(None);
        }
        let record = read_record_nofollow(&path)?;
        let expected = Self::slot_filename(record.host_public_id()?, record.nonce_bytes()?);
        let actual = path
            .file_name()
            .and_then(|value| value.to_str())
            .ok_or(IdentityError::Corrupt)?;
        if expected != actual
            || record.host_public_id()? != host_id
            || record.nonce_bytes()? != transition_nonce
        {
            return Err(IdentityError::Corrupt);
        }
        Ok(Some(record))
    }

    fn persist_record(
        &self,
        _lock: &VaultRootLock,
        record: &HostVaultRecord,
    ) -> Result<(), IdentityError> {
        let host_id = record.host_public_id()?;
        let nonce = record.nonce_bytes()?;
        let path = self.slot_path(host_id, nonce)?;
        let bytes = serde_json::to_vec(record).map_err(|_| IdentityError::PersistFailed)?;
        if bytes.len() > MAX_IDENTITY_PHYSICAL_BYTES {
            return Err(IdentityError::LimitExceeded {
                field: IdentityLimitField::PhysicalBytes,
            });
        }
        // Resolve mutations relative to the retained protected directory rather
        // than re-resolving its path. Reuse ConfigStore's atomic primitives.
        #[cfg(windows)]
        crate::config::project_store::write_bytes_in_retained_directory(
            _lock._directories.last().ok_or(IdentityError::Corrupt)?,
            path.file_name().ok_or(IdentityError::Corrupt)?,
            &bytes,
        )
        .map_err(|_| IdentityError::PersistFailed)?;
        #[cfg(not(windows))]
        crate::diagnostics::profile::write_bytes_atomically(&path, &bytes)
            .map_err(|_| IdentityError::PersistFailed)?;
        // Re-open nofollow and require decryptable custody before success.
        let written = read_record_nofollow(&path)?;
        let custody = self.decrypt_custody(&written)?;
        if public_fingerprint(&custody.public()) != written.fingerprint
            || written.state != record.state
        {
            return Err(IdentityError::Corrupt);
        }
        Ok(())
    }

    fn delete_prepared_slot(
        &self,
        _lock: &VaultRootLock,
        handle: &HostEstablishmentHandle,
    ) -> Result<(), IdentityError> {
        let path = self.slot_path(handle.host_public_id(), handle.transition_nonce())?;
        if !path.exists() {
            return Ok(());
        }
        // Open exact file nofollow, confirm prepared + handle match, then delete.
        let record = read_record_nofollow(&path)?;
        if !self.record_matches_handle(&record, handle)? {
            return Err(IdentityError::MissingCredentialProof);
        }
        if record.state != HostVaultState::Prepared {
            return Err(IdentityError::Corrupt);
        }
        // Decrypt must still succeed for the prepared slot we are removing.
        let custody = self.decrypt_custody(&record)?;
        if public_fingerprint(&custody.public()) != handle.proof().fingerprint() {
            return Err(IdentityError::Corrupt);
        }
        #[cfg(windows)]
        return crate::config::project_store::remove_file_in_retained_directory(
            _lock._directories.last().ok_or(IdentityError::Corrupt)?,
            path.file_name().ok_or(IdentityError::Corrupt)?,
        )
        .map_err(|_| IdentityError::PersistFailed);
        #[cfg(not(windows))]
        fs::remove_file(&path).map_err(|_| IdentityError::PersistFailed)
    }

    fn handle_from_verified_record(
        &self,
        record: &HostVaultRecord,
    ) -> Result<HostEstablishmentHandle, IdentityError> {
        if record.binding_hash != self.binding_hash() {
            return Err(IdentityError::CopiedProfile);
        }
        if record.version != HOST_VAULT_VERSION {
            return Err(IdentityError::Corrupt);
        }
        let custody = self.decrypt_custody(record)?;
        let fingerprint = public_fingerprint(&custody.public());
        if fingerprint != record.fingerprint {
            return Err(IdentityError::Corrupt);
        }
        validate_fingerprint(&fingerprint)?;
        let host_id = record.host_public_id()?;
        let proof = HostKeyProof::from_parts(host_id, record.generation, fingerprint);
        KeyReference::from_host_proof(&proof)?;
        Ok(HostEstablishmentHandle::from_parts(
            host_id,
            record.nonce_bytes()?,
            record.slot_bytes()?,
            proof,
        ))
    }

    fn record_matches_handle(
        &self,
        record: &HostVaultRecord,
        handle: &HostEstablishmentHandle,
    ) -> Result<bool, IdentityError> {
        Ok(record.binding_hash == self.binding_hash()
            && record.host_public_id()? == handle.host_public_id()
            && record.nonce_bytes()? == handle.transition_nonce()
            && record.slot_bytes()? == handle.slot()
            && record.generation == handle.proof().generation()
            && record.fingerprint == handle.proof().fingerprint())
    }

    fn decrypt_custody(
        &self,
        record: &HostVaultRecord,
    ) -> Result<ConnectNoiseCustody, IdentityError> {
        if record.binding_hash != self.binding_hash() {
            return Err(IdentityError::CopiedProfile);
        }
        let public_bytes = decode_exact_hex::<NOISE_STATIC_KEY_BYTES>(&record.public_key)?;
        let public = crate::protocol::NoiseStaticPublicKey::from_bytes(public_bytes)
            .map_err(|_| IdentityError::Corrupt)?;
        if public_fingerprint(&public) != record.fingerprint {
            return Err(IdentityError::Corrupt);
        }
        let ciphertext = decode_hex_bounded(&record.ciphertext, MAX_CIPHERTEXT_BYTES)?;
        let entropy = vault_entropy(
            &record.binding_hash,
            record.host_public_id()?,
            record.generation,
            &record.nonce_bytes()?,
            &record.slot_bytes()?,
            &public,
        );
        let mut plain =
            unprotect_noise_private(&ciphertext, &entropy).map_err(map_custody_error)?;
        if plain.len() != NOISE_STATIC_KEY_BYTES {
            plain.zeroize();
            return Err(IdentityError::Corrupt);
        }
        let mut key_bytes = [0_u8; NOISE_STATIC_KEY_BYTES];
        key_bytes.copy_from_slice(&plain);
        plain.zeroize();
        let private = crate::protocol::NoiseStaticPrivateKey::from_vault_bytes(key_bytes)
            .map_err(|_| IdentityError::Corrupt)?;
        key_bytes.fill(0);
        let custody =
            ConnectNoiseCustody::from_vault(private, public).map_err(|_| IdentityError::Corrupt)?;
        if custody.public() != public {
            return Err(IdentityError::Corrupt);
        }
        Ok(custody)
    }

    fn mint_prepared(
        &self,
        lock: &VaultRootLock,
        host_id: HostPublicId,
        transition_nonce: [u8; 16],
    ) -> Result<HostEstablishmentHandle, IdentityError> {
        let slot = generate_transition_nonce()?;
        let custody = ConnectNoiseCustody::generate().map_err(|_| IdentityError::Corrupt)?;
        let public = custody.public();
        let fingerprint = public_fingerprint(&public);
        validate_fingerprint(&fingerprint)?;
        let generation = HOST_VAULT_GENERATION;
        let binding_hash = self.binding_hash();
        let entropy = vault_entropy(
            &binding_hash,
            host_id,
            generation,
            &transition_nonce,
            &slot,
            &public,
        );
        let mut private = Zeroizing::new(*custody.private().as_bytes());
        let blob =
            protect_noise_private(private.as_slice(), &entropy).map_err(map_custody_error)?;
        private.zeroize();
        if blob.is_empty() || blob.len() > MAX_CIPHERTEXT_BYTES {
            return Err(IdentityError::Corrupt);
        }
        let record = HostVaultRecord {
            version: HOST_VAULT_VERSION,
            state: HostVaultState::Prepared,
            host_public_id: hex_encode(host_id.as_bytes()),
            transition_nonce: hex_encode(&transition_nonce),
            slot: hex_encode(&slot),
            generation,
            binding_hash,
            public_key: hex_encode(&public.as_bytes()),
            fingerprint: fingerprint.clone(),
            ciphertext: hex_encode(&blob),
        };
        self.persist_record(lock, &record)?;
        self.handle_from_verified_record(&record)
    }

    fn device_slot_filename(device_id: DeviceId, transition_nonce: [u8; 16]) -> String {
        format!(
            "d-{}-n-{}.json",
            hex_encode(device_id.as_bytes()),
            hex_encode(&transition_nonce)
        )
    }

    fn device_slot_path(
        &self,
        device_id: DeviceId,
        transition_nonce: [u8; 16],
    ) -> Result<PathBuf, IdentityError> {
        let name = Self::device_slot_filename(device_id, transition_nonce);
        if !is_safe_device_slot_filename(&name) {
            return Err(IdentityError::Corrupt);
        }
        let path = self.root.join(&name);
        if path.parent() != Some(self.root.as_path()) {
            return Err(IdentityError::Corrupt);
        }
        if path.file_name().and_then(|value| value.to_str()) != Some(name.as_str()) {
            return Err(IdentityError::Corrupt);
        }
        Ok(path)
    }

    fn load_device_slot(
        &self,
        device_id: DeviceId,
        transition_nonce: [u8; 16],
    ) -> Result<Option<DeviceVaultRecord>, IdentityError> {
        let path = self.device_slot_path(device_id, transition_nonce)?;
        if !path.exists() {
            return Ok(None);
        }
        let record = read_device_record_nofollow(&path)?;
        let expected = Self::device_slot_filename(record.device_id()?, record.nonce_bytes()?);
        let actual = path
            .file_name()
            .and_then(|value| value.to_str())
            .ok_or(IdentityError::Corrupt)?;
        if expected != actual
            || record.device_id()? != device_id
            || record.nonce_bytes()? != transition_nonce
        {
            return Err(IdentityError::Corrupt);
        }
        Ok(Some(record))
    }

    fn for_each_device_slot(
        &self,
        device_id: DeviceId,
        mut visitor: impl FnMut(DeviceVaultRecord) -> Result<(), IdentityError>,
    ) -> Result<(), IdentityError> {
        let names = list_vault_entry_names_bounded(&self.root)?;
        let device_hex = hex_encode(device_id.as_bytes());
        let prefix = format!("d-{device_hex}-n-");
        for name in names {
            if name == VAULT_LOCK_FILE_NAME {
                continue;
            }
            if !is_safe_slot_filename(&name) {
                return Err(IdentityError::Corrupt);
            }
            if !name.starts_with(&prefix) {
                continue;
            }
            let path = self.root.join(&name);
            if path.parent() != Some(self.root.as_path()) {
                return Err(IdentityError::Corrupt);
            }
            let record = read_device_record_nofollow(&path)?;
            let expected = Self::device_slot_filename(record.device_id()?, record.nonce_bytes()?);
            if expected != name {
                return Err(IdentityError::Corrupt);
            }
            if record.device_id()? != device_id {
                return Err(IdentityError::Corrupt);
            }
            visitor(record)?;
        }
        Ok(())
    }

    fn persist_device_record(
        &self,
        _lock: &VaultRootLock,
        record: &DeviceVaultRecord,
    ) -> Result<(), IdentityError> {
        let device_id = record.device_id()?;
        let nonce = record.nonce_bytes()?;
        let path = self.device_slot_path(device_id, nonce)?;
        let bytes = serde_json::to_vec(record).map_err(|_| IdentityError::PersistFailed)?;
        if bytes.len() > MAX_IDENTITY_PHYSICAL_BYTES {
            return Err(IdentityError::LimitExceeded {
                field: IdentityLimitField::PhysicalBytes,
            });
        }
        #[cfg(windows)]
        crate::config::project_store::write_bytes_in_retained_directory(
            _lock._directories.last().ok_or(IdentityError::Corrupt)?,
            path.file_name().ok_or(IdentityError::Corrupt)?,
            &bytes,
        )
        .map_err(|_| IdentityError::PersistFailed)?;
        #[cfg(not(windows))]
        crate::diagnostics::profile::write_bytes_atomically(&path, &bytes)
            .map_err(|_| IdentityError::PersistFailed)?;
        let written = read_device_record_nofollow(&path)?;
        let public = decode_device_public(&written.public_key)?;
        if public_fingerprint(&public) != written.fingerprint || written.state != record.state {
            return Err(IdentityError::Corrupt);
        }
        if written.contains_private_material() {
            return Err(IdentityError::Corrupt);
        }
        Ok(())
    }

    fn delete_prepared_device_slot(
        &self,
        _lock: &VaultRootLock,
        handle: &DeviceEstablishmentHandle,
    ) -> Result<(), IdentityError> {
        let path = self.device_slot_path(handle.device_id(), handle.transition_nonce())?;
        if !path.exists() {
            return Ok(());
        }
        let record = read_device_record_nofollow(&path)?;
        if !self.device_record_matches_handle(&record, handle)? {
            return Err(IdentityError::MissingCredentialProof);
        }
        if record.state != DeviceVaultState::Prepared {
            return Err(IdentityError::Corrupt);
        }
        let public = decode_device_public(&record.public_key)?;
        if public_fingerprint(&public) != handle.proof().fingerprint() {
            return Err(IdentityError::Corrupt);
        }
        #[cfg(windows)]
        return crate::config::project_store::remove_file_in_retained_directory(
            _lock._directories.last().ok_or(IdentityError::Corrupt)?,
            path.file_name().ok_or(IdentityError::Corrupt)?,
        )
        .map_err(|_| IdentityError::PersistFailed);
        #[cfg(not(windows))]
        fs::remove_file(&path).map_err(|_| IdentityError::PersistFailed)
    }

    fn device_handle_from_verified_record(
        &self,
        record: &DeviceVaultRecord,
    ) -> Result<DeviceEstablishmentHandle, IdentityError> {
        if record.binding_hash != self.binding_hash() {
            return Err(IdentityError::CopiedProfile);
        }
        if record.version != DEVICE_VAULT_VERSION {
            return Err(IdentityError::Corrupt);
        }
        let public = decode_device_public(&record.public_key)?;
        let fingerprint = public_fingerprint(&public);
        if fingerprint != record.fingerprint {
            return Err(IdentityError::Corrupt);
        }
        validate_fingerprint(&fingerprint)?;
        let device_id = record.device_id()?;
        let proof = DeviceKeyProof::from_parts(device_id, record.kind, fingerprint);
        KeyReference::from_device_proof(&proof)?;
        Ok(DeviceEstablishmentHandle::from_parts(
            device_id,
            record.nonce_bytes()?,
            record.slot_bytes()?,
            proof,
        ))
    }

    fn device_record_matches_handle(
        &self,
        record: &DeviceVaultRecord,
        handle: &DeviceEstablishmentHandle,
    ) -> Result<bool, IdentityError> {
        Ok(record.binding_hash == self.binding_hash()
            && record.device_id()? == handle.device_id()
            && record.nonce_bytes()? == handle.transition_nonce()
            && record.slot_bytes()? == handle.slot()
            && record.kind == handle.proof().kind()
            && record.fingerprint == handle.proof().fingerprint())
    }

    fn mint_prepared_device(
        &mut self,
        lock: &VaultRootLock,
        device_id: DeviceId,
        kind: DeviceKind,
        transition_nonce: [u8; 16],
        peer_public: NoiseStaticPublicKey,
    ) -> Result<DeviceEstablishmentHandle, IdentityError> {
        let slot = generate_transition_nonce()?;
        let fingerprint = public_fingerprint(&peer_public);
        validate_fingerprint(&fingerprint)?;
        let record = DeviceVaultRecord {
            version: DEVICE_VAULT_VERSION,
            state: DeviceVaultState::Prepared,
            device_id: hex_encode(device_id.as_bytes()),
            kind,
            transition_nonce: hex_encode(&transition_nonce),
            slot: hex_encode(&slot),
            binding_hash: self.binding_hash(),
            public_key: hex_encode(&peer_public.as_bytes()),
            fingerprint: fingerprint.clone(),
            revocation_epoch: 0,
        };
        if record.contains_private_material() {
            return Err(IdentityError::Corrupt);
        }
        self.persist_device_record(lock, &record)?;
        self.device_handle_from_verified_record(&record)
    }

    fn find_occupied_device_conflict(&self, device_id: DeviceId) -> Result<(), IdentityError> {
        let mut occupied = false;
        self.for_each_device_slot(device_id, |_record| {
            occupied = true;
            Ok(())
        })?;
        if occupied {
            return Err(IdentityError::DuplicateDevice);
        }
        Ok(())
    }
}

impl CredentialVault for OsConnectHostVault {
    fn establish_host(
        &mut self,
        host_id: HostPublicId,
        transition_nonce: [u8; 16],
    ) -> Result<HostEstablishmentHandle, IdentityError> {
        if transition_nonce == [0; 16] {
            return Err(IdentityError::Corrupt);
        }
        let lock = self.lock_for_write()?;
        if let Some(existing) = self.load_slot(host_id, transition_nonce)? {
            if existing.host_public_id()? != host_id
                || existing.nonce_bytes()? != transition_nonce
                || existing.binding_hash != self.binding_hash()
            {
                return Err(IdentityError::Corrupt);
            }
            return self.handle_from_verified_record(&existing);
        }
        self.mint_prepared(&lock, host_id, transition_nonce)
    }

    fn commit_host_establishment(
        &mut self,
        handle: &HostEstablishmentHandle,
    ) -> Result<(), IdentityError> {
        let lock = self.lock_for_write()?;
        let Some(mut record) =
            self.load_slot(handle.host_public_id(), handle.transition_nonce())?
        else {
            return Err(IdentityError::MissingCredentialProof);
        };
        if !self.record_matches_handle(&record, handle)? {
            return Err(IdentityError::MissingCredentialProof);
        }
        // Always decrypt before claiming committed success.
        let custody = self.decrypt_custody(&record)?;
        if public_fingerprint(&custody.public()) != handle.proof().fingerprint() {
            return Err(IdentityError::Corrupt);
        }
        match record.state {
            HostVaultState::Committed => Ok(()),
            HostVaultState::Prepared => {
                record.state = HostVaultState::Committed;
                self.persist_record(&lock, &record)
            }
        }
    }

    fn rollback_host_establishment(
        &mut self,
        handle: &HostEstablishmentHandle,
    ) -> Result<(), IdentityError> {
        let lock = self.lock_for_write()?;
        let Some(record) = self.load_slot(handle.host_public_id(), handle.transition_nonce())?
        else {
            return Ok(());
        };
        if !self.record_matches_handle(&record, handle)? {
            return Err(IdentityError::MissingCredentialProof);
        }
        match record.state {
            HostVaultState::Committed => Err(IdentityError::Corrupt),
            HostVaultState::Prepared => self.delete_prepared_slot(&lock, handle),
        }
    }

    fn recover_host_establishment(
        &mut self,
        host_id: HostPublicId,
        transition_nonce: [u8; 16],
    ) -> Result<Option<HostEstablishmentHandle>, IdentityError> {
        let Some(_lock) = self.try_lock_existing()? else {
            return Ok(None);
        };
        let Some(record) = self.load_slot(host_id, transition_nonce)? else {
            return Ok(None);
        };
        if record.host_public_id()? != host_id || record.nonce_bytes()? != transition_nonce {
            return Err(IdentityError::Corrupt);
        }
        if record.binding_hash != self.binding_hash() {
            return Err(IdentityError::CopiedProfile);
        }
        Ok(Some(self.handle_from_verified_record(&record)?))
    }

    fn host_establishment_committed(
        &self,
        handle: &HostEstablishmentHandle,
    ) -> Result<bool, IdentityError> {
        let Some(_lock) = self.try_lock_existing()? else {
            return Err(IdentityError::MissingCredentialProof);
        };
        let Some(record) = self.load_slot(handle.host_public_id(), handle.transition_nonce())?
        else {
            return Err(IdentityError::MissingCredentialProof);
        };
        if !self.record_matches_handle(&record, handle)? {
            return Err(IdentityError::MissingCredentialProof);
        }
        let custody = self.decrypt_custody(&record)?;
        if public_fingerprint(&custody.public()) != handle.proof().fingerprint() {
            return Err(IdentityError::Corrupt);
        }
        Ok(matches!(record.state, HostVaultState::Committed))
    }

    fn prepare_host_rotation(
        &mut self,
        _host_id: HostPublicId,
        _transition_nonce: [u8; 16],
    ) -> Result<HostRotationHandle, IdentityError> {
        Err(IdentityError::UnsupportedOperation)
    }

    fn commit_host_rotation(&mut self, _handle: &HostRotationHandle) -> Result<(), IdentityError> {
        Err(IdentityError::UnsupportedOperation)
    }

    fn abort_host_rotation(&mut self, _handle: &HostRotationHandle) -> Result<(), IdentityError> {
        Err(IdentityError::UnsupportedOperation)
    }

    fn recover_host_rotation(
        &mut self,
        _host_id: HostPublicId,
        _transition_nonce: [u8; 16],
    ) -> Result<Option<HostRotationHandle>, IdentityError> {
        Err(IdentityError::UnsupportedOperation)
    }

    fn verify_host(
        &self,
        host_id: HostPublicId,
        proof: &HostKeyProof,
    ) -> Result<(), IdentityError> {
        if proof.host_public_id() != host_id {
            return Err(IdentityError::MissingCredentialProof);
        }
        validate_fingerprint(proof.fingerprint())?;
        if proof.generation() == 0 {
            return Err(IdentityError::Corrupt);
        }
        let Some(_lock) = self.try_lock_existing()? else {
            return Err(IdentityError::MissingCredentialProof);
        };
        let mut matched = false;
        self.for_each_host_slot(host_id, |record| {
            if record.state != HostVaultState::Committed {
                return Ok(());
            }
            if record.binding_hash != self.binding_hash()
                || record.generation != proof.generation()
                || record.fingerprint != proof.fingerprint()
            {
                return Err(IdentityError::Corrupt);
            }
            let custody = self.decrypt_custody(&record)?;
            if public_fingerprint(&custody.public()) != proof.fingerprint() {
                return Err(IdentityError::Corrupt);
            }
            if matched {
                return Err(IdentityError::Corrupt);
            }
            matched = true;
            Ok(())
        })?;
        if matched {
            Ok(())
        } else {
            Err(IdentityError::MissingCredentialProof)
        }
    }

    fn establish_device(
        &mut self,
        device_id: DeviceId,
        kind: DeviceKind,
        transition_nonce: [u8; 16],
    ) -> Result<DeviceEstablishmentHandle, IdentityError> {
        if transition_nonce == [0; 16] {
            return Err(IdentityError::Corrupt);
        }
        let lock = self.lock_for_write()?;
        if let Some(existing) = self.load_device_slot(device_id, transition_nonce)? {
            if existing.device_id()? != device_id
                || existing.nonce_bytes()? != transition_nonce
                || existing.binding_hash != self.binding_hash()
                || existing.kind != kind
            {
                return Err(IdentityError::Corrupt);
            }
            let handle = self.device_handle_from_verified_record(&existing)?;
            if let Some(authorization) = self.pending_device_enrollment.take() {
                if authorization.kind != kind
                    || public_fingerprint(&authorization.peer_public)
                        != handle.proof().fingerprint()
                {
                    self.pending_device_enrollment = Some(authorization);
                    return Err(IdentityError::DuplicateDevice);
                }
            }
            // Exact same-slot replay is idempotent.
            return Ok(handle);
        }
        let authorization = self
            .pending_device_enrollment
            .as_ref()
            .ok_or(IdentityError::MissingCredentialProof)?;
        if authorization.kind != kind {
            return Err(IdentityError::InvalidDevice);
        }
        self.find_occupied_device_conflict(device_id)?;
        let authorization = self
            .pending_device_enrollment
            .take()
            .ok_or(IdentityError::MissingCredentialProof)?;
        self.mint_prepared_device(
            &lock,
            device_id,
            kind,
            transition_nonce,
            authorization.peer_public,
        )
    }

    fn commit_device_establishment(
        &mut self,
        handle: &DeviceEstablishmentHandle,
    ) -> Result<(), IdentityError> {
        let lock = self.lock_for_write()?;
        let Some(mut record) =
            self.load_device_slot(handle.device_id(), handle.transition_nonce())?
        else {
            return Err(IdentityError::MissingCredentialProof);
        };
        if !self.device_record_matches_handle(&record, handle)? {
            return Err(IdentityError::MissingCredentialProof);
        }
        let public = decode_device_public(&record.public_key)?;
        if public_fingerprint(&public) != handle.proof().fingerprint() {
            return Err(IdentityError::Corrupt);
        }
        if record.revocation_epoch != 0 {
            return Err(IdentityError::Corrupt);
        }
        match record.state {
            DeviceVaultState::Committed => Ok(()),
            DeviceVaultState::Prepared => {
                record.state = DeviceVaultState::Committed;
                self.persist_device_record(&lock, &record)
            }
        }
    }

    fn recover_device_establishment(
        &mut self,
        device_id: DeviceId,
        transition_nonce: [u8; 16],
    ) -> Result<Option<DeviceEstablishmentHandle>, IdentityError> {
        let Some(_lock) = self.try_lock_existing()? else {
            return Ok(None);
        };
        let Some(record) = self.load_device_slot(device_id, transition_nonce)? else {
            return Ok(None);
        };
        if record.device_id()? != device_id || record.nonce_bytes()? != transition_nonce {
            return Err(IdentityError::Corrupt);
        }
        if record.binding_hash != self.binding_hash() {
            return Err(IdentityError::CopiedProfile);
        }
        Ok(Some(self.device_handle_from_verified_record(&record)?))
    }

    fn device_establishment_committed(
        &self,
        handle: &DeviceEstablishmentHandle,
    ) -> Result<bool, IdentityError> {
        let Some(_lock) = self.try_lock_existing()? else {
            return Err(IdentityError::MissingCredentialProof);
        };
        let Some(record) = self.load_device_slot(handle.device_id(), handle.transition_nonce())?
        else {
            return Err(IdentityError::MissingCredentialProof);
        };
        if !self.device_record_matches_handle(&record, handle)? {
            return Err(IdentityError::MissingCredentialProof);
        }
        let public = decode_device_public(&record.public_key)?;
        if public_fingerprint(&public) != handle.proof().fingerprint() {
            return Err(IdentityError::Corrupt);
        }
        Ok(matches!(record.state, DeviceVaultState::Committed))
    }

    fn prepare_device_repair(
        &mut self,
        _device_id: DeviceId,
        _kind: DeviceKind,
        _transition_nonce: [u8; 16],
    ) -> Result<DeviceRepairHandle, IdentityError> {
        Err(IdentityError::UnsupportedOperation)
    }

    fn commit_device_repair(&mut self, _handle: &DeviceRepairHandle) -> Result<(), IdentityError> {
        Err(IdentityError::UnsupportedOperation)
    }

    fn device_repair_committed(&self, _handle: &DeviceRepairHandle) -> Result<bool, IdentityError> {
        Err(IdentityError::UnsupportedOperation)
    }

    fn rollback_device_repair(
        &mut self,
        _handle: &DeviceRepairHandle,
    ) -> Result<(), IdentityError> {
        Err(IdentityError::UnsupportedOperation)
    }

    fn abort_device_repair(&mut self, _handle: &DeviceRepairHandle) -> Result<(), IdentityError> {
        Err(IdentityError::UnsupportedOperation)
    }

    fn recover_device_repair(
        &mut self,
        _device_id: DeviceId,
        _transition_nonce: [u8; 16],
    ) -> Result<Option<DeviceRepairHandle>, IdentityError> {
        Err(IdentityError::UnsupportedOperation)
    }

    fn invalidate_device_credential(
        &mut self,
        device_id: DeviceId,
        revocation_epoch: u64,
    ) -> Result<(), IdentityError> {
        if revocation_epoch == 0 {
            return Err(IdentityError::Corrupt);
        }
        let lock = self.lock_for_write()?;
        let mut matched_committed = None;
        self.for_each_device_slot(device_id, |record| {
            if record.state != DeviceVaultState::Committed {
                return Ok(());
            }
            if record.binding_hash != self.binding_hash() {
                return Err(IdentityError::CopiedProfile);
            }
            if matched_committed.is_some() {
                return Err(IdentityError::Corrupt);
            }
            matched_committed = Some(record);
            Ok(())
        })?;
        let Some(mut record) = matched_committed else {
            return Err(IdentityError::MissingCredentialProof);
        };
        if record.revocation_epoch > revocation_epoch {
            return Err(IdentityError::Corrupt);
        }
        if record.revocation_epoch == revocation_epoch {
            return Ok(());
        }
        record.revocation_epoch = revocation_epoch;
        self.persist_device_record(&lock, &record)
    }

    fn restore_device_credential(
        &mut self,
        device_id: DeviceId,
        revocation_epoch: u64,
    ) -> Result<(), IdentityError> {
        if revocation_epoch == 0 {
            return Err(IdentityError::Corrupt);
        }
        let lock = self.lock_for_write()?;
        let mut matched_committed = None;
        self.for_each_device_slot(device_id, |record| {
            if record.state != DeviceVaultState::Committed {
                return Ok(());
            }
            if record.binding_hash != self.binding_hash() {
                return Err(IdentityError::CopiedProfile);
            }
            if matched_committed.is_some() {
                return Err(IdentityError::Corrupt);
            }
            matched_committed = Some(record);
            Ok(())
        })?;
        let Some(mut record) = matched_committed else {
            return Err(IdentityError::MissingCredentialProof);
        };
        if record.revocation_epoch == 0 {
            return Err(IdentityError::Corrupt);
        }
        if record.revocation_epoch != revocation_epoch {
            // Newer invalidation (or mismatched epoch) cannot be cleared.
            return Err(IdentityError::Corrupt);
        }
        record.revocation_epoch = 0;
        self.persist_device_record(&lock, &record)
    }

    fn rollback_device_establishment(
        &mut self,
        handle: &DeviceEstablishmentHandle,
    ) -> Result<(), IdentityError> {
        let lock = self.lock_for_write()?;
        let Some(record) = self.load_device_slot(handle.device_id(), handle.transition_nonce())?
        else {
            return Ok(());
        };
        if !self.device_record_matches_handle(&record, handle)? {
            return Err(IdentityError::MissingCredentialProof);
        }
        match record.state {
            DeviceVaultState::Committed => Err(IdentityError::Corrupt),
            DeviceVaultState::Prepared => self.delete_prepared_device_slot(&lock, handle),
        }
    }

    fn verify_device(
        &self,
        device_id: DeviceId,
        proof: &DeviceKeyProof,
    ) -> Result<(), IdentityError> {
        if proof.device_id() != device_id {
            return Err(IdentityError::MissingCredentialProof);
        }
        validate_fingerprint(proof.fingerprint())?;
        let Some(_lock) = self.try_lock_existing()? else {
            return Err(IdentityError::MissingCredentialProof);
        };
        let mut matched = false;
        self.for_each_device_slot(device_id, |record| {
            if record.state != DeviceVaultState::Committed {
                return Ok(());
            }
            if record.binding_hash != self.binding_hash()
                || record.kind != proof.kind()
                || record.fingerprint != proof.fingerprint()
            {
                return Err(IdentityError::Corrupt);
            }
            if record.revocation_epoch != 0 {
                return Err(IdentityError::MissingCredentialProof);
            }
            let public = decode_device_public(&record.public_key)?;
            if public_fingerprint(&public) != proof.fingerprint() {
                return Err(IdentityError::Corrupt);
            }
            if matched {
                return Err(IdentityError::Corrupt);
            }
            matched = true;
            Ok(())
        })?;
        if matched {
            Ok(())
        } else {
            Err(IdentityError::MissingCredentialProof)
        }
    }
}

impl VaultRootLock {
    fn acquire(root: &Path, create_root: bool) -> Result<Self, IdentityError> {
        validate_existing_ancestors_no_reparse(root)?;
        let directories = retain_directory_chain(root, create_root)?;
        let lock_path = root.join(VAULT_LOCK_FILE_NAME);
        if lock_path.parent() != Some(root) {
            return Err(IdentityError::Corrupt);
        }
        let lock_file = open_exclusive_lock_file(&lock_path)?;
        let lock_meta = lock_file
            .metadata()
            .map_err(|_| IdentityError::PersistFailed)?;
        if metadata_is_reparse(&lock_meta) || !lock_meta.is_file() {
            return Err(IdentityError::Corrupt);
        }
        Ok(Self {
            _lock_file: lock_file,
            _directories: directories,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
enum HostVaultState {
    Prepared,
    Committed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
enum DeviceVaultState {
    Prepared,
    Committed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct HostVaultRecord {
    version: u16,
    state: HostVaultState,
    host_public_id: String,
    transition_nonce: String,
    slot: String,
    generation: u64,
    binding_hash: String,
    public_key: String,
    fingerprint: String,
    ciphertext: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct DeviceVaultRecord {
    version: u16,
    state: DeviceVaultState,
    device_id: String,
    kind: DeviceKind,
    transition_nonce: String,
    slot: String,
    binding_hash: String,
    public_key: String,
    fingerprint: String,
    #[serde(default)]
    revocation_epoch: u64,
}

impl HostVaultRecord {
    fn host_id_bytes(&self) -> Result<[u8; 16], IdentityError> {
        decode_exact_hex::<16>(&self.host_public_id)
    }

    fn host_public_id(&self) -> Result<HostPublicId, IdentityError> {
        let bytes = self.host_id_bytes()?;
        let uuid = Uuid::from_bytes(bytes);
        HostPublicId::parse(&uuid.to_string())
    }

    fn nonce_bytes(&self) -> Result<[u8; 16], IdentityError> {
        decode_exact_hex::<16>(&self.transition_nonce)
    }

    fn slot_bytes(&self) -> Result<[u8; 16], IdentityError> {
        decode_exact_hex::<16>(&self.slot)
    }
}

impl DeviceVaultRecord {
    fn device_id_bytes(&self) -> Result<[u8; 16], IdentityError> {
        decode_exact_hex::<16>(&self.device_id)
    }

    fn device_id(&self) -> Result<DeviceId, IdentityError> {
        let bytes = self.device_id_bytes()?;
        let uuid = Uuid::from_bytes(bytes);
        DeviceId::parse(&uuid.to_string())
    }

    fn nonce_bytes(&self) -> Result<[u8; 16], IdentityError> {
        decode_exact_hex::<16>(&self.transition_nonce)
    }

    fn slot_bytes(&self) -> Result<[u8; 16], IdentityError> {
        decode_exact_hex::<16>(&self.slot)
    }

    fn contains_private_material(&self) -> bool {
        // Public-only registration: reject legacy/host-shaped secret fields if
        // a corrupted writer ever injected them under an alternate schema.
        let encoded = serde_json::to_string(self).unwrap_or_default();
        encoded.contains("ciphertext")
            || encoded.contains("private")
            || encoded.contains("privateKey")
            || encoded.contains("seed")
    }
}

fn vault_entropy(
    binding_hash: &str,
    host_id: HostPublicId,
    generation: u64,
    nonce: &[u8; 16],
    slot: &[u8; 16],
    public: &crate::protocol::NoiseStaticPublicKey,
) -> [u8; 32] {
    let mut digest = Sha256::new();
    digest.update(HOST_VAULT_PURPOSE);
    digest.update(HOST_VAULT_VERSION.to_be_bytes());
    digest.update(binding_hash.as_bytes());
    digest.update(host_id.as_bytes());
    digest.update(generation.to_be_bytes());
    digest.update(nonce);
    digest.update(slot);
    digest.update(&public.as_bytes());
    digest.finalize().into()
}

fn public_fingerprint(public: &NoiseStaticPublicKey) -> String {
    hex_encode(&Sha256::digest(public.as_bytes()))
}

fn decode_device_public(raw: &str) -> Result<NoiseStaticPublicKey, IdentityError> {
    let public_bytes = decode_exact_hex::<NOISE_STATIC_KEY_BYTES>(raw)?;
    NoiseStaticPublicKey::from_bytes(public_bytes).map_err(|_| IdentityError::Corrupt)
}

fn map_custody_error(error: OsNoiseCustodyError) -> IdentityError {
    match error {
        OsNoiseCustodyError::UnsupportedPlatform => IdentityError::UnsupportedOperation,
        OsNoiseCustodyError::PersistFailed => IdentityError::PersistFailed,
        OsNoiseCustodyError::EntropyUnavailable => IdentityError::Corrupt,
        OsNoiseCustodyError::ContextMismatch | OsNoiseCustodyError::PublicMismatch => {
            IdentityError::CopiedProfile
        }
        OsNoiseCustodyError::InvalidBlob
        | OsNoiseCustodyError::UnprotectFailed
        | OsNoiseCustodyError::ProtectFailed => IdentityError::Corrupt,
    }
}

fn is_safe_slot_filename(name: &str) -> bool {
    is_safe_host_slot_filename(name) || is_safe_device_slot_filename(name)
}

fn is_safe_host_slot_filename(name: &str) -> bool {
    let Some(stem) = name.strip_suffix(".json") else {
        return false;
    };
    let Some((host_part, nonce_part)) = stem.split_once("-n-") else {
        return false;
    };
    let Some(host_hex) = host_part.strip_prefix("h-") else {
        return false;
    };
    host_hex.len() == MAX_HOST_HEX_BYTES
        && nonce_part.len() == MAX_NONCE_HEX_BYTES
        && is_lowercase_hex(host_hex)
        && is_lowercase_hex(nonce_part)
}

fn is_safe_device_slot_filename(name: &str) -> bool {
    let Some(stem) = name.strip_suffix(".json") else {
        return false;
    };
    let Some((device_part, nonce_part)) = stem.split_once("-n-") else {
        return false;
    };
    let Some(device_hex) = device_part.strip_prefix("d-") else {
        return false;
    };
    device_hex.len() == MAX_DEVICE_HEX_BYTES
        && nonce_part.len() == MAX_NONCE_HEX_BYTES
        && is_lowercase_hex(device_hex)
        && is_lowercase_hex(nonce_part)
}

fn is_lowercase_hex(raw: &str) -> bool {
    !raw.is_empty()
        && raw
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
}

fn metadata_is_reparse(metadata: &fs::Metadata) -> bool {
    if metadata.file_type().is_symlink() {
        return true;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
        return metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0;
    }
    #[cfg(not(windows))]
    {
        false
    }
}

fn validate_existing_ancestors_no_reparse(path: &Path) -> Result<(), IdentityError> {
    if !path.is_absolute() {
        return Err(IdentityError::Corrupt);
    }
    let mut ancestors = path
        .ancestors()
        .filter(|ancestor| !ancestor.as_os_str().is_empty())
        .collect::<Vec<_>>();
    ancestors.reverse();
    for ancestor in ancestors {
        if !ancestor.exists() {
            continue;
        }
        let meta = fs::symlink_metadata(ancestor).map_err(|_| IdentityError::PersistFailed)?;
        if metadata_is_reparse(&meta) {
            return Err(IdentityError::Corrupt);
        }
        if ancestor == path && !meta.is_dir() {
            return Err(IdentityError::Corrupt);
        }
    }
    Ok(())
}

fn retain_directory_chain(root: &Path, create_root: bool) -> Result<Vec<File>, IdentityError> {
    validate_existing_ancestors_no_reparse(root)?;
    if create_root && !root.exists() {
        if let Some(parent) = root.parent() {
            // Parent must already exist as a plain directory; never create
            // through a missing intermediate that could race into a junction.
            let parent_meta =
                fs::symlink_metadata(parent).map_err(|_| IdentityError::PersistFailed)?;
            if !parent_meta.is_dir() || metadata_is_reparse(&parent_meta) {
                return Err(IdentityError::Corrupt);
            }
        }
        fs::create_dir(root).map_err(|_| IdentityError::PersistFailed)?;
    }
    let mut ancestors = root
        .ancestors()
        .filter(|ancestor| !ancestor.as_os_str().is_empty())
        .collect::<Vec<_>>();
    ancestors.reverse();
    let mut retained = Vec::with_capacity(ancestors.len());
    for ancestor in ancestors {
        retained.push(open_directory_retain_handle(ancestor, ancestor == root)?);
    }
    Ok(retained)
}

fn open_directory_retain_handle(path: &Path, writable: bool) -> Result<File, IdentityError> {
    let metadata = fs::symlink_metadata(path).map_err(|_| IdentityError::PersistFailed)?;
    if !metadata.is_dir() || metadata_is_reparse(&metadata) {
        return Err(IdentityError::Corrupt);
    }
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        use windows::Win32::Storage::FileSystem::{
            FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_READ,
            FILE_SHARE_WRITE,
        };
        options
            .write(writable)
            // Child rename opens the destination directory for write, even
            // with a RootDirectory handle. Do not grant DELETE sharing: root
            // replacement remains forbidden while this handle is retained.
            .share_mode(FILE_SHARE_READ.0 | if writable { FILE_SHARE_WRITE.0 } else { 0 })
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS.0 | FILE_FLAG_OPEN_REPARSE_POINT.0);
    }
    #[cfg(not(windows))]
    let _ = writable;
    let handle = options
        .open(path)
        .map_err(|_| IdentityError::PersistFailed)?;
    let opened = handle
        .metadata()
        .map_err(|_| IdentityError::PersistFailed)?;
    if !opened.is_dir() || metadata_is_reparse(&opened) {
        return Err(IdentityError::Corrupt);
    }
    Ok(handle)
}

fn open_exclusive_lock_file(path: &Path) -> Result<File, IdentityError> {
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        use windows::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT;
        // share_mode(0): exclusive. Busy competitors fail immediately.
        options
            .share_mode(0)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT.0);
    }
    options.open(path).map_err(|err| {
        if err.kind() == std::io::ErrorKind::PermissionDenied
            || err.raw_os_error() == Some(32)
            || err.raw_os_error() == Some(33)
        {
            IdentityError::PersistFailed
        } else {
            IdentityError::PersistFailed
        }
    })
}

fn open_file_nofollow(path: &Path) -> Result<File, IdentityError> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        use windows::Win32::Storage::FileSystem::{FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_READ};
        options
            .share_mode(FILE_SHARE_READ.0)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT.0);
    }
    let file = options
        .open(path)
        .map_err(|_| IdentityError::PersistFailed)?;
    let meta = file.metadata().map_err(|_| IdentityError::PersistFailed)?;
    if !meta.is_file() || metadata_is_reparse(&meta) {
        return Err(IdentityError::Corrupt);
    }
    Ok(file)
}

fn list_vault_entry_names_bounded(root: &Path) -> Result<Vec<String>, IdentityError> {
    let entries = match fs::read_dir(root) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(_) => return Err(IdentityError::PersistFailed),
    };
    let mut names = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|_| IdentityError::PersistFailed)?;
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| IdentityError::Corrupt)?;
        names.push(name);
        if names.len() > MAX_VAULT_DIR_ENTRIES {
            return Err(IdentityError::LimitExceeded {
                field: IdentityLimitField::PhysicalBytes,
            });
        }
    }
    Ok(names)
}

fn read_record_nofollow(path: &Path) -> Result<HostVaultRecord, IdentityError> {
    let mut file = open_file_nofollow(path)?;
    let meta = file.metadata().map_err(|_| IdentityError::PersistFailed)?;
    if meta.len() > MAX_IDENTITY_PHYSICAL_BYTES as u64 {
        return Err(IdentityError::LimitExceeded {
            field: IdentityLimitField::PhysicalBytes,
        });
    }
    let mut bytes = Vec::new();
    file.by_ref()
        .take(MAX_IDENTITY_PHYSICAL_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| IdentityError::PersistFailed)?;
    if bytes.len() > MAX_IDENTITY_PHYSICAL_BYTES {
        return Err(IdentityError::LimitExceeded {
            field: IdentityLimitField::PhysicalBytes,
        });
    }
    let record: HostVaultRecord =
        serde_json::from_slice(&bytes).map_err(|_| IdentityError::Corrupt)?;
    validate_record_bounds(&record)?;
    Ok(record)
}

fn read_device_record_nofollow(path: &Path) -> Result<DeviceVaultRecord, IdentityError> {
    let mut file = open_file_nofollow(path)?;
    let meta = file.metadata().map_err(|_| IdentityError::PersistFailed)?;
    if meta.len() > MAX_IDENTITY_PHYSICAL_BYTES as u64 {
        return Err(IdentityError::LimitExceeded {
            field: IdentityLimitField::PhysicalBytes,
        });
    }
    let mut bytes = Vec::new();
    file.by_ref()
        .take(MAX_IDENTITY_PHYSICAL_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| IdentityError::PersistFailed)?;
    if bytes.len() > MAX_IDENTITY_PHYSICAL_BYTES {
        return Err(IdentityError::LimitExceeded {
            field: IdentityLimitField::PhysicalBytes,
        });
    }
    let record: DeviceVaultRecord =
        serde_json::from_slice(&bytes).map_err(|_| IdentityError::Corrupt)?;
    validate_device_record_bounds(&record)?;
    if record.contains_private_material() {
        return Err(IdentityError::Corrupt);
    }
    Ok(record)
}

fn validate_record_bounds(record: &HostVaultRecord) -> Result<(), IdentityError> {
    if record.version != HOST_VAULT_VERSION {
        return Err(IdentityError::Corrupt);
    }
    if record.generation == 0 {
        return Err(IdentityError::Corrupt);
    }
    validate_fingerprint(&record.fingerprint)?;
    if record.public_key.len() != MAX_PUBLIC_HEX_BYTES || !is_lowercase_hex(&record.public_key) {
        return Err(IdentityError::Corrupt);
    }
    if record.host_public_id.len() != MAX_HOST_HEX_BYTES
        || !is_lowercase_hex(&record.host_public_id)
    {
        return Err(IdentityError::Corrupt);
    }
    if record.transition_nonce.len() != MAX_NONCE_HEX_BYTES
        || !is_lowercase_hex(&record.transition_nonce)
    {
        return Err(IdentityError::Corrupt);
    }
    if record.slot.len() != MAX_NONCE_HEX_BYTES || !is_lowercase_hex(&record.slot) {
        return Err(IdentityError::Corrupt);
    }
    if record.binding_hash.len() != MAX_FINGERPRINT_BYTES || !is_lowercase_hex(&record.binding_hash)
    {
        return Err(IdentityError::Corrupt);
    }
    if record.ciphertext.is_empty()
        || record.ciphertext.len() > MAX_CIPHERTEXT_BYTES * 2
        || !is_lowercase_hex(&record.ciphertext)
    {
        return Err(IdentityError::Corrupt);
    }
    Ok(())
}

fn validate_device_record_bounds(record: &DeviceVaultRecord) -> Result<(), IdentityError> {
    if record.version != DEVICE_VAULT_VERSION {
        return Err(IdentityError::Corrupt);
    }
    validate_fingerprint(&record.fingerprint)?;
    if record.public_key.len() != MAX_PUBLIC_HEX_BYTES || !is_lowercase_hex(&record.public_key) {
        return Err(IdentityError::Corrupt);
    }
    if record.device_id.len() != MAX_DEVICE_HEX_BYTES || !is_lowercase_hex(&record.device_id) {
        return Err(IdentityError::Corrupt);
    }
    if record.transition_nonce.len() != MAX_NONCE_HEX_BYTES
        || !is_lowercase_hex(&record.transition_nonce)
    {
        return Err(IdentityError::Corrupt);
    }
    if record.slot.len() != MAX_NONCE_HEX_BYTES || !is_lowercase_hex(&record.slot) {
        return Err(IdentityError::Corrupt);
    }
    if record.binding_hash.len() != MAX_FINGERPRINT_BYTES || !is_lowercase_hex(&record.binding_hash)
    {
        return Err(IdentityError::Corrupt);
    }
    Ok(())
}

fn decode_exact_hex<const N: usize>(raw: &str) -> Result<[u8; N], IdentityError> {
    if raw.len() != N * 2 || !is_lowercase_hex(raw) {
        return Err(IdentityError::Corrupt);
    }
    let mut out = [0_u8; N];
    for (index, chunk) in raw.as_bytes().chunks(2).enumerate() {
        let hi = hex_nibble(chunk[0])?;
        let lo = hex_nibble(chunk[1])?;
        out[index] = (hi << 4) | lo;
    }
    Ok(out)
}

fn decode_hex_bounded(raw: &str, max_bytes: usize) -> Result<Vec<u8>, IdentityError> {
    if raw.len() % 2 != 0 || raw.len() / 2 > max_bytes || !is_lowercase_hex(raw) {
        return Err(IdentityError::Corrupt);
    }
    let mut out = Vec::with_capacity(raw.len() / 2);
    for chunk in raw.as_bytes().chunks(2) {
        out.push((hex_nibble(chunk[0])? << 4) | hex_nibble(chunk[1])?);
    }
    Ok(out)
}

fn hex_nibble(byte: u8) -> Result<u8, IdentityError> {
    match byte {
        b'0'..=b'9' => Ok(byte - b'0'),
        b'a'..=b'f' => Ok(byte - b'a' + 10),
        _ => Err(IdentityError::Corrupt),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connect::identity::{IdentityCommand, IdentityOp, CONNECT_IDENTITY_SCHEMA_VERSION};
    use crate::connect::identity_store::{InMemoryIdentityPersistence, IsolatedRemoteStore};
    use crate::domain::id::CommandId;

    fn temp_vault_root(label: &str) -> PathBuf {
        let mut root = std::env::temp_dir();
        root.push(format!(
            "devmanager-host-vault-{}-{}-{}",
            label,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        // Ensure absolute for open().
        fs::create_dir_all(&root).ok();
        let absolute = root.canonicalize().unwrap_or(root);
        let _ = fs::remove_dir_all(&absolute);
        absolute
    }

    fn binding_for(profile: &str) -> MachineBinding {
        derive_machine_binding(profile).expect("named profile binding")
    }

    fn open_vault(root: PathBuf, profile: &str) -> OsConnectHostVault {
        OsConnectHostVault::open(root, binding_for(profile)).expect("open vault")
    }

    #[test]
    fn derive_machine_binding_domain_tags_profile_fingerprint() {
        let binding = binding_for("host-vault-binding");
        let fingerprint =
            crate::host::profile_fingerprint_for_named_profile("host-vault-binding").unwrap();
        assert_ne!(binding.binding_hash(), fingerprint.to_hex());
        assert_ne!(
            MachineBinding::new(fingerprint.to_hex()).binding_hash(),
            binding.binding_hash()
        );
        let production = derive_machine_binding(PRODUCTION_HOST_PROFILE).expect("production");
        assert_eq!(production.binding_hash().len(), 64);
    }

    #[test]
    fn resolve_host_profile_falls_back_to_production_name() {
        let resolved = resolve_host_profile_for_binding().expect("resolve");
        assert!(!resolved.is_empty());
        // Named validation must accept the packaged production profile token.
        assert!(
            crate::host::profile_fingerprint_for_named_profile(PRODUCTION_HOST_PROFILE).is_ok()
        );
    }

    #[test]
    fn open_rejects_relative_root() {
        let err = OsConnectHostVault::open(
            PathBuf::from("relative-vault"),
            binding_for("host-vault-relative"),
        );
        assert!(matches!(err, Err(IdentityError::Corrupt)));
    }

    #[cfg(windows)]
    #[test]
    fn establish_recover_idempotent_same_public_and_commit_retry() {
        let root = temp_vault_root("establish");
        let profile = "host-vault-establish";
        let mut vault = open_vault(root.clone(), profile);
        let host = HostPublicId::new();
        let nonce = generate_transition_nonce().expect("nonce");
        let first = vault.establish_host(host, nonce).expect("establish");
        let again = vault.establish_host(host, nonce).expect("idempotent");
        assert_eq!(first.proof().fingerprint(), again.proof().fingerprint());
        assert_eq!(first.slot(), again.slot());
        assert!(!vault.host_establishment_committed(&first).unwrap());
        vault.commit_host_establishment(&first).expect("commit");
        assert!(vault.host_establishment_committed(&first).unwrap());
        vault
            .commit_host_establishment(&first)
            .expect("commit retry");
        let recovered = vault
            .recover_host_establishment(host, nonce)
            .expect("recover")
            .expect("pending/committed handle");
        assert_eq!(recovered.proof().fingerprint(), first.proof().fingerprint());
        let _ = fs::remove_dir_all(&root);
    }

    #[cfg(windows)]
    #[test]
    fn wrong_nonce_host_profile_and_proof_reject() {
        let root = temp_vault_root("reject");
        let mut vault = open_vault(root.clone(), "host-vault-reject");
        let host = HostPublicId::new();
        let nonce = generate_transition_nonce().expect("nonce");
        let handle = vault.establish_host(host, nonce).expect("establish");
        vault.commit_host_establishment(&handle).expect("commit");

        let other_nonce = generate_transition_nonce().expect("other nonce");
        assert!(vault
            .recover_host_establishment(host, other_nonce)
            .unwrap()
            .is_none());
        let other_host = HostPublicId::new();
        assert!(vault
            .recover_host_establishment(other_host, nonce)
            .unwrap()
            .is_none());

        let wrong_binding = open_vault(root.clone(), "host-vault-reject-other");
        assert!(matches!(
            wrong_binding.verify_host(host, handle.proof()),
            Err(IdentityError::MissingCredentialProof)
                | Err(IdentityError::CopiedProfile)
                | Err(IdentityError::Corrupt)
        ));

        let forged = HostKeyProof::from_parts(host, 1, "ab".repeat(32));
        assert!(matches!(
            vault.verify_host(host, &forged),
            Err(IdentityError::MissingCredentialProof) | Err(IdentityError::Corrupt)
        ));
        let _ = fs::remove_dir_all(&root);
    }

    #[cfg(windows)]
    #[test]
    fn rollback_pending_only_and_restart_load_same_static() {
        let root = temp_vault_root("rollback");
        let profile = "host-vault-rollback";
        let mut vault = open_vault(root.clone(), profile);
        let host = HostPublicId::new();
        let nonce = generate_transition_nonce().expect("nonce");
        let pending = vault.establish_host(host, nonce).expect("establish");
        vault
            .rollback_host_establishment(&pending)
            .expect("rollback pending");
        assert!(vault
            .recover_host_establishment(host, nonce)
            .unwrap()
            .is_none());

        let handle = vault.establish_host(host, nonce).expect("re-establish");
        vault.commit_host_establishment(&handle).expect("commit");
        assert!(matches!(
            vault.rollback_host_establishment(&handle),
            Err(IdentityError::Corrupt)
        ));

        let identity = ConnectIdentity {
            schema_version: CONNECT_IDENTITY_SCHEMA_VERSION,
            host_public_id: host,
            host_key: KeyReference::from_host_proof(handle.proof()).expect("key"),
            pairing_code: crate::connect::identity::PairingCode::parse_valid("ABCDEFGH")
                .expect("pairing"),
            pairing_code_generation: 1,
            pairing_purpose: crate::connect::identity::PairingPurpose::OwnerDevice,
            profile_binding_hash: binding_for(profile).binding_hash(),
            last_seen_host_build: None,
            created_at_epoch_ms: 1,
            devices: Vec::new(),
        };
        let first = vault.load_host_noise(&identity).expect("load");
        drop(vault);
        let vault = open_vault(root.clone(), profile);
        let second = vault.load_host_noise(&identity).expect("reload");
        assert_eq!(first.public(), second.public());
        let _ = fs::remove_dir_all(&root);
    }

    #[cfg(windows)]
    #[test]
    fn missing_corrupt_oversize_wrong_fingerprint_never_mint() {
        let root = temp_vault_root("fail-closed");
        let profile = "host-vault-fail-closed";
        let vault = open_vault(root.clone(), profile);
        let host = HostPublicId::new();
        let identity = ConnectIdentity {
            schema_version: CONNECT_IDENTITY_SCHEMA_VERSION,
            host_public_id: host,
            host_key: KeyReference {
                location: crate::connect::identity::CredentialLocation::OsHostVault,
                fingerprint: "cd".repeat(32),
                generation: Some(1),
            },
            pairing_code: crate::connect::identity::PairingCode::parse_valid("ABCDEFGH")
                .expect("pairing"),
            pairing_code_generation: 1,
            pairing_purpose: crate::connect::identity::PairingPurpose::OwnerDevice,
            profile_binding_hash: binding_for(profile).binding_hash(),
            last_seen_host_build: None,
            created_at_epoch_ms: 1,
            devices: Vec::new(),
        };
        assert!(matches!(
            vault.load_host_noise(&identity),
            Err(IdentityError::MissingCredentialProof)
        ));
        assert!(!root.exists(), "missing load must not create vault root");

        fs::create_dir_all(&root).unwrap();
        let path = root.join(format!(
            "h-{}-n-{}.json",
            hex_encode(host.as_bytes()),
            "11".repeat(16)
        ));
        fs::write(&path, b"{not-json").unwrap();
        assert!(matches!(
            vault.load_host_noise(&identity),
            Err(IdentityError::Corrupt) | Err(IdentityError::MissingCredentialProof)
        ));

        let oversize = vec![b'a'; MAX_IDENTITY_PHYSICAL_BYTES + 8];
        fs::write(&path, &oversize).unwrap();
        assert!(matches!(
            read_record_nofollow(&path),
            Err(IdentityError::LimitExceeded {
                field: IdentityLimitField::PhysicalBytes
            })
        ));
        let _ = fs::remove_dir_all(&root);
    }

    #[cfg(windows)]
    #[test]
    fn matching_corrupt_slot_and_nonce_filename_mismatch_fail_closed() {
        let root = temp_vault_root("corrupt-match");
        let profile = "host-vault-corrupt-match";
        let mut vault = open_vault(root.clone(), profile);
        let host = HostPublicId::new();
        let nonce = generate_transition_nonce().unwrap();
        let handle = vault.establish_host(host, nonce).expect("establish");
        vault.commit_host_establishment(&handle).expect("commit");

        let good_path = root.join(OsConnectHostVault::slot_filename(host, nonce));
        let mut record: serde_json::Value =
            serde_json::from_slice(&fs::read(&good_path).unwrap()).unwrap();
        // Filename host matches but record nonce is swapped → fail closed.
        let other_nonce = generate_transition_nonce().unwrap();
        record["transitionNonce"] = serde_json::Value::String(hex_encode(&other_nonce));
        fs::write(&good_path, serde_json::to_vec(&record).unwrap()).unwrap();
        assert!(matches!(
            vault.verify_host(host, handle.proof()),
            Err(IdentityError::Corrupt)
        ));
        let _ = fs::remove_dir_all(&root);
    }

    #[cfg(windows)]
    #[test]
    fn duplicate_committed_proof_slots_fail_closed() {
        let root = temp_vault_root("duplicate");
        let profile = "host-vault-duplicate";
        let mut vault = open_vault(root.clone(), profile);
        let host = HostPublicId::new();
        let nonce = generate_transition_nonce().unwrap();
        let handle = vault.establish_host(host, nonce).expect("establish");
        vault.commit_host_establishment(&handle).expect("commit");
        let path = root.join(OsConnectHostVault::slot_filename(host, nonce));
        let bytes = fs::read(&path).unwrap();
        let other_nonce = generate_transition_nonce().unwrap();
        let dup_path = root.join(OsConnectHostVault::slot_filename(host, other_nonce));
        // Copy bytes then patch filename fields to claim the other nonce while
        // keeping the same fingerprint — enumeration must fail on duplicates
        // or filename/record mismatch.
        let mut value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        value["transitionNonce"] = serde_json::Value::String(hex_encode(&other_nonce));
        fs::write(&dup_path, serde_json::to_vec(&value).unwrap()).unwrap();
        assert!(matches!(
            vault.verify_host(host, handle.proof()),
            Err(IdentityError::Corrupt)
        ));
        let _ = fs::remove_dir_all(&root);
    }

    #[cfg(windows)]
    #[test]
    fn commit_retry_with_corrupt_ciphertext_never_reports_success() {
        let root = temp_vault_root("bad-cipher");
        let profile = "host-vault-bad-cipher";
        let mut vault = open_vault(root.clone(), profile);
        let host = HostPublicId::new();
        let nonce = generate_transition_nonce().unwrap();
        let handle = vault.establish_host(host, nonce).expect("establish");
        let path = root.join(OsConnectHostVault::slot_filename(host, nonce));
        let mut value: serde_json::Value =
            serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        value["ciphertext"] = serde_json::Value::String("ab".repeat(64));
        fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
        assert!(matches!(
            vault.commit_host_establishment(&handle),
            Err(IdentityError::Corrupt) | Err(IdentityError::CopiedProfile)
        ));
        // Slot must not have been marked committed after decrypt failure.
        let reloaded: HostVaultRecord = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(reloaded.state, HostVaultState::Prepared);
        let _ = fs::remove_dir_all(&root);
    }

    #[cfg(windows)]
    #[test]
    fn root_reparse_point_rejected_on_open_or_establish() {
        let base = temp_vault_root("reparse-base");
        fs::create_dir_all(&base).unwrap();
        let target = base.join("target");
        let junction = base.join("junction-root");
        fs::create_dir_all(&target).unwrap();
        let linked = std::os::windows::fs::symlink_dir(&target, &junction);
        if linked.is_err() {
            let _ = fs::remove_dir_all(&base);
            return;
        }
        let binding = binding_for("host-vault-reparse");
        match OsConnectHostVault::open(junction.clone(), binding.clone()) {
            Err(IdentityError::Corrupt) => {}
            Ok(mut vault) => {
                let err =
                    vault.establish_host(HostPublicId::new(), generate_transition_nonce().unwrap());
                assert!(
                    matches!(
                        err,
                        Err(IdentityError::Corrupt) | Err(IdentityError::PersistFailed)
                    ),
                    "establish through reparse root must fail: {err:?}"
                );
            }
            Err(err) => panic!("unexpected open error: {err:?}"),
        }
        let _ = fs::remove_dir_all(&base);
    }

    #[cfg(windows)]
    #[test]
    fn unsupported_rotation_and_device_repair_ops() {
        let root = temp_vault_root("unsupported");
        let mut vault = open_vault(root.clone(), "host-vault-unsupported");
        let host = HostPublicId::new();
        let nonce = generate_transition_nonce().unwrap();
        assert!(matches!(
            vault.prepare_host_rotation(host, nonce),
            Err(IdentityError::UnsupportedOperation)
        ));
        assert!(matches!(
            vault.prepare_device_repair(DeviceId::new(), DeviceKind::Native, nonce),
            Err(IdentityError::UnsupportedOperation)
        ));
        assert!(matches!(
            vault.establish_device(DeviceId::new(), DeviceKind::Native, nonce),
            Err(IdentityError::MissingCredentialProof)
        ));
        let _ = fs::remove_dir_all(&root);
    }

    #[cfg(windows)]
    #[test]
    fn identity_store_enable_with_vault_reaches_committed_custody() {
        let root = temp_vault_root("enable");
        let profile = "host-vault-enable";
        let binding = binding_for(profile);
        let mut vault = open_vault(root.clone(), profile);
        let mut store =
            IsolatedRemoteStore::new(InMemoryIdentityPersistence::default()).expect("memory store");
        let receipt = store
            .execute(
                &binding,
                &mut vault,
                IdentityCommand {
                    command_id: CommandId::new(),
                    expected_revision: 0,
                    op: IdentityOp::Enable {
                        host_build: 1,
                        now_epoch_ms: 42,
                    },
                },
            )
            .expect("enable");
        let setup = receipt.setup().expect("setup");
        let loaded = store.load(&binding, &mut vault).expect("load identity");
        let identity = loaded.identity().expect("enabled").clone();
        assert_eq!(
            identity.host_key().fingerprint(),
            setup.host_key.fingerprint()
        );
        let proof = HostKeyProof::from_parts(
            identity.host_public_id(),
            identity.host_key().generation().unwrap(),
            identity.host_key().fingerprint().to_string(),
        );
        vault
            .verify_host(identity.host_public_id(), &proof)
            .expect("enable must leave committed verifiable custody");
        let custody = vault.load_host_noise(&identity).expect("load custody");
        assert_eq!(
            public_fingerprint(&custody.public()),
            identity.host_key().fingerprint()
        );
        // Enumerate committed slot and assert host_establishment_committed.
        let names = list_vault_entry_names_bounded(&root).expect("list");
        let slot_name = names
            .into_iter()
            .find(|name| is_safe_host_slot_filename(name))
            .expect("committed slot file");
        let record = read_record_nofollow(&root.join(&slot_name)).expect("read slot");
        assert_eq!(record.state, HostVaultState::Committed);
        let handle = vault
            .handle_from_verified_record(&record)
            .expect("verified handle");
        assert!(vault
            .host_establishment_committed(&handle)
            .expect("committed query"));
        let _ = fs::remove_dir_all(&root);
    }

    #[cfg(windows)]
    fn noise_device_peer_against_host() -> AuthenticatedPeer {
        use crate::protocol::{
            instantiate_noise_channel, ChannelRole, CredentialPurpose, CryptoPrologue,
            NoiseCustody, NoiseIdentityBinding, NOISE_FIRST_PAIRING_PATTERN, PROTOCOL_MAJOR,
        };
        let device_keys = NoiseCustody::generate().expect("device keys");
        let host_keys = NoiseCustody::generate().expect("host keys");
        let prologue = CryptoPrologue::new(
            PROTOCOL_MAJOR,
            CredentialPurpose::OwnerPairing,
            [3; 16],
            [4; 16],
        )
        .expect("prologue");
        let mut device = instantiate_noise_channel(
            NOISE_FIRST_PAIRING_PATTERN,
            true,
            device_keys.private(),
            device_keys.public(),
            None,
            prologue,
            ChannelRole::Initiator,
            NoiseIdentityBinding::host_device([0x11; 16], [0x22; 16]),
            40,
            true,
        )
        .expect("device handshake");
        let mut host = instantiate_noise_channel(
            NOISE_FIRST_PAIRING_PATTERN,
            true,
            host_keys.private(),
            host_keys.public(),
            None,
            prologue,
            ChannelRole::Responder,
            NoiseIdentityBinding::host([0x33; 16]),
            40,
            true,
        )
        .expect("host handshake");
        let msg1 = device.write_message().expect("msg1");
        host.read_message(&msg1).expect("read1");
        let msg2 = host.write_message().expect("msg2");
        device.read_message(&msg2).expect("read2");
        let msg3 = device.write_message().expect("msg3");
        host.read_message(&msg3).expect("read3");
        let peer = host.finish().expect("host finish").remote_peer();
        assert!(peer.is_device());
        assert_eq!(peer.static_public(), device_keys.public());
        peer
    }

    #[cfg(windows)]
    fn noise_host_peer_against_host() -> AuthenticatedPeer {
        use crate::protocol::{
            instantiate_noise_channel, ChannelRole, CredentialPurpose, CryptoPrologue,
            NoiseCustody, NoiseIdentityBinding, NOISE_FIRST_PAIRING_PATTERN, PROTOCOL_MAJOR,
        };
        let left_keys = NoiseCustody::generate().expect("left");
        let right_keys = NoiseCustody::generate().expect("right");
        let prologue = CryptoPrologue::new(
            PROTOCOL_MAJOR,
            CredentialPurpose::OwnerPairing,
            [5; 16],
            [6; 16],
        )
        .expect("prologue");
        let mut left = instantiate_noise_channel(
            NOISE_FIRST_PAIRING_PATTERN,
            true,
            left_keys.private(),
            left_keys.public(),
            None,
            prologue,
            ChannelRole::Initiator,
            NoiseIdentityBinding::host([0x44; 16]),
            50,
            true,
        )
        .expect("left");
        let mut right = instantiate_noise_channel(
            NOISE_FIRST_PAIRING_PATTERN,
            true,
            right_keys.private(),
            right_keys.public(),
            None,
            prologue,
            ChannelRole::Responder,
            NoiseIdentityBinding::host([0x55; 16]),
            50,
            true,
        )
        .expect("right");
        let msg1 = left.write_message().expect("msg1");
        right.read_message(&msg1).expect("read1");
        let msg2 = right.write_message().expect("msg2");
        left.read_message(&msg2).expect("read2");
        let msg3 = left.write_message().expect("msg3");
        right.read_message(&msg3).expect("read3");
        let peer = right.finish().expect("finish").remote_peer();
        assert!(peer.is_host());
        peer
    }

    #[cfg(windows)]
    fn tempfile_vault(label: &str) -> (tempfile::TempDir, OsConnectHostVault) {
        let temp = tempfile::Builder::new()
            .prefix(&format!("devmanager-device-enroll-{label}-"))
            .tempdir()
            .expect("tempdir");
        let root = temp
            .path()
            .canonicalize()
            .unwrap_or_else(|_| temp.path().to_path_buf());
        let vault = open_vault(root, &format!("host-vault-device-{label}"));
        (temp, vault)
    }

    #[cfg(windows)]
    #[test]
    fn authorize_rejects_host_kind_noise_peer() {
        let (_temp, mut vault) = tempfile_vault("host-reject");
        let host_peer = noise_host_peer_against_host();
        assert!(matches!(
            vault.authorize_device_enrollment(host_peer, DeviceKind::Native),
            Err(IdentityError::InvalidDevice)
        ));
        let nonce = generate_transition_nonce().unwrap();
        assert!(matches!(
            vault.establish_device(DeviceId::new(), DeviceKind::Native, nonce),
            Err(IdentityError::MissingCredentialProof)
        ));
    }

    #[cfg(windows)]
    #[test]
    fn device_enrollment_commit_retry_reload_and_no_private_material() {
        let (temp, mut vault) = tempfile_vault("enroll");
        let peer = noise_device_peer_against_host();
        vault
            .authorize_device_enrollment(peer, DeviceKind::Native)
            .expect("authorize device");
        let device_id = DeviceId::new();
        let nonce = generate_transition_nonce().unwrap();
        let first = vault
            .establish_device(device_id, DeviceKind::Native, nonce)
            .expect("establish");
        assert_eq!(
            first.proof().fingerprint(),
            public_fingerprint(&peer.static_public())
        );
        assert!(!vault.device_establishment_committed(&first).unwrap());
        let again = vault
            .establish_device(device_id, DeviceKind::Native, nonce)
            .expect("exact retry");
        assert_eq!(first.proof().fingerprint(), again.proof().fingerprint());
        assert_eq!(first.slot(), again.slot());
        vault.commit_device_establishment(&first).expect("commit");
        vault
            .commit_device_establishment(&first)
            .expect("commit retry");
        assert!(vault.device_establishment_committed(&first).unwrap());
        vault
            .verify_device(device_id, first.proof())
            .expect("verify");

        let slot = vault.device_slot_path(device_id, nonce).expect("slot path");
        let raw = fs::read_to_string(&slot).expect("read device slot");
        assert!(!raw.contains("ciphertext"));
        assert!(!raw.contains("privateKey"));
        let record: DeviceVaultRecord = serde_json::from_str(&raw).expect("parse");
        assert!(!record.contains_private_material());
        assert_eq!(record.state, DeviceVaultState::Committed);

        drop(vault);
        let mut vault = open_vault(
            temp.path()
                .canonicalize()
                .unwrap_or_else(|_| temp.path().to_path_buf()),
            "host-vault-device-enroll",
        );
        let recovered = vault
            .recover_device_establishment(device_id, nonce)
            .expect("recover")
            .expect("handle");
        assert_eq!(recovered.proof().fingerprint(), first.proof().fingerprint());
        vault
            .verify_device(device_id, recovered.proof())
            .expect("reload verify");
    }

    #[cfg(windows)]
    #[test]
    fn foreign_nonce_and_different_key_on_occupied_id_reject() {
        let (_temp, mut vault) = tempfile_vault("foreign");
        let peer = noise_device_peer_against_host();
        vault
            .authorize_device_enrollment(peer, DeviceKind::Native)
            .expect("authorize");
        let device_id = DeviceId::new();
        let nonce = generate_transition_nonce().unwrap();
        let handle = vault
            .establish_device(device_id, DeviceKind::Native, nonce)
            .expect("establish");
        vault.commit_device_establishment(&handle).expect("commit");

        let other_nonce = generate_transition_nonce().unwrap();
        assert!(vault
            .recover_device_establishment(device_id, other_nonce)
            .unwrap()
            .is_none());

        let other_peer = noise_device_peer_against_host();
        vault
            .authorize_device_enrollment(other_peer, DeviceKind::Native)
            .expect("authorize other");
        assert!(matches!(
            vault.establish_device(device_id, DeviceKind::Native, other_nonce),
            Err(IdentityError::DuplicateDevice)
        ));
    }

    #[cfg(windows)]
    #[test]
    fn revocation_blocks_verify_and_exact_epoch_restore() {
        let (_temp, mut vault) = tempfile_vault("revoke");
        let peer = noise_device_peer_against_host();
        vault
            .authorize_device_enrollment(peer, DeviceKind::Native)
            .expect("authorize");
        let device_id = DeviceId::new();
        let nonce = generate_transition_nonce().unwrap();
        let handle = vault
            .establish_device(device_id, DeviceKind::Native, nonce)
            .expect("establish");
        vault.commit_device_establishment(&handle).expect("commit");
        vault
            .invalidate_device_credential(device_id, 7)
            .expect("invalidate");
        assert!(matches!(
            vault.verify_device(device_id, handle.proof()),
            Err(IdentityError::MissingCredentialProof)
        ));
        assert!(matches!(
            vault.restore_device_credential(device_id, 3),
            Err(IdentityError::Corrupt)
        ));
        vault
            .invalidate_device_credential(device_id, 9)
            .expect("newer invalidate");
        assert!(matches!(
            vault.restore_device_credential(device_id, 7),
            Err(IdentityError::Corrupt)
        ));
        vault
            .restore_device_credential(device_id, 9)
            .expect("exact restore");
        vault
            .verify_device(device_id, handle.proof())
            .expect("restored verify");
    }

    #[cfg(windows)]
    #[test]
    fn rollback_removes_only_exact_uncommitted_device_slot() {
        let (_temp, mut vault) = tempfile_vault("rollback-device");
        let peer = noise_device_peer_against_host();
        vault
            .authorize_device_enrollment(peer, DeviceKind::Native)
            .expect("authorize");
        let device_id = DeviceId::new();
        let nonce = generate_transition_nonce().unwrap();
        let pending = vault
            .establish_device(device_id, DeviceKind::Native, nonce)
            .expect("establish");
        vault
            .rollback_device_establishment(&pending)
            .expect("rollback prepared");
        assert!(vault
            .recover_device_establishment(device_id, nonce)
            .unwrap()
            .is_none());

        let peer = noise_device_peer_against_host();
        vault
            .authorize_device_enrollment(peer, DeviceKind::Native)
            .expect("re-authorize");
        let handle = vault
            .establish_device(device_id, DeviceKind::Native, nonce)
            .expect("re-establish");
        vault.commit_device_establishment(&handle).expect("commit");
        assert!(matches!(
            vault.rollback_device_establishment(&handle),
            Err(IdentityError::Corrupt)
        ));
    }
}
