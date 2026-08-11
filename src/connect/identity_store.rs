//! Isolated identity persistence facade.
//!
//! This slice exposes only an isolated in-memory persistence seam. It never
//! follows filesystem paths, opens `%APPDATA%`, or claims OS/WebCrypto
//! custody; a real production adapter remains an explicit future gate.

use std::fmt;

use super::identity::{
    rotate_pairing_until_changed, seed_pairing_code, validate_device_record, BrowserPrivateStorage,
    ConnectIdentity, CredentialVault, DeviceKeyProof, DeviceKind, DeviceRecord,
    HostIdentityRotation, HostKeyProof, HostPublicId, IdentityCommand, IdentityError,
    IdentityLimitField, IdentityOp, IdentityReceipt, IdentitySetup, KeyReference, MachineBinding,
    PairingPurpose, PendingIdentityTransition, PendingIdentityTransitionKind, RegisterDevice,
    CONNECT_IDENTITY_SCHEMA_VERSION, MAX_IDENTITY_DEVICES, MAX_IDENTITY_PHYSICAL_BYTES,
    MAX_IDENTITY_RECEIPTS, MAX_LABEL_BYTES,
};
use super::identity_codec::{
    decode_identity_bytes, device_receipt, empty_receipt, enable_receipt, encode_identity_document,
    host_rotation_receipt, pairing_receipt, scan_bounded_json, IdentityDocument,
};

/// Isolated persistence seam used by this contract slice.
///
/// The trait deliberately has no caller-supplied authority or production
/// alias. This module has no production `remote.json` implementation; a
/// future production adapter must be introduced behind an explicit reviewed
/// boundary instead of claiming authority through this test seam.
pub trait IdentityPersistence {
    /// Monotonic persistence CAS epoch. A pending marker may consume an
    /// epoch without consuming the document's logical identity revision.
    fn current_revision(&self) -> u64;
    fn read_bounded(&self, max_bytes: usize) -> Result<Option<Vec<u8>>, IdentityError>;
    /// Atomically replace the bytes iff `expected_revision` is current.
    /// Implementations must leave both bytes and revision unchanged on error.
    fn compare_and_swap(
        &mut self,
        expected_revision: u64,
        bytes: &[u8],
    ) -> Result<u64, IdentityError>;
    /// Atomically replace a durable transition marker. The persistence CAS
    /// epoch advances, but the marker keeps the logical identity revision
    /// unchanged. Implementations must leave both bytes and epoch unchanged
    /// on error.
    fn replace_pending(
        &mut self,
        expected_revision: u64,
        bytes: &[u8],
    ) -> Result<u64, IdentityError>;
}

#[derive(Clone, Default)]
pub struct InMemoryIdentityPersistence {
    bytes: Option<Vec<u8>>,
    revision: u64,
}

impl fmt::Debug for InMemoryIdentityPersistence {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("InMemoryIdentityPersistence")
            .field("revision", &self.revision)
            .field("has_bytes", &self.bytes.is_some())
            .finish()
    }
}

impl InMemoryIdentityPersistence {
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, IdentityError> {
        if bytes.len() > MAX_IDENTITY_PHYSICAL_BYTES {
            return Err(IdentityError::LimitExceeded {
                field: IdentityLimitField::PhysicalBytes,
            });
        }
        let revision = decode_identity_bytes(bytes)
            .map(|document| {
                if document.cas_epoch > 0 {
                    document.cas_epoch
                } else {
                    document.revision
                }
            })
            .unwrap_or(0);
        Ok(Self {
            bytes: Some(bytes.to_vec()),
            revision,
        })
    }

    #[cfg(test)]
    #[doc(hidden)]
    pub fn from_unchecked_oversize(bytes: Vec<u8>) -> Self {
        Self {
            bytes: Some(bytes),
            revision: 0,
        }
    }

    #[cfg(test)]
    #[doc(hidden)]
    pub fn snapshot_bytes(&self) -> Option<Vec<u8>> {
        self.bytes.clone()
    }

    #[cfg(test)]
    #[doc(hidden)]
    pub fn replace_bytes_for_test(&mut self, bytes: Vec<u8>) -> Result<(), IdentityError> {
        if bytes.len() > MAX_IDENTITY_PHYSICAL_BYTES {
            return Err(IdentityError::LimitExceeded {
                field: IdentityLimitField::PhysicalBytes,
            });
        }
        self.bytes = Some(bytes);
        Ok(())
    }
}

impl IdentityPersistence for InMemoryIdentityPersistence {
    fn current_revision(&self) -> u64 {
        self.revision
    }

