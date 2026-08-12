//! Isolated identity persistence facade.
//!
//! This slice exposes only an isolated in-memory persistence seam. It never
//! follows filesystem paths, opens `%APPDATA%`, or claims OS/WebCrypto
//! custody; a real production adapter remains an explicit future gate.

use std::fmt;

use super::identity::{
    bind_device_credential_from_snapshot, current_epoch_ms, generate_transition_nonce,
    rotate_pairing_until_changed,
    seed_pairing_code,
    validate_device_record, BrowserPrivateStorage, ConnectIdentity, CredentialVault,
    DeviceEstablishmentHandle, DeviceKeyProof, DeviceKind, DeviceRecord, DeviceRepairHandle,
    HostEstablishmentHandle, HostIdentityRotation, HostKeyProof, HostPublicId, HostRotationHandle,
    IdentityCommand, IdentityError, IdentityLimitField, IdentityOp, IdentityReceipt, IdentitySetup,
    KeyReference, MachineBinding, PairingPurpose, PendingIdentityTransition,
    PendingRevocationJournal,
    PendingIdentityTransitionKind, RegisterDevice, RepairDevice, CONNECT_IDENTITY_SCHEMA_VERSION,
    MAX_IDENTITY_DEVICES, MAX_IDENTITY_PHYSICAL_BYTES, MAX_IDENTITY_RECEIPTS, MAX_LABEL_BYTES,
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
    /// Atomically replace bytes iff both the physical CAS epoch and the exact
    /// bytes read by the caller are still current. This closes the ABA window
    /// where a stale executor observes a newer physical epoch and overwrites
    /// a different logical transition by CASing only that epoch.
    fn compare_and_swap_exact(
        &mut self,
        expected_revision: u64,
        expected_bytes: Option<&[u8]>,
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

    fn compare_and_swap_exact(
        &mut self,
        expected_revision: u64,
        expected_bytes: Option<&[u8]>,
        bytes: &[u8],
    ) -> Result<u64, IdentityError> {
        if self.revision != expected_revision || self.bytes.as_deref() != expected_bytes {
            return Err(IdentityError::RevisionConflict);
        }
        self.compare_and_swap(expected_revision, bytes)
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
    /// The owner token this in-process store successfully claimed. It lets a
    /// retry after a transient vault/persistence error reuse its own exact
    /// marker while another reader remains excluded.
    claimed_owner: Option<[u8; 16]>,
}

impl<P> fmt::Debug for IsolatedRemoteStore<P> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("IsolatedRemoteStore(redacted)")
    }
}

#[derive(Clone)]
enum VaultTransition {
    HostEstablishment {
        handle: HostEstablishmentHandle,
    },
    DeviceEstablishment {
        handle: DeviceEstablishmentHandle,
    },
    DeviceRepair {
        handle: DeviceRepairHandle,
    },
    HostRotation {
        handle: HostRotationHandle,
    },
    DeviceRevocations {
        entries: Vec<(super::identity::DeviceId, u64)>,
    },
}