    fn read_bounded(&self, max_bytes: usize) -> Result<Option<Vec<u8>>, IdentityError> {
        match &self.bytes {
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
        if bytes.len() > MAX_IDENTITY_PHYSICAL_BYTES {
            return Err(IdentityError::LimitExceeded {
                field: IdentityLimitField::PhysicalBytes,
            });
        }
        if self.revision != expected_revision {
            return Err(IdentityError::RevisionConflict);
        }
        let next_revision = self
            .revision
            .checked_add(1)
            .ok_or(IdentityError::Overflow)?;
        self.bytes = Some(bytes.to_vec());
        self.revision = next_revision;
        Ok(self.revision)
    }

    fn replace_pending(
        &mut self,
        expected_revision: u64,
        bytes: &[u8],
    ) -> Result<u64, IdentityError> {
        if bytes.len() > MAX_IDENTITY_PHYSICAL_BYTES {
            return Err(IdentityError::LimitExceeded {
                field: IdentityLimitField::PhysicalBytes,
            });
        }
        if self.revision != expected_revision {
            return Err(IdentityError::RevisionConflict);
        }
        let next_revision = self
            .revision
            .checked_add(1)
            .ok_or(IdentityError::Overflow)?;
        self.bytes = Some(bytes.to_vec());
        self.revision = next_revision;
        Ok(self.revision)
    }
}

#[cfg(test)]
impl InMemoryIdentityPersistence {
    #[doc(hidden)]
    pub fn set_revision_for_test(&mut self, revision: u64) {
        self.revision = revision;
    }
}

#[derive(Clone)]
pub struct IsolatedRemoteStore<P> {
    persistence: P,
}

impl<P> fmt::Debug for IsolatedRemoteStore<P> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("IsolatedRemoteStore(redacted)")
    }
}

#[derive(Clone)]
enum VaultTransition {
    HostEstablishment {
        host_id: HostPublicId,
        proof: HostKeyProof,
    },
    DeviceEstablishment {
        device_id: super::identity::DeviceId,
        proof: DeviceKeyProof,
    },
    HostRotation,
}

impl<P: IdentityPersistence + 'static> IsolatedRemoteStore<P> {
    #[cfg(test)]
    pub fn new(persistence: P) -> Result<Self, IdentityError> {
        Ok(Self { persistence })
    }

    #[cfg(not(test))]
    pub fn new(persistence: P) -> Result<Self, IdentityError> {
        // HOLD: Portal/relay/OS-vault production adapter is not in this slice.
        // Debug apps are not test authority; in-memory cannot be custody.
        let _ = persistence;
        Err(IdentityError::ProductionStoreForbidden)
    }

    pub fn persistence(&self) -> &P {
        &self.persistence
    }

    /// Mutable persistence is a debug/test-only fault-injection seam. A
    /// production caller cannot replace the stored bytes through this facade.
    #[cfg(test)]
    #[doc(hidden)]
    pub fn persistence_mut(&mut self) -> &mut P {
        &mut self.persistence
    }

    pub fn load<V: CredentialVault>(
        &mut self,
        binding: &MachineBinding,
        vault: &V,
    ) -> Result<LoadedRemoteDocument, IdentityError> {
        let document = self.read_document()?;
        if let Some(pending) = document.pending_transition {
            // Enable/Register keep any already-committed identity visible.
            // RotateHost may have written a next key that is not vault-committed;
            // do not treat that key as live authority until retry settles.
            if matches!(
                pending.kind,
                PendingIdentityTransitionKind::RotateHostIdentity
            ) {
                if let Some(identity) = &document.identity {
                    identity.validate_structure()?;
                    if identity.profile_binding_hash != binding.binding_hash() {
                        return Err(IdentityError::CopiedProfile);
                    }
                }
                return Ok(LoadedRemoteDocument {
                    document: IdentityDocument {
                        identity: None,
                        ..document
                    },
                });
            }
            if let Some(identity) = &document.identity {
                verify_bound_identity(identity, binding, vault)?;
            }
            return Ok(LoadedRemoteDocument { document });
        }
        if let Some(identity) = &document.identity {
            verify_bound_identity(identity, binding, vault)?;
        }
        Ok(LoadedRemoteDocument { document })
    }

    pub fn recover_corrupt<V: CredentialVault>(
        &mut self,
        binding: &MachineBinding,
        vault: &V,
    ) -> Result<LoadedRemoteDocument, IdentityError> {
        let observed_persistence_revision = self.persistence.current_revision();
        match self.load(binding, vault) {
            Ok(document) => Ok(document),
            Err(IdentityError::CopiedProfile) => Err(IdentityError::CopiedProfile),
            Err(error) if recoverable_identity_corruption(&error) => {
                let mut document = self.read_document_stripped()?;
                document.identity = None;
                document.receipts.clear();
                document.requires_explicit_reestablish = true;
                document.revision = document
                    .revision
                    .checked_add(1)
                    .ok_or(IdentityError::Overflow)?;
                document.cas_epoch = observed_persistence_revision
                    .checked_add(1)
                    .ok_or(IdentityError::Overflow)?;
                let encoded = encode_identity_document(&document)?;
                self.persistence
                    .compare_and_swap(observed_persistence_revision, &encoded)?;
                Ok(LoadedRemoteDocument { document })
            }
            Err(error) => Err(error),
        }
    }

    /// Explicitly abandon an interrupted vault transition and require setup
    /// again. Already-created vault slots named by the pending marker are
    /// rolled back first; a rollback failure leaves the pending marker.
    pub fn abandon_pending_transition<V: CredentialVault>(
        &mut self,
        binding: &MachineBinding,
        vault: &mut V,
    ) -> Result<LoadedRemoteDocument, IdentityError> {
        let document = self.read_document()?;
        let pending = document
            .pending_transition
            .ok_or(IdentityError::TransitionPending)?;
        if let Some(identity) = &document.identity {
            identity.validate_structure()?;
            if identity.profile_binding_hash != binding.binding_hash() {
                return Err(IdentityError::CopiedProfile);
            }
        }
        let claimed_pending = self.claim_pending_transition()?;
        rollback_pending_vault(vault, &pending)?;
        let observed_after_claim = self.persistence.current_revision();
        let mut document = self.read_document()?;
        if document.pending_transition != Some(claimed_pending) {
            return Err(IdentityError::RevisionConflict);
        }
        document.identity = None;
        document.receipts.clear();
        document.pending_transition = None;
        document.requires_explicit_reestablish = true;
        document.revision = document
            .revision
            .checked_add(1)
            .ok_or(IdentityError::Overflow)?;
        document.cas_epoch = observed_after_claim
            .checked_add(1)
            .ok_or(IdentityError::Overflow)?;
        let encoded = encode_identity_document(&document)?;
        self.persistence
            .compare_and_swap(observed_after_claim, &encoded)?;
        Ok(LoadedRemoteDocument { document })
    }

    pub fn execute<V: CredentialVault>(
        &mut self,
        binding: &MachineBinding,
        vault: &mut V,
        command: IdentityCommand,
    ) -> Result<IdentityReceipt, IdentityError> {
        let observed_persistence_revision = self.persistence.current_revision();
        let mut document = self.read_document()?;
        let command_digest = command.payload_digest();
        let pending_retry = if let Some(pending) = document.pending_transition {
            if let Some(identity) = &document.identity {
                identity.validate_structure()?;
                if identity.profile_binding_hash != binding.binding_hash() {
                    return Err(IdentityError::CopiedProfile);
                }
            }
            if pending.command_id == command.command_id && pending.command_digest == command_digest
            {
                if pending.kind == PendingIdentityTransitionKind::RotateHostIdentity {
                    self.claim_pending_transition()?;
                    document = self.read_document()?;
                }
                if let Some(existing) = document
                    .receipts
                    .iter()
                    .find(|receipt| receipt.command_id() == command.command_id)
                    .cloned()
                {
                    if pending.kind == PendingIdentityTransitionKind::RotateHostIdentity {
                        vault.commit_host_rotation()?;
                    }
                    if let Some(identity) = &document.identity {
                        verify_bound_identity(identity, binding, vault)?;
                    }
                    self.clear_pending_transition(document.revision)?;
                    return Ok(existing);
                }
            } else if pending.command_id == command.command_id {
                return Err(IdentityError::CommandConflict);
            } else {
                return Err(IdentityError::TransitionPending);
            }
            if document.revision != command.expected_revision {
                return Err(IdentityError::RevisionConflict);
            }
            Some(pending)
        } else {
            None
        };
        if pending_retry.is_some() {
            if PendingIdentityTransitionKind::from_operation(&command.op)
                != pending_retry.map(|pending| pending.kind)
            {
                return Err(IdentityError::CommandConflict);
            }
            if let Some(identity) = &document.identity {
                verify_bound_identity(identity, binding, vault)?;
            }
        }
        if pending_retry.is_none() {
            if let Some(identity) = &document.identity {
                verify_bound_identity(identity, binding, vault)?;
            }
            if let Some(existing) = document
                .receipts
                .iter()
                .find(|receipt| receipt.command_id() == command.command_id)
                .cloned()
            {
                if existing.command_digest != Some(command_digest) {
                    return Err(IdentityError::CommandConflict);
                }
                return Ok(existing);
            }
        }
        if document.revision != command.expected_revision {
            return Err(IdentityError::RevisionConflict);
        }
        if pending_retry.is_none() && document.receipts.len() >= MAX_IDENTITY_RECEIPTS {
            if let Some(oldest) = document
                .receipts
                .iter()
                .map(IdentityReceipt::command_id)
                .min()
            {
                if command.command_id <= oldest {
                    return Err(IdentityError::CommandConflict);
                }
            }
        }
        let pending_kind = PendingIdentityTransitionKind::from_operation(&command.op);
        let pending_was_preexisting = pending_retry.is_some();
        let (pending_marker, durable_revision) = if let Some(pending) = pending_retry {
            (Some(pending), command.expected_revision)
        } else if let Some(kind) = pending_kind {
            let pending = PendingIdentityTransition {
                command_id: command.command_id,
                command_digest,
                kind,
                host_public_id: match kind {
                    PendingIdentityTransitionKind::Enable => Some(HostPublicId::new()),
                    PendingIdentityTransitionKind::RegisterDevice
                    | PendingIdentityTransitionKind::RotateHostIdentity => None,
                },
                device_id: match kind {
                    PendingIdentityTransitionKind::RegisterDevice => {
                        Some(super::identity::DeviceId::new())
                    }
                    PendingIdentityTransitionKind::Enable
                    | PendingIdentityTransitionKind::RotateHostIdentity => None,
                },
            };
            let pending_revision =
                self.persist_pending_transition(&document, pending, observed_persistence_revision)?;
            document.revision = pending_revision;
            document.pending_transition = Some(pending);
            (Some(pending), pending_revision)
        } else {
            (None, command.expected_revision)
        };
        let (receipt, transition) = match apply_command(
            &mut document,
            binding,
            vault,
            &command,
            pending_marker.as_ref(),
        ) {
            Ok(result) => result,
            Err(error) => {
                if matches!(
                    pending_kind,
                    Some(PendingIdentityTransitionKind::RotateHostIdentity)
                ) {
                    if let Err(cleanup) = vault.abort_host_rotation() {
                        return Err(cleanup);
                    }
                }
                if error != IdentityError::TransitionRollbackFailed
                    && pending_kind.is_some()
                    && !pending_was_preexisting
                {
                    if self.clear_pending_transition(durable_revision).is_err() {
                        return Err(IdentityError::TransitionRollbackFailed);
                    }
                }
                return Err(error);
            }
        };
        let keep_pending_until_vault_commit =
            matches!(&transition, Some(VaultTransition::HostRotation));
        if !keep_pending_until_vault_commit {
            // Establishment has no second vault commit phase. The marker is
            // needed until this CAS, then the durable identity is complete.
            document.pending_transition = None;
        }
        let expected_cas_revision = if pending_kind.is_some() {
            self.persistence.current_revision()
        } else {
            observed_persistence_revision
        };
        document.cas_epoch = expected_cas_revision
            .checked_add(1)
            .ok_or(IdentityError::Overflow)?;
        let encoded = match encode_identity_document(&document) {
            Ok(bytes) => bytes,
            Err(error) => {
                if rollback_transition(vault, transition).is_ok() {
                    if pending_kind.is_some()
                        && !pending_was_preexisting
                        && self.clear_pending_transition(durable_revision).is_err()
                    {
                        return Err(IdentityError::TransitionRollbackFailed);
                    }
                    return Err(error);
                }
                return Err(IdentityError::TransitionRollbackFailed);
            }
        };
        match self
            .persistence
            .compare_and_swap(expected_cas_revision, &encoded)
        {
            Ok(_physical_revision) => {
                if matches!(&transition, Some(VaultTransition::HostRotation)) {
                    if let Err(error) = vault.commit_host_rotation() {
                        // Identity CAS already landed. Keep pending + vault
                        // rotation so a matching retry can commit.
                        return Err(error);
                    }
                }
                let mut receipt = receipt;
                let logical_revision = document.revision;
                receipt.revision = logical_revision;
                if let Some(stored) = document
                    .receipts
                    .iter_mut()
                    .find(|item| item.command_id() == receipt.command_id())
                {
                    stored.revision = logical_revision;
                }
                if keep_pending_until_vault_commit
                    && self.clear_pending_transition(logical_revision).is_err()
                {
                    // The identity/vault transition committed. Retain the
                    // durable marker so a retry can clear it idempotently.
                    return Err(IdentityError::PersistFailed);
                }
                Ok(receipt)
            }
            Err(error) => {
                if rollback_transition(vault, transition).is_ok() {
                    if pending_kind.is_some()
                        && !pending_was_preexisting
                        && self.clear_pending_transition(durable_revision).is_err()
                    {
                        return Err(IdentityError::TransitionRollbackFailed);
                    }
                    Err(error)
                } else {
                    Err(IdentityError::TransitionRollbackFailed)
                }
            }
        }
    }

    fn claim_pending_transition(&mut self) -> Result<PendingIdentityTransition, IdentityError> {
        let observed_persistence_revision = self.persistence.current_revision();
        let mut document = self.read_document()?;
        let pending = document
            .pending_transition
            .ok_or(IdentityError::TransitionPending)?;
        document.cas_epoch = observed_persistence_revision
            .checked_add(1)
            .ok_or(IdentityError::Overflow)?;
        let encoded = encode_identity_document(&document)?;
        self.persistence
            .replace_pending(observed_persistence_revision, &encoded)?;
        Ok(pending)
    }

    fn persist_pending_transition(
        &mut self,
        original: &IdentityDocument,
        pending: PendingIdentityTransition,
        expected_persistence_revision: u64,
    ) -> Result<u64, IdentityError> {
        let mut marker = original.clone();
        marker.pending_transition = Some(pending);
        marker.revision = original.revision;
        marker.cas_epoch = expected_persistence_revision
            .checked_add(1)
            .ok_or(IdentityError::Overflow)?;
        if marker.identity.is_none() {
            marker.requires_explicit_reestablish = true;
        }
        let encoded = encode_identity_document(&marker)?;
        self.persistence
            .replace_pending(expected_persistence_revision, &encoded)?;
        Ok(original.revision)
    }

    fn clear_pending_transition(&mut self, expected_revision: u64) -> Result<u64, IdentityError> {
        let expected_persistence_revision = self.persistence.current_revision();
        let mut document = self.read_document()?;
        if document.pending_transition.is_none() {
            return Ok(document.revision);
        }
        if document.revision != expected_revision {
            return Err(IdentityError::RevisionConflict);
        }
        document.pending_transition = None;
        document.cas_epoch = expected_persistence_revision
            .checked_add(1)
            .ok_or(IdentityError::Overflow)?;
        let encoded = encode_identity_document(&document)?;
        self.persistence
            .replace_pending(expected_persistence_revision, &encoded)?;
        Ok(expected_persistence_revision)
    }

    fn read_document(&self) -> Result<IdentityDocument, IdentityError> {
        match self.persistence.read_bounded(MAX_IDENTITY_PHYSICAL_BYTES)? {
            None => Ok(IdentityDocument::default()),
            Some(bytes) => decode_identity_bytes(&bytes),
        }
    }

    fn read_document_stripped(&self) -> Result<IdentityDocument, IdentityError> {
        match self.persistence.read_bounded(MAX_IDENTITY_PHYSICAL_BYTES) {
            Ok(Some(bytes)) => match decode_identity_bytes(&bytes) {
                Ok(document) => Ok(document),
                // Recovery must still be able to write an explicit pending
                // marker when the legacy envelope itself is malformed.
                Err(_) => Ok(decode_legacy_pairing_only(&bytes).unwrap_or_default()),
            },
            Ok(None) => Ok(IdentityDocument::default()),
            Err(_) => Ok(IdentityDocument::default()),
        }
    }
}