impl<P: IdentityPersistence + 'static> IsolatedRemoteStore<P> {
    #[cfg(test)]
    pub fn new(persistence: P) -> Result<Self, IdentityError> {
        Ok(Self {
            persistence,
            claimed_owner: None,
        })
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

    #[cfg(test)]
    #[doc(hidden)]
    pub(crate) fn pending_transition_for_test(
        &self,
    ) -> Result<Option<PendingIdentityTransition>, IdentityError> {
        Ok(self.read_document()?.pending_transition)
    }

    #[cfg(test)]
    #[doc(hidden)]
    pub(crate) fn pending_revocation_for_test(
        &self,
    ) -> Result<Option<PendingRevocationJournal>, IdentityError> {
        Ok(self.read_document()?.pending_revocation)
    }

    #[cfg(test)]
    #[doc(hidden)]
    pub(crate) fn claim_pending_transition_for_test(
        &mut self,
        expected: &PendingIdentityTransition,
    ) -> Result<(), IdentityError> {
        self.claim_pending_transition(expected).map(|_| ())
    }

    #[cfg(test)]
    #[doc(hidden)]
    pub(crate) fn expire_pending_claim_for_test(&mut self) -> Result<(), IdentityError> {
        let expected_revision = self.persistence.current_revision();
        let (mut document, expected_bytes) = self.read_document_with_bytes()?;
        let pending = document
            .pending_transition
            .as_mut()
            .ok_or(IdentityError::TransitionPending)?;
        pending.claim_expires_at_epoch_ms = Some(1);
        let encoded = encode_identity_document(&document)?;
        self.persistence.compare_and_swap_exact(
            expected_revision,
            expected_bytes.as_deref(),
            &encoded,
        )?;
        Ok(())
    }

    pub fn load<V: CredentialVault>(
        &mut self,
        binding: &MachineBinding,
        vault: &V,
    ) -> Result<LoadedRemoteDocument, IdentityError> {
        let mut document = self.read_document()?;
        if document.pending_revocation.is_some() {
            // A durable journal is an unfinished authority transition. A
            // read-only load cannot restore or finalize vault leases, so it
            // must not expose the snapshot as settled identity.
            return Err(IdentityError::TransitionPending);
        }
        if let Some(pending) = document.pending_transition.clone() {
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
                let degraded = verify_bound_identity(identity, binding, vault)?;
                self.persist_browser_degradation(&mut document, &degraded)?;
            }
            return Ok(LoadedRemoteDocument { document });
        }
        if let Some(identity) = &document.identity {
            let degraded = verify_bound_identity(identity, binding, vault)?;
            self.persist_browser_degradation(&mut document, &degraded)?;
        }
        Ok(LoadedRemoteDocument { document })
    }

    /// Validate a credential against the document currently persisted by this
    /// store. Callers must not authorize from a previously loaded identity
    /// snapshot because revocation, host rotation, and repair can invalidate
    /// an otherwise well-formed proof.
    pub fn validate_device_credential<V: CredentialVault>(
        &mut self,
        binding: &MachineBinding,
        vault: &V,
        proof: &super::identity::DeviceCredentialProof,
        active_session_epoch: u64,
    ) -> Result<(), IdentityError> {
        let loaded = self.load(binding, vault)?;
        if loaded.has_pending_transition() {
            return Err(IdentityError::TransitionPending);
        }
        let identity = loaded.identity().ok_or(IdentityError::NotEnabled)?;
        super::identity::validate_device_credential(
            identity,
            binding,
            vault,
            proof,
            active_session_epoch,
        )
    }

    /// Mint a credential only from the identity currently persisted by this
    /// store. A caller cannot supply a snapshot, so a revoke, host rotation,
    /// or authenticated repair that won the persistence CAS is authoritative
    /// before a proof is created.
    pub fn bind_device_credential<V: CredentialVault>(
        &mut self,
        binding: &MachineBinding,
        vault: &V,
        device_id: super::identity::DeviceId,
        session_epoch: u64,
    ) -> Result<super::identity::DeviceCredentialProof, IdentityError> {
        let loaded = self.load(binding, vault)?;
        if loaded.has_pending_transition() {
            return Err(IdentityError::TransitionPending);
        }
        let identity = loaded.identity().ok_or(IdentityError::NotEnabled)?;
        bind_device_credential_from_snapshot(identity, binding, vault, device_id, session_epoch)
    }

    fn persist_browser_degradation(
        &mut self,
        document: &mut IdentityDocument,
        degraded_devices: &[super::identity::DeviceId],
    ) -> Result<(), IdentityError> {
        if degraded_devices.is_empty() {
            return Ok(());
        }
        let observed = self.persistence.current_revision();
        let expected_bytes = self
            .persistence
            .read_bounded(MAX_IDENTITY_PHYSICAL_BYTES)?;
        let mut updated = document.clone();
        let identity = updated.identity.as_mut().ok_or(IdentityError::Corrupt)?;
        for device in &mut identity.devices {
            if degraded_devices.contains(&device.device_id) {
                device.requires_re_pair = true;
            }
        }
        updated.revision = updated
            .revision
            .checked_add(1)
            .ok_or(IdentityError::Overflow)?;
        updated.cas_epoch = observed.checked_add(1).ok_or(IdentityError::Overflow)?;
        let encoded = encode_identity_document(&updated)?;
        self.persistence
            .compare_and_swap_exact(observed, expected_bytes.as_deref(), &encoded)?;
        *document = updated;
        Ok(())
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
                let expected_bytes = self
                    .persistence
                    .read_bounded(MAX_IDENTITY_PHYSICAL_BYTES)?;
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
                    .compare_and_swap_exact(
                        observed_persistence_revision,
                        expected_bytes.as_deref(),
                        &encoded,
                    )?;
                Ok(LoadedRemoteDocument { document })
            }
            Err(error) => Err(error),
        }
    }

    /// Explicitly abandon an interrupted vault transition. Register preserves
    /// the committed identity, Rotate/Repair restore their exact previous
    /// snapshot when the new vault slot is not committed, and Enable clears
    /// only its uncommitted host. A rollback failure leaves the marker.
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
        let claimed_pending = if pending.claim_owner.is_some()
            && self.claimed_owner != pending.claim_owner
            && pending_vault_handle_exists(vault, &pending)?
        {
            self.reclaim_pending_transition_after_vault_recovery(&pending)?
        } else {
            self.claim_pending_transition(&pending)?
        };
        let observed_after_claim = self.persistence.current_revision();
        let (mut document, expected_bytes) = self.read_document_with_bytes()?;
        if document.pending_transition.as_ref() != Some(&claimed_pending) {
            return Err(IdentityError::RevisionConflict);
        }
        let rotation_handle = if claimed_pending.kind
            == PendingIdentityTransitionKind::RotateHostIdentity
        {
            recover_host_rotation(vault, &claimed_pending)?
        } else {
            None
        };
        let repair_handle = if claimed_pending.kind == PendingIdentityTransitionKind::RepairDevice
        {
            recover_device_repair(
                vault,
                &claimed_pending,
                claimed_pending.device_id.ok_or(IdentityError::Corrupt)?,
            )?
        } else {
            None
        };
        let vault_already_matches_document = match claimed_pending.kind {
            PendingIdentityTransitionKind::RotateHostIdentity => document
                .identity
                .as_ref()
                .zip(claimed_pending.previous_identity.as_deref())
                .filter(|(identity, previous)| *identity != *previous)
                .map(|(identity, _)| {
                    rotation_handle.is_some() && verify_identity_host(identity, vault).is_ok()
                })
                .unwrap_or(false),
            PendingIdentityTransitionKind::RepairDevice => {
                match (
                    claimed_pending.device_id,
                    document.identity.as_ref(),
                    claimed_pending.previous_identity.as_deref(),
                    repair_handle.as_ref(),
                ) {
                    (Some(device_id), Some(identity), Some(previous), Some(handle))
                        if identity != previous && identity.device(device_id).is_some() =>
                    {
                        vault.device_repair_committed(handle)?
                    }
                    _ => false,
                }
            }
            PendingIdentityTransitionKind::Enable => {
                if let Some(identity) = document.identity.as_ref() {
                    match recover_host_establishment(vault, &claimed_pending)? {
                        Some(handle) => {
                            handle.host_public_id() == identity.host_public_id
                                && handle.proof().fingerprint() == identity.host_key.fingerprint()
                                && vault.host_establishment_committed(&handle)?
                        }
                        None => false,
                    }
                } else {
                    false
                }
            }
            PendingIdentityTransitionKind::RegisterDevice => {
                match (claimed_pending.device_id, document.identity.as_ref()) {
                    (Some(device_id), Some(identity)) => match identity.device(device_id) {
                        Some(device) => {
                            match recover_device_establishment(
                                vault,
                                &claimed_pending,
                                device_id,
                            )? {
                                Some(handle) => {
                                    handle.proof().fingerprint() == device.public_key.fingerprint()
                                        && vault.device_establishment_committed(&handle)?
                                }
                                None => false,
                            }
                        }
                        None => false,
                    },
                    _ => return Err(IdentityError::Corrupt),
                }
            }
        };
        if !vault_already_matches_document {
            rollback_pending_vault(vault, &claimed_pending)?;
        }
        match claimed_pending.kind {
            PendingIdentityTransitionKind::Enable => {
                if !vault_already_matches_document {
                    document.identity = None;
                    document.receipts.clear();
                    document.requires_explicit_reestablish = true;
                }
            }
            PendingIdentityTransitionKind::RotateHostIdentity => {
                if !vault_already_matches_document {
                    if let Some(previous) = claimed_pending.previous_identity.clone() {
                        verify_identity_host(&previous, vault)?;
                        document.identity = Some(*previous);
                    }
                    document
                        .receipts
                        .retain(|receipt| receipt.command_id() != claimed_pending.command_id);
                }
                document.requires_explicit_reestablish = false;
            }
            PendingIdentityTransitionKind::RegisterDevice => {
                if !vault_already_matches_document {
                    // The marker owns this exact generated DeviceId. If the
                    // establishment handle is absent or still prepared, it
                    // is safe to remove only that marker-owned record while
                    // preserving every previously committed device.
                    if let (Some(identity), Some(device_id)) =
                        (document.identity.as_mut(), claimed_pending.device_id)
                    {
                        identity.devices.retain(|device| device.device_id != device_id);
                    }
                    document
                        .receipts
                        .retain(|receipt| receipt.command_id() != claimed_pending.command_id);
                }
            }
            PendingIdentityTransitionKind::RepairDevice => {
                if !vault_already_matches_document {
                    if let Some(previous) = claimed_pending.previous_identity.clone() {
                        if let Some(device_id) = claimed_pending.device_id {
                            verify_identity_device(&previous, device_id, vault)?;
                        }
                        document.identity = Some(*previous);
                    }
                    document
                        .receipts
                        .retain(|receipt| receipt.command_id() != claimed_pending.command_id);
                }
            }
        }
        document.pending_transition = None;
        document.revision = document
            .revision
            .checked_add(1)
            .ok_or(IdentityError::Overflow)?;
        document.cas_epoch = observed_after_claim
            .checked_add(1)
            .ok_or(IdentityError::Overflow)?;
        let encoded = encode_identity_document(&document)?;
        self.persistence.compare_and_swap_exact(
            observed_after_claim,
            expected_bytes.as_deref(),
            &encoded,
        )?;
        self.claimed_owner = None;
        Ok(LoadedRemoteDocument { document })
    }

    pub fn execute<V: CredentialVault>(
        &mut self,
        binding: &MachineBinding,
        vault: &mut V,
        command: IdentityCommand,
    ) -> Result<IdentityReceipt, IdentityError> {
        let mut observed_persistence_revision = self.persistence.current_revision();
        let (mut document, mut expected_document_bytes) = self.read_document_with_bytes()?;
        if document.pending_revocation.is_some() {
            self.reconcile_pending_revocation(binding, vault)?;
            observed_persistence_revision = self.persistence.current_revision();
            (document, expected_document_bytes) = self.read_document_with_bytes()?;
        }
        let command_digest = command.payload_digest();
        let pending_retry = if let Some(pending) = document.pending_transition.clone() {
            if let Some(identity) = &document.identity {
                identity.validate_structure()?;
                if identity.profile_binding_hash != binding.binding_hash() {
                    return Err(IdentityError::CopiedProfile);
                }
            }
            if pending.command_id == command.command_id && pending.command_digest == command_digest
            {
                // Every retry claims the exact opaque marker before touching
                // vault state. This serializes retry/abandon and prevents a
                // stale transition from settling a newer marker in the same
                // command slot.
                let claimed_pending = self.claim_pending_transition(&pending)?;
                (document, expected_document_bytes) = self.read_document_with_bytes()?;
                if let Some(existing) = document
                    .receipts
                    .iter()
                    .find(|receipt| receipt.command_id() == command.command_id)
                    .cloned()
                {
                    if pending.kind == PendingIdentityTransitionKind::RotateHostIdentity {
                        let handle = recover_host_rotation(vault, &claimed_pending)?
                            .ok_or(IdentityError::TransitionPending)?;
                        vault.commit_host_rotation(&handle)?;
                    } else if pending.kind == PendingIdentityTransitionKind::Enable {
                        let handle = recover_host_establishment(vault, &claimed_pending)?
                            .ok_or(IdentityError::TransitionPending)?;
                        vault.commit_host_establishment(&handle)?;
                    } else if pending.kind == PendingIdentityTransitionKind::RegisterDevice {
                        if let Some(device_id) = pending.device_id {
                            let handle = recover_device_establishment(
                                vault,
                                &claimed_pending,
                                device_id,
                            )?
                            .ok_or(IdentityError::TransitionPending)?;
                            vault.commit_device_establishment(&handle)?;
                        }
                    } else if pending.kind == PendingIdentityTransitionKind::RepairDevice {
                        if let Some(device_id) = pending.device_id {
                            let handle = recover_device_repair(vault, &claimed_pending, device_id)?
                                .ok_or(IdentityError::TransitionPending)?;
                            vault.commit_device_repair(&handle)?;
                        }
                    }
                    if let Some(identity) = &document.identity {
                        verify_bound_identity(identity, binding, vault)?;
                    }
                    self.clear_pending_transition(&claimed_pending, document.revision)?;
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
            Some(claimed_pending)
        } else {
            None
        };
        if pending_retry.is_some() {
            if PendingIdentityTransitionKind::from_operation(&command.op)
                != pending_retry.as_ref().map(|pending| pending.kind)
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
        let (pending_marker, durable_revision) = if let Some(pending) = pending_retry.clone() {
            (Some(pending), command.expected_revision)
        } else if let Some(kind) = pending_kind {
            let transition_nonce = generate_transition_nonce()?;
            let mut claim_owner = generate_transition_nonce()?;
            while claim_owner == transition_nonce {
                claim_owner = generate_transition_nonce()?;
            }
            let claim_expires_at_epoch_ms = current_epoch_ms()?
                .checked_add(super::identity::PENDING_CLAIM_LEASE_MS)
                .ok_or(IdentityError::Overflow)?;
            let pending = PendingIdentityTransition {
                command_id: command.command_id,
                command_digest,
                kind,
                transition_nonce,
                claim_owner: Some(claim_owner),
                claim_expires_at_epoch_ms: Some(claim_expires_at_epoch_ms),
                claim_logical_revision: Some(document.revision),
                host_public_id: match kind {
                    PendingIdentityTransitionKind::Enable => Some(HostPublicId::new()),
                    PendingIdentityTransitionKind::RegisterDevice
                    | PendingIdentityTransitionKind::RepairDevice
                    | PendingIdentityTransitionKind::RotateHostIdentity => None,
                },
                device_id: match kind {
                    PendingIdentityTransitionKind::RegisterDevice => {
                        Some(super::identity::DeviceId::new())
                    }
                    PendingIdentityTransitionKind::RepairDevice => match &command.op {
                        IdentityOp::RepairDevice(request) => Some(request.device_id),
                        _ => return Err(IdentityError::Corrupt),
                    },
                    PendingIdentityTransitionKind::Enable
                    | PendingIdentityTransitionKind::RotateHostIdentity => None,
                },
                previous_identity: match kind {
                    PendingIdentityTransitionKind::RotateHostIdentity => {
                        document.identity.clone().map(Box::new)
                    }
                    PendingIdentityTransitionKind::Enable
                    | PendingIdentityTransitionKind::RegisterDevice => None,
                    PendingIdentityTransitionKind::RepairDevice => {
                        document.identity.clone().map(Box::new)
                    }
                },
            };
            let (pending_revision, pending_bytes) = self.persist_pending_transition(
                &document,
                pending.clone(),
                observed_persistence_revision,
            )?;
            document.revision = pending_revision;
            document.pending_transition = Some(pending.clone());
            self.claimed_owner = pending.claim_owner;
            expected_document_bytes = Some(pending_bytes);
            (Some(pending), pending_revision)
        } else {
            (None, command.expected_revision)
        };
        if let Some(journal) = revocation_journal_for_command(&document, &command)? {
            let (_, journal_bytes) = self.persist_pending_revocation(&mut document, journal)?;
            expected_document_bytes = Some(journal_bytes);
        }
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
                    if let Some(marker) = pending_marker.as_ref() {
                        if let Ok(Some(handle)) = recover_host_rotation(vault, marker) {
                            if let Err(cleanup) = vault.abort_host_rotation(&handle) {
                                return Err(cleanup);
                            }
                        }
                    }
                }
                if error != IdentityError::TransitionRollbackFailed
                    && pending_kind.is_some()
                    && !pending_was_preexisting
                {
                    if self
                        .clear_pending_transition(
                            pending_marker.as_ref().ok_or(IdentityError::Corrupt)?,
                            durable_revision,
                        )
                        .is_err()
                    {
                        return Err(IdentityError::TransitionRollbackFailed);
                    }
                }
                return Err(error);
            }
        };
        let keep_pending_until_vault_commit = matches!(
            &transition,
            Some(VaultTransition::HostEstablishment { .. })
                | Some(VaultTransition::DeviceEstablishment { .. })
                | Some(VaultTransition::HostRotation { .. })
                | Some(VaultTransition::DeviceRepair { .. })
        );
        if !keep_pending_until_vault_commit {
            // Marker-free operations can settle in this CAS. Vault-backed
            // establishment/repair/rotation markers remain until their
            // opaque custody handle commits below.
            document.pending_transition = None;
        }
        let expected_cas_revision = self.persistence.current_revision();
        let expected_cas_bytes = expected_document_bytes.as_deref();
        // A successful revoke clears the durable journal in the same exact
        // final CAS that publishes the identity flags/receipt.
        if matches!(
            &command.op,
            IdentityOp::RevokeDevice { .. } | IdentityOp::RevokeAllDevices { .. }
        ) {
            document.pending_revocation = None;
        }
        document.cas_epoch = expected_cas_revision
            .checked_add(1)
            .ok_or(IdentityError::Overflow)?;
        let encoded = match encode_identity_document(&document) {
            Ok(bytes) => bytes,
            Err(error) => {
                if rollback_transition(vault, transition).is_ok() {
                    if pending_kind.is_some()
                        && !pending_was_preexisting
                        && self
                            .clear_pending_transition(
                                pending_marker.as_ref().ok_or(IdentityError::Corrupt)?,
                                durable_revision,
                            )
                            .is_err()
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
            .compare_and_swap_exact(expected_cas_revision, expected_cas_bytes, &encoded)
        {
            Ok(_physical_revision) => {
                if let Some(VaultTransition::HostEstablishment { handle }) = &transition {
                    if let Err(error) = vault.commit_host_establishment(handle) {
                        return Err(error);
                    }
                } else if let Some(VaultTransition::DeviceEstablishment { handle }) = &transition {
                    if let Err(error) = vault.commit_device_establishment(handle) {
                        return Err(error);
                    }
                } else if let Some(VaultTransition::HostRotation { handle }) = &transition {
                    if let Err(error) = vault.commit_host_rotation(handle) {
                        // Identity CAS already landed. Keep pending + vault
                        // rotation so a matching retry can commit.
                        return Err(error);
                    }
                } else if let Some(VaultTransition::DeviceRepair { handle }) = &transition {
                    if let Err(error) = vault.commit_device_repair(handle) {
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
                    && self
                        .clear_pending_transition(
                            pending_marker.as_ref().ok_or(IdentityError::Corrupt)?,
                            logical_revision,
                        )
                        .is_err()
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
                        && self
                            .clear_pending_transition(
                                pending_marker.as_ref().ok_or(IdentityError::Corrupt)?,
                                durable_revision,
                            )
                            .is_err()
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

    fn claim_pending_transition(
        &mut self,
        expected: &PendingIdentityTransition,
    ) -> Result<PendingIdentityTransition, IdentityError> {
        let observed_persistence_revision = self.persistence.current_revision();
        let (mut document, expected_bytes) = self.read_document_with_bytes()?;
        let pending = document
            .pending_transition
            .clone()
            .ok_or(IdentityError::TransitionPending)?;
        if &pending != expected {
            return Err(IdentityError::RevisionConflict);
        }
        if let Some(owner) = pending.claim_owner {
            let now = current_epoch_ms()?;
            let lease_live = pending
                .claim_expires_at_epoch_ms
                .is_some_and(|expires| expires > now);
            return if lease_live
                && self.claimed_owner == Some(owner)
                && pending.claim_logical_revision == Some(document.revision)
            {
                Ok(pending)
            } else if lease_live {
                Err(IdentityError::RevisionConflict)
            } else {
                let mut reclaimed = pending;
                let mut owner = generate_transition_nonce()?;
                while owner == reclaimed.transition_nonce {
                    owner = generate_transition_nonce()?;
                }
                reclaimed.claim_owner = Some(owner);
                reclaimed.claim_expires_at_epoch_ms = Some(
                    now.checked_add(super::identity::PENDING_CLAIM_LEASE_MS)
                        .ok_or(IdentityError::Overflow)?,
                );
                reclaimed.claim_logical_revision = Some(document.revision);
                document.pending_transition = Some(reclaimed.clone());
                document.cas_epoch = observed_persistence_revision
                    .checked_add(1)
                    .ok_or(IdentityError::Overflow)?;
                let encoded = encode_identity_document(&document)?;
                self.persistence.compare_and_swap_exact(
                    observed_persistence_revision,
                    expected_bytes.as_deref(),
                    &encoded,
                )?;
                self.claimed_owner = reclaimed.claim_owner;
                Ok(reclaimed)
            };
        }
        let mut claimed = pending;
        let mut owner = generate_transition_nonce()?;
        while owner == claimed.transition_nonce {
            owner = generate_transition_nonce()?;
        }
        claimed.claim_owner = Some(owner);
        let now = current_epoch_ms()?;
        claimed.claim_expires_at_epoch_ms = Some(
            now.checked_add(super::identity::PENDING_CLAIM_LEASE_MS)
                .ok_or(IdentityError::Overflow)?,
        );
        claimed.claim_logical_revision = Some(document.revision);
        document.pending_transition = Some(claimed.clone());
        document.cas_epoch = observed_persistence_revision
            .checked_add(1)
            .ok_or(IdentityError::Overflow)?;
        let encoded = encode_identity_document(&document)?;
        self.persistence.compare_and_swap_exact(
            observed_persistence_revision,
            expected_bytes.as_deref(),
            &encoded,
        )?;
        self.claimed_owner = claimed.claim_owner;
        Ok(claimed)
    }

    fn reclaim_pending_transition_after_vault_recovery(
        &mut self,
        expected: &PendingIdentityTransition,
    ) -> Result<PendingIdentityTransition, IdentityError> {
        let expected_revision = self.persistence.current_revision();
        let (mut document, expected_bytes) = self.read_document_with_bytes()?;
        if document.pending_transition.as_ref() != Some(expected) {
            return Err(IdentityError::RevisionConflict);
        }
        let pending = document
            .pending_transition
            .as_mut()
            .ok_or(IdentityError::TransitionPending)?;
        pending.claim_expires_at_epoch_ms = Some(1);
        let expired = pending.clone();
        let encoded = encode_identity_document(&document)?;
        self.persistence.compare_and_swap_exact(
            expected_revision,
            expected_bytes.as_deref(),
            &encoded,
        )?;
        self.claim_pending_transition(&expired)
    }

    fn persist_pending_transition(
        &mut self,
        original: &IdentityDocument,
        pending: PendingIdentityTransition,
        expected_persistence_revision: u64,
    ) -> Result<(u64, Vec<u8>), IdentityError> {
        let (_, expected_bytes) = self.read_document_with_bytes()?;
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
        self.persistence.compare_and_swap_exact(
            expected_persistence_revision,
            expected_bytes.as_deref(),
            &encoded,
        )?;
        Ok((original.revision, encoded))
    }

    fn clear_pending_transition(
        &mut self,
        expected_pending: &PendingIdentityTransition,
        expected_revision: u64,
    ) -> Result<u64, IdentityError> {
        let expected_persistence_revision = self.persistence.current_revision();
        let (mut document, expected_bytes) = self.read_document_with_bytes()?;
        if document.pending_transition.is_none() {
            self.claimed_owner = None;
            return Ok(document.revision);
        }
        if document.pending_transition.as_ref() != Some(expected_pending) {
            return Err(IdentityError::RevisionConflict);
        }
        if document.revision != expected_revision {
            return Err(IdentityError::RevisionConflict);
        }
        if let Some(claim_revision) = expected_pending.claim_logical_revision {
            let settles_claimed_revision = claim_revision == document.revision
                || claim_revision
                    .checked_add(1)
                    .is_some_and(|revision| revision == document.revision);
            if !settles_claimed_revision {
                return Err(IdentityError::RevisionConflict);
            }
        }
        if expected_pending.claim_owner.is_some()
            && self.claimed_owner != expected_pending.claim_owner
        {
            return Err(IdentityError::RevisionConflict);
        }
        document.pending_transition = None;
        document.cas_epoch = expected_persistence_revision
            .checked_add(1)
            .ok_or(IdentityError::Overflow)?;
        let encoded = encode_identity_document(&document)?;
        self.persistence.compare_and_swap_exact(
            expected_persistence_revision,
            expected_bytes.as_deref(),
            &encoded,
        )?;
        self.claimed_owner = None;
        Ok(expected_persistence_revision)
    }

    fn persist_pending_revocation(
        &mut self,
        document: &mut IdentityDocument,
        journal: PendingRevocationJournal,
    ) -> Result<(u64, Vec<u8>), IdentityError> {
        let expected_persistence_revision = self.persistence.current_revision();
        let (_, expected_bytes) = self.read_document_with_bytes()?;
        let mut marked = document.clone();
        marked.pending_revocation = Some(journal);
        marked.cas_epoch = expected_persistence_revision
            .checked_add(1)
            .ok_or(IdentityError::Overflow)?;
        let encoded = encode_identity_document(&marked)?;
        let next = self.persistence.compare_and_swap_exact(
            expected_persistence_revision,
            expected_bytes.as_deref(),
            &encoded,
        )?;
        *document = marked;
        Ok((next, encoded))
    }

    fn reconcile_pending_revocation<V: CredentialVault>(
        &mut self,
        binding: &MachineBinding,
        vault: &mut V,
    ) -> Result<(), IdentityError> {
        let expected_persistence_revision = self.persistence.current_revision();
        let (mut document, expected_bytes) = self.read_document_with_bytes()?;
        let journal = document
            .pending_revocation
            .clone()
            .ok_or(IdentityError::Corrupt)?;
        let identity = document.identity.as_ref().ok_or(IdentityError::Corrupt)?;
        identity.validate_structure()?;
        if identity.profile_binding_hash != binding.binding_hash() {
            return Err(IdentityError::CopiedProfile);
        }
        let mut invalidated = Vec::new();
        let mut uncertain = Vec::new();
        for (device_id, epoch) in &journal.entries {
            let device = identity
                .device(*device_id)
                .ok_or(IdentityError::UnknownDevice)?;
            let proof = DeviceKeyProof::from_parts(
                device.device_id,
                device.kind,
                device.public_key.fingerprint().to_string(),
            );
            match vault.verify_device(*device_id, &proof) {
                Ok(()) => {}
                Err(IdentityError::UnknownDevice) => invalidated.push((*device_id, *epoch)),
                Err(_) => uncertain.push((*device_id, *epoch)),
            }
        }
        if invalidated.is_empty() && uncertain.is_empty() {
            // The journal may remain after a final-CAS failure whose vault
            // rollback already completed. Do not restore an active slot a
            // second time; clear only the durable intent.
            document.pending_revocation = None;
        } else if invalidated.len() == journal.entries.len() {
            let next_revision = document
                .revision
                .checked_add(1)
                .ok_or(IdentityError::Overflow)?;
            let identity = document.identity.as_mut().ok_or(IdentityError::Corrupt)?;
            for (device_id, epoch) in &journal.entries {
                let device = identity
                    .devices
                    .iter_mut()
                    .find(|device| device.device_id == *device_id)
                    .ok_or(IdentityError::UnknownDevice)?;
                device.revoked = true;
                device.revoked_at_epoch_ms = Some(*epoch);
            }
            let mut receipt = empty_receipt(journal.command_id, next_revision);
            receipt.command_digest = Some(journal.command_digest);
            push_receipt(&mut document, receipt)?;
            document.revision = next_revision;
            document.pending_revocation = None;
        } else {
            let mut restore_error = None;
            for (device_id, epoch) in invalidated.into_iter().chain(uncertain).rev() {
                if let Err(error) = vault.restore_device_credential(device_id, epoch) {
                    restore_error = Some(error);
                    break;
                }
            }
            if restore_error.is_some() {
                return Err(IdentityError::TransitionRollbackFailed);
            }
            document.pending_revocation = None;
        }
        document.cas_epoch = expected_persistence_revision
            .checked_add(1)
            .ok_or(IdentityError::Overflow)?;
        let encoded = encode_identity_document(&document)?;
        self.persistence.compare_and_swap_exact(
            expected_persistence_revision,
            expected_bytes.as_deref(),
            &encoded,
        )?;
        Ok(())
    }

    fn read_document(&self) -> Result<IdentityDocument, IdentityError> {
        match self.persistence.read_bounded(MAX_IDENTITY_PHYSICAL_BYTES)? {
            None => Ok(IdentityDocument::default()),
            Some(bytes) => decode_identity_bytes(&bytes),
        }
    }

    fn read_document_with_bytes(
        &self,
    ) -> Result<(IdentityDocument, Option<Vec<u8>>), IdentityError> {
        match self.persistence.read_bounded(MAX_IDENTITY_PHYSICAL_BYTES)? {
            None => Ok((IdentityDocument::default(), None)),
            Some(bytes) => Ok((decode_identity_bytes(&bytes)?, Some(bytes))),
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

fn recover_host_rotation<V: CredentialVault>(
    vault: &mut V,
    pending: &PendingIdentityTransition,
) -> Result<Option<HostRotationHandle>, IdentityError> {
    let host_id = pending
        .previous_identity
        .as_deref()
        .map(ConnectIdentity::host_public_id)
        .or(pending.host_public_id)
        .ok_or(IdentityError::Corrupt)?;
    vault.recover_host_rotation(host_id, pending.transition_nonce)
}

fn recover_host_establishment<V: CredentialVault>(
    vault: &mut V,
    pending: &PendingIdentityTransition,
) -> Result<Option<HostEstablishmentHandle>, IdentityError> {
    let host_id = pending.host_public_id.ok_or(IdentityError::Corrupt)?;
    vault.recover_host_establishment(host_id, pending.transition_nonce)
}

fn recover_device_repair<V: CredentialVault>(
    vault: &mut V,
    pending: &PendingIdentityTransition,
    device_id: super::identity::DeviceId,
) -> Result<Option<DeviceRepairHandle>, IdentityError> {
    vault.recover_device_repair(device_id, pending.transition_nonce)
}

fn recover_device_establishment<V: CredentialVault>(
    vault: &mut V,
    pending: &PendingIdentityTransition,
    device_id: super::identity::DeviceId,
) -> Result<Option<DeviceEstablishmentHandle>, IdentityError> {
    vault.recover_device_establishment(device_id, pending.transition_nonce)
}

fn pending_vault_handle_exists<V: CredentialVault>(
    vault: &mut V,
    pending: &PendingIdentityTransition,
) -> Result<bool, IdentityError> {
    match pending.kind {
        PendingIdentityTransitionKind::Enable => pending
            .host_public_id
            .ok_or(IdentityError::Corrupt)
            .and_then(|host_id| {
                vault
                    .recover_host_establishment(host_id, pending.transition_nonce)
                    .map(|handle| handle.is_some())
            }),
        PendingIdentityTransitionKind::RegisterDevice => pending
            .device_id
            .ok_or(IdentityError::Corrupt)
            .and_then(|device_id| {
                vault
                    .recover_device_establishment(device_id, pending.transition_nonce)
                    .map(|handle| handle.is_some())
            }),
        PendingIdentityTransitionKind::RepairDevice => pending
            .device_id
            .ok_or(IdentityError::Corrupt)
            .and_then(|device_id| {
                vault
                    .recover_device_repair(device_id, pending.transition_nonce)
                    .map(|handle| handle.is_some())
            }),
        PendingIdentityTransitionKind::RotateHostIdentity => {
            recover_host_rotation(vault, pending).map(|handle| handle.is_some())
        }
    }
}

fn rollback_pending_vault<V: CredentialVault>(
    vault: &mut V,
    pending: &PendingIdentityTransition,
) -> Result<(), IdentityError> {
    match pending.kind {
        PendingIdentityTransitionKind::Enable => {
            if pending.host_public_id.is_some() {
                if let Some(handle) = recover_host_establishment(vault, pending)? {
                    vault
                        .rollback_host_establishment(&handle)
                        .map_err(|_| IdentityError::TransitionRollbackFailed)?;
                }
            }
        }
        PendingIdentityTransitionKind::RegisterDevice => {
            if let Some(device_id) = pending.device_id {
                if let Some(handle) = recover_device_establishment(vault, pending, device_id)? {
                    vault
                        .rollback_device_establishment(&handle)
                        .map_err(|_| IdentityError::TransitionRollbackFailed)?;
                }
            }
        }
        PendingIdentityTransitionKind::RepairDevice => {
            let device_id = pending.device_id.ok_or(IdentityError::Corrupt)?;
            if let Some(handle) = recover_device_repair(vault, pending, device_id)? {
                vault.abort_device_repair(&handle)?;
            }
        }
        PendingIdentityTransitionKind::RotateHostIdentity => {
            if let Some(handle) = recover_host_rotation(vault, pending)? {
                vault.abort_host_rotation(&handle)?;
            }
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
        Some(VaultTransition::HostEstablishment { handle }) => vault
            .rollback_host_establishment(&handle)
            .map_err(|_| IdentityError::TransitionRollbackFailed),
        Some(VaultTransition::DeviceEstablishment { handle }) => vault
            .rollback_device_establishment(&handle)
            .map_err(|_| IdentityError::TransitionRollbackFailed),
        Some(VaultTransition::DeviceRepair { handle }) => vault
            .rollback_device_repair(&handle)
            .map_err(|_| IdentityError::TransitionRollbackFailed),
        Some(VaultTransition::HostRotation { handle }) => vault
            .abort_host_rotation(&handle)
            .map_err(|_| IdentityError::TransitionRollbackFailed),
        Some(VaultTransition::DeviceRevocations { entries }) => {
            for (device_id, epoch) in entries.into_iter().rev() {
                vault
                    .restore_device_credential(device_id, epoch)
                    .map_err(|_| IdentityError::TransitionRollbackFailed)?;
            }
            Ok(())
        }
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
                "has_pending_revocation",
                &self.document.pending_revocation.is_some(),
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

fn verify_identity_host<V: CredentialVault>(
    identity: &ConnectIdentity,
    vault: &V,
) -> Result<(), IdentityError> {
    let generation = identity.host_key.generation.ok_or(IdentityError::Corrupt)?;
    if generation == 0 {
        return Err(IdentityError::Corrupt);
    }
    let proof = HostKeyProof::from_parts(
        identity.host_public_id,
        generation,
        identity.host_key.fingerprint.clone(),
    );
    vault.verify_host(identity.host_public_id, &proof)
}

fn verify_identity_device<V: CredentialVault>(
    identity: &ConnectIdentity,
    device_id: super::identity::DeviceId,
    vault: &V,
) -> Result<(), IdentityError> {
    let device = identity
        .device(device_id)
        .ok_or(IdentityError::UnknownDevice)?;
    let proof = DeviceKeyProof::from_parts(
        device.device_id,
        device.kind,
        device.public_key.fingerprint().to_string(),
    );
    vault.verify_device(device.device_id, &proof)
}

fn verify_bound_identity<V: CredentialVault>(
    identity: &ConnectIdentity,
    binding: &MachineBinding,
    vault: &V,
) -> Result<Vec<super::identity::DeviceId>, IdentityError> {
    identity.validate_structure()?;
    if identity.profile_binding_hash != binding.binding_hash() {
        return Err(IdentityError::CopiedProfile);
    }
    let proof = HostKeyProof::from_parts(
        identity.host_public_id,
        identity.host_key.generation.unwrap_or(0),
        identity.host_key.fingerprint.clone(),
    );
    vault.verify_host(identity.host_public_id, &proof)?;
    let mut degraded = Vec::new();
    for device in identity.devices() {
        if device.revoked || device.requires_re_pair {
            continue;
        }
        let proof = super::identity::DeviceKeyProof::from_parts(
            device.device_id,
            device.kind,
            device.public_key.fingerprint().to_string(),
        );
        if let Err(error) = vault.verify_device(device.device_id, &proof) {
            if device.kind == DeviceKind::Browser
                && matches!(
                    error,
                    IdentityError::MissingCredentialProof
                        | IdentityError::WrongCredentialGeneration
                )
            {
                degraded.push(device.device_id);
            } else {
                return Err(error);
            }
        }
    }
    Ok(degraded)
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

fn revocation_journal_for_command(
    document: &IdentityDocument,
    command: &IdentityCommand,
) -> Result<Option<PendingRevocationJournal>, IdentityError> {
    let (revoke_all, now_epoch_ms, requested_device) = match &command.op {
        IdentityOp::RevokeDevice {
            device_id,
            now_epoch_ms,
        } => (false, *now_epoch_ms, Some(*device_id)),
        IdentityOp::RevokeAllDevices { now_epoch_ms } => (true, *now_epoch_ms, None),
        _ => return Ok(None),
    };
    let identity = document.identity.as_ref().ok_or(IdentityError::NotEnabled)?;
    let entries = if let Some(device_id) = requested_device {
        if identity.device(device_id).is_none() {
            return Err(IdentityError::UnknownDevice);
        }
        vec![(device_id, now_epoch_ms)]
    } else {
        identity
            .devices
            .iter()
            .map(|device| (device.device_id, now_epoch_ms))
            .collect::<Vec<_>>()
    };
    if entries.is_empty() {
        return Ok(None);
    }
    Ok(Some(PendingRevocationJournal {
        command_id: command.command_id,
        command_digest: command.payload_digest(),
        revoke_all,
        entries,
    }))
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
            let transition_nonce = pending
                .map(|pending| pending.transition_nonce)
                .ok_or(IdentityError::Corrupt)?;
            let handle = vault.establish_host(host_public_id, transition_nonce)?;
            let proof = handle.proof();
            if proof.host_public_id() != host_public_id
                || handle.host_public_id() != host_public_id
                || handle.transition_nonce() != transition_nonce
            {
                let rollback = vault.rollback_host_establishment(&handle);
                return Err(rollback
                    .map(|_| IdentityError::MissingCredentialProof)
                    .unwrap_or(IdentityError::TransitionRollbackFailed));
            }
            let host_key = match KeyReference::from_host_proof(&proof) {
                Ok(host_key) => host_key,
                Err(error) => {
                    let rollback = vault.rollback_host_establishment(&handle);
                    return Err(rollback
                        .map(|_| error)
                        .unwrap_or(IdentityError::TransitionRollbackFailed));
                }
            };
            transition = Some(VaultTransition::HostEstablishment {
                handle: handle.clone(),
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
            let (device, handle) = register_device(
                document,
                vault,
                request,
                pending.and_then(|pending| pending.device_id),
                pending.map(|pending| pending.transition_nonce),
            )?;
            transition = Some(VaultTransition::DeviceEstablishment {
                handle,
            });
            device_receipt(command.command_id, next_revision, device)
        }
        IdentityOp::RepairDevice(request) => {
            let (device, handle) = repair_device(
                document,
                vault,
                request,
                pending.ok_or(IdentityError::Corrupt)?.transition_nonce,
            )?;
            transition = Some(VaultTransition::DeviceRepair { handle });
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
            let transition_nonce = pending
                .ok_or(IdentityError::Corrupt)?
                .transition_nonce;
            let handle = vault.prepare_host_rotation(host_id, transition_nonce)?;
            if handle.host_public_id() != host_id
                || handle.transition_nonce() != transition_nonce
            {
                let rollback = vault.abort_host_rotation(&handle);
                return Err(rollback
                    .map(|_| IdentityError::MissingCredentialProof)
                    .unwrap_or(IdentityError::TransitionRollbackFailed));
            }
            let proof = handle.proof().clone();
            let identity = document
                .identity
                .as_mut()
                .ok_or(IdentityError::NotEnabled)?;
            identity.host_key = match KeyReference::from_host_proof(&proof) {
                Ok(key) => key,
                Err(error) => {
                    let rollback = vault.abort_host_rotation(&handle);
                    return Err(rollback
                        .map(|_| error)
                        .unwrap_or(IdentityError::TransitionRollbackFailed));
                }
            };
            transition = Some(VaultTransition::HostRotation { handle });
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
            vault.invalidate_device_credential(*device_id, *now_epoch_ms)?;
            transition = Some(VaultTransition::DeviceRevocations {
                entries: vec![(*device_id, *now_epoch_ms)],
            });
            device.revoked = true;
            device.revoked_at_epoch_ms = Some(*now_epoch_ms);
            empty_receipt(command.command_id, next_revision)
        }
        IdentityOp::RevokeAllDevices { now_epoch_ms } => {
            let device_ids = document
                .identity
                .as_ref()
                .ok_or(IdentityError::NotEnabled)?
                .devices
                .iter()
                .map(|device| device.device_id)
                .collect::<Vec<_>>();
            let mut entries = Vec::with_capacity(device_ids.len());
            for device_id in device_ids {
                if let Err(error) = vault.invalidate_device_credential(device_id, *now_epoch_ms) {
                    let mut restore_error = None;
                    for (restored_id, restored_epoch) in entries.into_iter().rev() {
                        if let Err(error) =
                            vault.restore_device_credential(restored_id, restored_epoch)
                        {
                            restore_error = Some(error);
                        }
                    }
                    if restore_error.is_some() {
                        return Err(IdentityError::TransitionRollbackFailed);
                    }
                    return Err(error);
                }
                entries.push((device_id, *now_epoch_ms));
            }
            transition = Some(VaultTransition::DeviceRevocations { entries });
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
    transition_nonce: Option<[u8; 16]>,
) -> Result<(DeviceRecord, DeviceEstablishmentHandle), IdentityError> {
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
    let transition_nonce = transition_nonce.ok_or(IdentityError::Corrupt)?;
    let handle = vault.establish_device(device_id, request.kind, transition_nonce)?;
    let proof = handle.proof();
    if proof.device_id() != device_id
        || proof.kind() != request.kind
        || handle.device_id() != device_id
        || handle.transition_nonce() != transition_nonce
    {
        let rollback = vault.rollback_device_establishment(&handle);
        return Err(rollback
            .map(|_| IdentityError::InvalidDevice)
            .unwrap_or(IdentityError::TransitionRollbackFailed));
    }
    if identity
        .devices
        .iter()
        .any(|existing| existing.device_id == device_id)
    {
        let rollback = vault.rollback_device_establishment(&handle);
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
                let rollback = vault.rollback_device_establishment(&handle);
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
        let rollback = vault.rollback_device_establishment(&handle);
        return Err(rollback
            .map(|_| error)
            .unwrap_or(IdentityError::TransitionRollbackFailed));
    }
    if identity
        .devices
        .iter()
        .any(|existing| existing.public_key.fingerprint() == record.public_key.fingerprint())
    {
        let rollback = vault.rollback_device_establishment(&handle);
        if rollback.is_err() {
            return Err(IdentityError::TransitionRollbackFailed);
        }
        return Err(IdentityError::DuplicateDevice);
    }
    identity.devices.push(record.clone());
    Ok((record, handle))
}

fn repair_device<V: CredentialVault>(
    document: &mut IdentityDocument,
    vault: &mut V,
    request: &RepairDevice,
    transition_nonce: [u8; 16],
) -> Result<(DeviceRecord, DeviceRepairHandle), IdentityError> {
    let existing = document
        .identity
        .as_ref()
        .ok_or(IdentityError::NotEnabled)?
        .device(request.device_id)
        .cloned()
        .ok_or(IdentityError::UnknownDevice)?;
    if existing.revoked || !existing.requires_re_pair || existing.kind != request.kind {
        return Err(IdentityError::InvalidDevice);
    }
    validate_repair_metadata(request)?;
    let handle = vault.prepare_device_repair(
        request.device_id,
        request.kind,
        transition_nonce,
    )?;
    let proof = handle.proof().clone();
    if proof.device_id() != request.device_id || proof.kind() != request.kind {
        let rollback = vault.rollback_device_repair(&handle);
        return Err(rollback
            .map(|_| IdentityError::InvalidDevice)
            .unwrap_or(IdentityError::TransitionRollbackFailed));
    }
    let public_key = match KeyReference::from_device_proof(&proof) {
        Ok(public_key) => public_key,
        Err(error) => {
            let rollback = vault.rollback_device_repair(&handle);
            return Err(rollback
                .map(|_| error)
                .unwrap_or(IdentityError::TransitionRollbackFailed));
        }
    };
    if let Err(error) = vault.verify_device(request.device_id, &proof) {
        let rollback = vault.rollback_device_repair(&handle);
        return Err(rollback
            .map(|_| error)
            .unwrap_or(IdentityError::TransitionRollbackFailed));
    }
    let record = DeviceRecord {
        device_id: existing.device_id,
        kind: request.kind,
        label: request.label.clone(),
        legacy_client_id: request.legacy_client_id.clone(),
        public_key,
        revoked: false,
        revoked_at_epoch_ms: None,
        requires_re_pair: false,
        browser: request.browser.clone(),
    };
    if let Err(error) = validate_device_record(&record) {
        let rollback = vault.rollback_device_repair(&handle);
        return Err(rollback
            .map(|_| error)
            .unwrap_or(IdentityError::TransitionRollbackFailed));
    }
    let identity = document
        .identity
        .as_ref()
        .ok_or(IdentityError::NotEnabled)?;
    if record.legacy_client_id.as_deref().is_some_and(|legacy| {
        identity.devices.iter().any(|existing| {
            existing.device_id != record.device_id
                && existing.legacy_client_id.as_deref() == Some(legacy)
        })
    }) || record.browser.as_ref().is_some_and(|browser| {
        identity.devices.iter().any(|existing| {
            existing.device_id != record.device_id
                && existing
                    .browser
                    .as_ref()
                    .is_some_and(|current| current.browser_install_id == browser.browser_install_id)
        })
    }) || identity.devices.iter().any(|existing| {
        existing.device_id != record.device_id
            && existing.public_key.fingerprint() == record.public_key.fingerprint()
    }) {
        let rollback = vault.rollback_device_repair(&handle);
        return Err(rollback
            .map(|_| IdentityError::DuplicateDevice)
            .unwrap_or(IdentityError::TransitionRollbackFailed));
    }
    let identity = document
        .identity
        .as_mut()
        .ok_or(IdentityError::NotEnabled)?;
    let slot = identity
        .devices
        .iter_mut()
        .find(|device| device.device_id == request.device_id)
        .ok_or(IdentityError::UnknownDevice)?;
    *slot = record.clone();
    Ok((record, handle))
}

fn validate_repair_metadata(request: &RepairDevice) -> Result<(), IdentityError> {
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
                field: IdentityLimitField::Id,
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
    Ok(())
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