fn rollback_pending_vault<V: CredentialVault>(
    vault: &mut V,
    pending: &PendingIdentityTransition,
) -> Result<(), IdentityError> {
    match pending.kind {
        PendingIdentityTransitionKind::Enable => {
            if let Some(host_id) = pending.host_public_id {
                vault
                    .discard_uncommitted_host(host_id)
                    .map_err(|_| IdentityError::TransitionRollbackFailed)?;
            }
        }
        PendingIdentityTransitionKind::RegisterDevice => {
            if let Some(device_id) = pending.device_id {
                vault
                    .discard_uncommitted_device(device_id)
                    .map_err(|_| IdentityError::TransitionRollbackFailed)?;
            }
        }
        PendingIdentityTransitionKind::RotateHostIdentity => {
            vault.abort_host_rotation()?;
        }
    }
    Ok(())
}

fn rollback_transition<V: CredentialVault>(
    vault: &mut V,
    transition: Option<VaultTransition>,
) -> Result<(), IdentityError> {
    match transition {
        None => Ok(()),
        Some(VaultTransition::HostEstablishment { host_id, proof }) => vault
            .rollback_host_establishment(host_id, &proof)
            .map_err(|_| IdentityError::TransitionRollbackFailed),
        Some(VaultTransition::DeviceEstablishment { device_id, proof }) => vault
            .rollback_device_establishment(device_id, &proof)
            .map_err(|_| IdentityError::TransitionRollbackFailed),
        Some(VaultTransition::HostRotation) => vault.abort_host_rotation(),
    }
}

#[derive(Clone)]
pub struct LoadedRemoteDocument {
    document: IdentityDocument,
}

impl fmt::Debug for LoadedRemoteDocument {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LoadedRemoteDocument")
            .field("revision", &self.document.revision)
            .field("has_identity", &self.document.identity.is_some())
            .field(
                "has_pending_transition",
                &self.document.pending_transition.is_some(),
            )
            .field(
                "requires_explicit_reestablish",
                &self.document.requires_explicit_reestablish,
            )
            .finish()
    }
}

impl LoadedRemoteDocument {
    pub fn revision(&self) -> u64 {
        self.document.revision
    }

    /// HOLD: returns the legacy remote pairing token if present. This is
    /// compatibility metadata, not Connect identity authority.
    #[allow(dead_code)]
    pub(crate) fn native_pairing_token(&self) -> Option<&str> {
        self.document
            .native_pairing_token
            .as_ref()
            .map(|code| code.as_str())
    }

    #[allow(dead_code)]
    pub(crate) fn web_pairing_token(&self) -> Option<&str> {
        self.document
            .web_pairing_token
            .as_ref()
            .map(|code| code.as_str())
    }

    pub fn identity(&self) -> Option<&ConnectIdentity> {
        self.document.identity.as_ref()
    }

    pub fn last_seen_host_build(&self) -> Option<u32> {
        self.document
            .identity
            .as_ref()
            .and_then(|identity| identity.last_seen_host_build)
            .or(self.document.connect_host_build)
    }

    pub fn requires_explicit_reestablish(&self) -> bool {
        self.document.requires_explicit_reestablish
    }

    pub fn has_pending_transition(&self) -> bool {
        self.document.pending_transition.is_some()
    }

    pub fn host_public_id_if_any(&self) -> Option<HostPublicId> {
        self.document
            .identity
            .as_ref()
            .map(ConnectIdentity::host_public_id)
    }
}

fn verify_bound_identity<V: CredentialVault>(
    identity: &ConnectIdentity,
    binding: &MachineBinding,
    vault: &V,
) -> Result<(), IdentityError> {
    identity.validate_structure()?;
    if identity.profile_binding_hash != binding.binding_hash() {
        return Err(IdentityError::CopiedProfile);
    }
    let proof = HostKeyProof::from_parts(
        identity.host_key.generation.unwrap_or(0),
        identity.host_key.fingerprint.clone(),
    );
    vault.verify_host(identity.host_public_id, &proof)?;
    for device in identity.devices() {
        if device.revoked {
            continue;
        }
        let proof = super::identity::DeviceKeyProof::from_parts(
            device.kind,
            device.public_key.fingerprint().to_string(),
        );
        vault.verify_device(device.device_id, &proof)?;
    }
    Ok(())
}

fn recoverable_identity_corruption(error: &IdentityError) -> bool {
    matches!(
        error,
        IdentityError::Corrupt
            | IdentityError::LimitExceeded { .. }
            | IdentityError::DuplicateField
            | IdentityError::UnknownField
            | IdentityError::DuplicateReceipt
            | IdentityError::DuplicateDevice
            | IdentityError::InvalidDevice
            | IdentityError::Overflow
    )
}

fn apply_command<V: CredentialVault>(
    document: &mut IdentityDocument,
    binding: &MachineBinding,
    vault: &mut V,
    command: &IdentityCommand,
    pending: Option<&PendingIdentityTransition>,
) -> Result<(IdentityReceipt, Option<VaultTransition>), IdentityError> {
    let next_revision = document
        .revision
        .checked_add(1)
        .ok_or(IdentityError::Overflow)?;
    let mut transition = None;
    let receipt = match &command.op {
        IdentityOp::NoteHostBuild { build } => {
            document.connect_host_build = Some(*build);
            if let Some(identity) = &mut document.identity {
                identity.last_seen_host_build = Some(*build);
            }
            empty_receipt(command.command_id, next_revision)
        }
        IdentityOp::Enable {
            host_build,
            now_epoch_ms,
        } => {
            if document.identity.is_some() {
                return Err(IdentityError::AlreadyEnabled);
            }
            let pairing_code = seed_pairing_code(
                document
                    .web_pairing_token
                    .as_ref()
                    .map(|code| code.as_str()),
                document
                    .native_pairing_token
                    .as_ref()
                    .map(|code| code.as_str()),
            )?;
            if document.web_pairing_token.is_none() {
                document.web_pairing_token = Some(pairing_code.clone());
            }
            let host_public_id = pending
                .and_then(|pending| pending.host_public_id)
                .unwrap_or_else(HostPublicId::new);
            let proof = vault.establish_host(host_public_id)?;
            let host_key = match KeyReference::from_host_proof(&proof) {
                Ok(host_key) => host_key,
                Err(error) => {
                    let rollback = vault.rollback_host_establishment(host_public_id, &proof);
                    return Err(rollback
                        .map(|_| error)
                        .unwrap_or(IdentityError::TransitionRollbackFailed));
                }
            };
            transition = Some(VaultTransition::HostEstablishment {
                host_id: host_public_id,
                proof: proof.clone(),
            });
            let identity = ConnectIdentity {
                schema_version: CONNECT_IDENTITY_SCHEMA_VERSION,
                host_public_id,
                host_key: host_key.clone(),
                pairing_code: pairing_code.clone(),
                pairing_code_generation: 1,
                pairing_purpose: PairingPurpose::OwnerDevice,
                profile_binding_hash: binding.binding_hash(),
                last_seen_host_build: Some(*host_build),
                created_at_epoch_ms: *now_epoch_ms,
                devices: Vec::new(),
            };
            document.connect_host_build = Some(*host_build);
            document.identity = Some(identity);
            document.requires_explicit_reestablish = false;
            enable_receipt(
                command.command_id,
                next_revision,
                IdentitySetup {
                    host_public_id,
                    host_key,
                    pairing_code,
                    pairing_purpose: PairingPurpose::OwnerDevice,
                    task_invite_id: None,
                },
            )
        }
        IdentityOp::RegisterDevice(request) => {
            let (device, proof) = register_device(
                document,
                vault,
                request,
                pending.and_then(|pending| pending.device_id),
            )?;
            transition = Some(VaultTransition::DeviceEstablishment {
                device_id: device.device_id,
                proof,
            });
            device_receipt(command.command_id, next_revision, device)
        }
        IdentityOp::RotatePairingCode { .. } => {
            let identity = document
                .identity
                .as_mut()
                .ok_or(IdentityError::NotEnabled)?;
            let next = rotate_pairing_until_changed(&identity.pairing_code)?;
            identity.pairing_code = next.clone();
            identity.pairing_code_generation = identity
                .pairing_code_generation
                .checked_add(1)
                .ok_or(IdentityError::Overflow)?;
            document.web_pairing_token = Some(next.clone());
            pairing_receipt(command.command_id, next_revision, next)
        }
        IdentityOp::RotateHostIdentity { .. } => {
            let host_id = document
                .identity
                .as_ref()
                .ok_or(IdentityError::NotEnabled)?
                .host_public_id;
            let proof = vault.prepare_host_rotation(host_id)?;
            let identity = document
                .identity
                .as_mut()
                .ok_or(IdentityError::NotEnabled)?;
            identity.host_key = KeyReference::from_host_proof(&proof)?;
            transition = Some(VaultTransition::HostRotation);
            let affected_device_count = identity.devices.len();
            for device in &mut identity.devices {
                device.requires_re_pair = true;
            }
            host_rotation_receipt(
                command.command_id,
                next_revision,
                HostIdentityRotation {
                    all_devices_require_repair: affected_device_count > 0,
                    affected_device_count,
                },
            )
        }
        IdentityOp::RevokeDevice {
            device_id,
            now_epoch_ms,
        } => {
            let identity = document
                .identity
                .as_mut()
                .ok_or(IdentityError::NotEnabled)?;
            let device = identity
                .devices
                .iter_mut()
                .find(|device| device.device_id == *device_id)
                .ok_or(IdentityError::UnknownDevice)?;
            device.revoked = true;
            device.revoked_at_epoch_ms = Some(*now_epoch_ms);
            empty_receipt(command.command_id, next_revision)
        }
        IdentityOp::RevokeAllDevices { now_epoch_ms } => {
            let identity = document
                .identity
                .as_mut()
                .ok_or(IdentityError::NotEnabled)?;
            for device in &mut identity.devices {
                device.revoked = true;
                device.revoked_at_epoch_ms = Some(*now_epoch_ms);
            }
            empty_receipt(command.command_id, next_revision)
        }
    };
    document.revision = next_revision;
    let mut receipt = receipt;
    receipt.command_digest = Some(command.payload_digest());
    push_receipt(document, receipt.clone())?;
    Ok((receipt, transition))
}

fn register_device<V: CredentialVault>(
    document: &mut IdentityDocument,
    vault: &mut V,
    request: &RegisterDevice,
    pending_device_id: Option<super::identity::DeviceId>,
) -> Result<(DeviceRecord, DeviceKeyProof), IdentityError> {
    let identity = document
        .identity
        .as_mut()
        .ok_or(IdentityError::NotEnabled)?;
    if identity.devices.len() >= MAX_IDENTITY_DEVICES as usize {
        return Err(IdentityError::LimitExceeded {
            field: IdentityLimitField::Devices,
        });
    }
    if request.label.chars().any(char::is_control) {
        return Err(IdentityError::InvalidDevice);
    }
    if request.label.len() > MAX_LABEL_BYTES {
        return Err(IdentityError::LimitExceeded {
            field: IdentityLimitField::Label,
        });
    }
    if let Some(legacy) = request.legacy_client_id.as_deref() {
        if legacy.is_empty()
            || legacy.chars().any(char::is_control)
            || legacy.len() > super::identity::MAX_ID_BYTES
        {
            return Err(IdentityError::LimitExceeded {
                field: super::identity::IdentityLimitField::Id,
            });
        }
    }
    match request.kind {
        DeviceKind::Browser => {
            let browser = request
                .browser
                .as_ref()
                .ok_or(IdentityError::InvalidDevice)?;
            if browser.private_identity_storage
                != BrowserPrivateStorage::WebCryptoNonExportableIndexedDb
                || !browser.cleared_storage_requires_visible_repair
                || browser.browser_install_id.is_empty()
            {
                return Err(IdentityError::InvalidDevice);
            }
            if browser.browser_install_id.chars().any(char::is_control) {
                return Err(IdentityError::InvalidDevice);
            }
            if browser.browser_install_id.len() > super::identity::MAX_ID_BYTES {
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
            if browser
                .nickname
                .as_deref()
                .is_some_and(|nickname| nickname.chars().any(char::is_control))
            {
                return Err(IdentityError::InvalidDevice);
            }
        }
        DeviceKind::Native if request.browser.is_some() => {
            return Err(IdentityError::InvalidDevice);
        }
        DeviceKind::Native => {}
    }
    if let Some(legacy) = request.legacy_client_id.as_deref() {
        if identity
            .devices
            .iter()
            .any(|device| device.legacy_client_id.as_deref() == Some(legacy))
        {
            return Err(IdentityError::DuplicateDevice);
        }
    }
    if let Some(install) = request
        .browser
        .as_ref()
        .map(|browser| browser.browser_install_id.as_str())
    {
        if identity.devices.iter().any(|device| {
            device
                .browser
                .as_ref()
                .is_some_and(|browser| browser.browser_install_id == install)
        }) {
            return Err(IdentityError::DuplicateDevice);
        }
    }
    let device_id = pending_device_id.unwrap_or_else(super::identity::DeviceId::new);
    let proof = vault.establish_device(device_id, request.kind)?;
    if proof.kind() != request.kind {
        let rollback = vault.rollback_device_establishment(device_id, &proof);
        return Err(rollback
            .map(|_| IdentityError::InvalidDevice)
            .unwrap_or(IdentityError::TransitionRollbackFailed));
    }
    if identity
        .devices
        .iter()
        .any(|existing| existing.device_id == device_id)
    {
        let rollback = vault.rollback_device_establishment(device_id, &proof);
        return Err(rollback
            .map(|_| IdentityError::DuplicateDevice)
            .unwrap_or(IdentityError::TransitionRollbackFailed));
    }
    let record = DeviceRecord {
        device_id,
        kind: request.kind,
        label: request.label.clone(),
        legacy_client_id: request.legacy_client_id.clone(),
        public_key: match KeyReference::from_device_proof(&proof) {
            Ok(public_key) => public_key,
            Err(error) => {
                let rollback = vault.rollback_device_establishment(device_id, &proof);
                return Err(rollback
                    .map(|_| error)
                    .unwrap_or(IdentityError::TransitionRollbackFailed));
            }
        },
        revoked: false,
        revoked_at_epoch_ms: None,
        requires_re_pair: false,
        browser: request.browser.clone(),
    };
    if let Err(error) = validate_device_record(&record) {
        let rollback = vault.rollback_device_establishment(device_id, &proof);
        return Err(rollback
            .map(|_| error)
            .unwrap_or(IdentityError::TransitionRollbackFailed));
    }
    if identity
        .devices
        .iter()
        .any(|existing| existing.public_key.fingerprint() == record.public_key.fingerprint())
    {
        let rollback = vault.rollback_device_establishment(device_id, &proof);
        if rollback.is_err() {
            return Err(IdentityError::TransitionRollbackFailed);
        }
        return Err(IdentityError::DuplicateDevice);
    }
    identity.devices.push(record.clone());
    Ok((record, proof))
}

fn push_receipt(
    document: &mut IdentityDocument,
    receipt: IdentityReceipt,
) -> Result<(), IdentityError> {
    if document.receipts.len() >= MAX_IDENTITY_RECEIPTS {
        document.receipts.remove(0);
    }
    document.receipts.push(receipt);
    Ok(())
}

fn decode_legacy_pairing_only(bytes: &[u8]) -> Result<IdentityDocument, IdentityError> {
    scan_bounded_json(bytes)?;
    let value: serde_json::Value =
        serde_json::from_slice(bytes).map_err(|_| IdentityError::Corrupt)?;
    let host = value
        .get("host")
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    let native = host
        .get("pairingToken")
        .and_then(serde_json::Value::as_str)
        .and_then(|token| super::identity::PairingCode::parse_valid(token).ok());
    let web = host
        .get("web")
        .and_then(|web| web.get("pairingToken"))
        .and_then(serde_json::Value::as_str)
        .and_then(|token| super::identity::PairingCode::parse_valid(token).ok());
    let server_id = host
        .get("serverId")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string);
    let server_id = server_id
        .filter(|value| !value.is_empty())
        .map(|value| {
            if value.chars().any(char::is_control) {
                Err(IdentityError::Corrupt)
            } else if value.len() > super::identity::MAX_ID_BYTES {
                Err(IdentityError::LimitExceeded {
                    field: IdentityLimitField::Id,
                })
            } else {
                Ok(value)
            }
        })
        .transpose()?;
    Ok(IdentityDocument {
        native_pairing_token: native,
        web_pairing_token: web,
        host_server_id: server_id,
        known_hosts: Vec::new(),
        pending_transition: None,
        requires_explicit_reestablish: true,
        ..IdentityDocument::default()
    })
}
