//! Canonical Device enrollment bridge from completed Noise authentication to
//! IdentityStore / host vault and live remote authorization.
//!
//! Cookie pins remain bootstrap trust. This module registers genuine Device-kind
//! peers once (or rebinds the exact cookie + public-key fingerprint) and mints a
//! session-epoch-bound [`DeviceCredentialProof`]. Host-kind peers are never
//! registered as devices; the paired-cookie route keeps explicit legacy Host
//! compatibility while the browser migrates to Device claims.
//!
//! `OsConnectHostVault::authorize_device_enrollment` is one-use. Exact
//! `RegisterDevice` retries reuse the retained command and let `store.execute`
//! recover the vault slot without re-authorization.
//!
//! Browser's persisted custody ID is UUIDv4 and must not be cast to canonical
//! [`DeviceId`] (UUIDv7). Host-generated DeviceId is authoritative; clients are
//! identified by bounded cookie mapping and registered fingerprint.

use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard};

use sha2::{Digest, Sha256};
use tokio::sync::watch;

use super::host_vault::{
    derive_machine_binding, resolve_host_profile_for_binding, OsConnectHostVault,
};
use super::identity::{
    hex_encode, BrowserDeviceDto, BrowserPrivateStorage, CredentialVault, DeviceCredentialProof,
    DeviceId, DeviceKind, IdentityCommand, IdentityError, IdentityOp, IdentityReceipt,
    MachineBinding, RegisterDevice, MAX_ID_BYTES, MAX_LABEL_BYTES,
};
use super::identity_store::{
    ConnectIdentityLiveState, IdentityPersistence, IsolatedRemoteStore, KernelIdentityPersistence,
    LoadedRemoteDocument,
};
use crate::domain::id::CommandId;
use crate::protocol::{AuthenticatedPeer, NoiseStaticPublicKey};

/// Browser metadata taken from the already-paired WebConfig client, never from
/// untrusted Noise claim fields.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrowserEnrollmentMetadata {
    pub paired_cookie_client_id: String,
    pub label: String,
    pub browser_install_id: String,
    pub nickname: Option<String>,
}

/// Result of bridging an authenticated Noise peer onto canonical identity.
#[derive(Clone)]
pub enum DeviceEnrollmentAuthority {
    /// Canonical Device registration/rebind with an opaque session-bound proof.
    Device {
        proof: DeviceCredentialProof,
        device_id: DeviceId,
        session_epoch: u64,
        /// Captured via [`IsolatedRemoteStore::authority_generation`] at proof
        /// mint time, not when a duplex later attaches a lease.
        authority_generation: u64,
        authority_rx: watch::Receiver<u64>,
    },
    /// Host-kind Noise claim on the paired-cookie route. Cookie pin remains the
    /// authorization fence until the browser migrates to Device claims.
    LegacyHostCompat {
        authority_generation: u64,
        authority_rx: watch::Receiver<u64>,
    },
}

impl fmt::Debug for DeviceEnrollmentAuthority {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Device {
                device_id,
                session_epoch,
                authority_generation,
                ..
            } => formatter
                .debug_struct("DeviceEnrollmentAuthority::Device")
                .field("device_id", device_id)
                .field("session_epoch", session_epoch)
                .field("authority_generation", authority_generation)
                .finish(),
            Self::LegacyHostCompat {
                authority_generation,
                ..
            } => formatter
                .debug_struct("DeviceEnrollmentAuthority::LegacyHostCompat")
                .field("authority_generation", authority_generation)
                .finish(),
        }
    }
}

impl DeviceEnrollmentAuthority {
    pub fn subscribe_authority(&self) -> watch::Receiver<u64> {
        match self {
            Self::Device { authority_rx, .. } | Self::LegacyHostCompat { authority_rx, .. } => {
                authority_rx.clone()
            }
        }
    }

    pub fn authority_generation(&self) -> u64 {
        match self {
            Self::Device {
                authority_generation,
                ..
            }
            | Self::LegacyHostCompat {
                authority_generation,
                ..
            } => *authority_generation,
        }
    }

    pub fn device_credential(&self) -> Option<(&DeviceCredentialProof, u64, u64)> {
        match self {
            Self::Device {
                proof,
                session_epoch,
                authority_generation,
                ..
            } => Some((proof, *session_epoch, *authority_generation)),
            Self::LegacyHostCompat { .. } => None,
        }
    }

    pub fn is_legacy_host_compat(&self) -> bool {
        matches!(self, Self::LegacyHostCompat { .. })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeviceEnrollmentError {
    Identity(IdentityError),
    HostClaimNotDevice,
    KeyMismatchRequiresRepair,
    IdentityNotLive,
    InvalidMetadata,
    UnsupportedNativeWire,
    Cancelled,
}

impl fmt::Display for DeviceEnrollmentError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Identity(error) => write!(formatter, "{error}"),
            Self::HostClaimNotDevice => {
                formatter.write_str("host-kind Noise peer cannot enroll as a device")
            }
            Self::KeyMismatchRequiresRepair => formatter.write_str(
                "paired cookie maps to a different device public key; revoke and re-pair",
            ),
            Self::IdentityNotLive => formatter.write_str("Connect identity is not live"),
            Self::InvalidMetadata => {
                formatter.write_str("paired browser metadata is missing or invalid")
            }
            Self::UnsupportedNativeWire => formatter.write_str(
                "native-kind device enrollment requires wire metadata not present on the HTTP paired browser route",
            ),
            Self::Cancelled => formatter.write_str("device enrollment was cancelled"),
        }
    }
}

impl std::error::Error for DeviceEnrollmentError {}

impl From<IdentityError> for DeviceEnrollmentError {
    fn from(error: IdentityError) -> Self {
        Self::Identity(error)
    }
}

/// Vault seam required to consume a genuine Noise Device peer into establish_device.
pub trait DeviceEnrollmentVault: CredentialVault {
    fn authorize_device_enrollment(
        &mut self,
        peer: AuthenticatedPeer,
        kind: DeviceKind,
    ) -> Result<(), IdentityError>;
}

impl DeviceEnrollmentVault for OsConnectHostVault {
    fn authorize_device_enrollment(
        &mut self,
        peer: AuthenticatedPeer,
        kind: DeviceKind,
    ) -> Result<(), IdentityError> {
        OsConnectHostVault::authorize_device_enrollment(self, peer, kind)
    }
}

/// Exact pending RegisterDevice command retained for retry (same digest).
struct RetainedRegister {
    command: IdentityCommand,
    peer_fingerprint: String,
    metadata: BrowserEnrollmentMetadata,
}

/// Single claim-owning enrollment context for one production Connect startup.
///
/// Mutex is never held across await. Callers must run slow filesystem/vault
/// work through cancellation-owned `RemoteBlockingWork`, admitting mutations
/// only after acquiring this context's lock. Install the returned authority
/// only if the async session and its authorization lease remain current.
pub struct DeviceEnrollmentContext<P: IdentityPersistence + 'static, V: DeviceEnrollmentVault> {
    store: IsolatedRemoteStore<P>,
    vault: V,
    binding: MachineBinding,
    retained_register: Option<RetainedRegister>,
    session_epochs: AtomicU64,
}

impl DeviceEnrollmentContext<KernelIdentityPersistence, OsConnectHostVault> {
    pub fn open_production() -> Result<Self, IdentityError> {
        let store = IsolatedRemoteStore::<KernelIdentityPersistence>::open_active_profile()?;
        let identity = store.require_active_profile_identity()?;
        let profile = resolve_host_profile_for_binding()?;
        let binding = derive_machine_binding(&profile)?;
        if identity.profile_binding_hash() != binding.binding_hash() {
            return Err(IdentityError::UnsupportedOperation);
        }
        let root = crate::persistence::app_config_dir()
            .map_err(|_| IdentityError::PersistFailed)?
            .join("connect-host-vault");
        let vault = OsConnectHostVault::open(root, binding.clone())?;
        let _custody = vault.load_host_noise(&identity)?;
        Ok(Self {
            store,
            vault,
            binding,
            retained_register: None,
            session_epochs: AtomicU64::new(1),
        })
    }
}

impl<P, V> DeviceEnrollmentContext<P, V>
where
    P: IdentityPersistence + 'static,
    V: DeviceEnrollmentVault,
{
    #[cfg(test)]
    pub(crate) fn from_parts(
        store: IsolatedRemoteStore<P>,
        vault: V,
        binding: MachineBinding,
    ) -> Self {
        Self {
            store,
            vault,
            binding,
            retained_register: None,
            session_epochs: AtomicU64::new(1),
        }
    }

    pub(crate) fn store(&self) -> &IsolatedRemoteStore<P> {
        &self.store
    }

    pub(crate) fn binding(&self) -> &MachineBinding {
        &self.binding
    }

    pub(crate) fn vault(&self) -> &V {
        &self.vault
    }

    #[cfg(test)]
    pub(crate) fn load_document(&mut self) -> Result<LoadedRemoteDocument, IdentityError> {
        let binding = self.binding.clone();
        self.store.load(&binding, &self.vault)
    }

    #[cfg(test)]
    pub(crate) fn execute_identity_command(
        &mut self,
        command: IdentityCommand,
    ) -> Result<IdentityReceipt, IdentityError> {
        let binding = self.binding.clone();
        self.store.execute(&binding, &mut self.vault, command)
    }

    pub fn identity_live_state(&self) -> Result<ConnectIdentityLiveState, IdentityError> {
        self.store.identity_live_state()
    }

    /// Explicit RegisterDevice-only orphan recovery after crash lost the
    /// retained in-memory command. Reuses store
    /// [`IsolatedRemoteStore::recover_orphaned_register_device_pending`].
    ///
    /// Not called from [`Self::open_production`] — that path must stay load-only
    /// for live identity. The singular `RemoteSetupRuntime` startup seam owns
    /// automatic invocation before listener admission.
    pub fn recover_orphaned_register_device_pending(
        &mut self,
    ) -> Result<bool, DeviceEnrollmentError> {
        let binding = self.binding.clone();
        self.store
            .recover_orphaned_register_device_pending(&binding, &mut self.vault)
            .map_err(DeviceEnrollmentError::Identity)
    }

    pub fn subscribe_authority_invalidation(&self) -> watch::Receiver<u64> {
        self.store.subscribe_authority_invalidation()
    }

    #[cfg(test)]
    pub(crate) fn retained_register_command(&self) -> Option<&IdentityCommand> {
        self.retained_register
            .as_ref()
            .map(|retained| &retained.command)
    }

    /// Simulate process death: drop the in-memory retained RegisterDevice and
    /// release the in-process claim token so recovery must reclaim as a fresh owner.
    #[cfg(test)]
    pub(crate) fn simulate_crash_lost_retained_register_for_test(&mut self) {
        self.retained_register = None;
        self.store.clear_claimed_owner_for_test();
    }

    #[cfg(test)]
    pub(crate) fn abandon_pending_transition_for_test(
        &mut self,
    ) -> Result<LoadedRemoteDocument, IdentityError> {
        let binding = self.binding.clone();
        self.store
            .abandon_pending_transition(&binding, &mut self.vault)
    }

    #[cfg(test)]
    pub(crate) fn into_parts_for_test(self) -> (IsolatedRemoteStore<P>, V, MachineBinding) {
        (self.store, self.vault, self.binding)
    }

    /// Enroll or rebind a Device-kind Noise peer for the HTTP paired browser route.
    ///
    /// Host-kind peers return [`DeviceEnrollmentAuthority::LegacyHostCompat`]
    /// without touching RegisterDevice. Native-kind enrollment is not supported
    /// on this route without additional wire metadata
    /// ([`DeviceEnrollmentError::UnsupportedNativeWire`] is reserved for that
    /// separate path).
    pub fn enroll_or_rebind_paired_browser(
        &mut self,
        peer: AuthenticatedPeer,
        metadata: &BrowserEnrollmentMetadata,
    ) -> Result<DeviceEnrollmentAuthority, DeviceEnrollmentError> {
        validate_browser_metadata(metadata)?;
        let authority_rx = self.store.subscribe_authority_invalidation();
        let legacy_authority_generation = self.store.authority_generation();
        if peer.is_host() {
            if legacy_authority_generation == u64::MAX
                || self.store.identity_live_state()? != ConnectIdentityLiveState::Live
            {
                return Err(DeviceEnrollmentError::IdentityNotLive);
            }
            return Ok(DeviceEnrollmentAuthority::LegacyHostCompat {
                authority_generation: legacy_authority_generation,
                authority_rx,
            });
        }
        if !peer.is_device() {
            return Err(DeviceEnrollmentError::HostClaimNotDevice);
        }

        let fingerprint = noise_public_fingerprint(peer.static_public());
        let live_state = self.store.identity_live_state()?;

        if let Some(retained) = self.retained_register.as_ref() {
            if retained_matches_claim(retained, &fingerprint, metadata) {
                match live_state {
                    ConnectIdentityLiveState::Live | ConnectIdentityLiveState::Pending => {
                        let command = retained.command.clone();
                        // Exact retry: never re-authorize the one-use vault slot.
                        return self.register_browser_device(
                            command,
                            metadata,
                            &fingerprint,
                            authority_rx,
                        );
                    }
                    ConnectIdentityLiveState::Absent => {}
                }
            } else {
                // Foreign peer denial while an exact retry is outstanding.
                return Err(DeviceEnrollmentError::Identity(
                    IdentityError::TransitionPending,
                ));
            }
        }

        match live_state {
            ConnectIdentityLiveState::Pending => {
                return Err(DeviceEnrollmentError::Identity(
                    IdentityError::TransitionPending,
                ));
            }
            ConnectIdentityLiveState::Absent => {
                return Err(DeviceEnrollmentError::IdentityNotLive);
            }
            ConnectIdentityLiveState::Live => {}
        }

        let binding = self.binding.clone();
        let loaded = self.store.load(&binding, &self.vault)?;
        match classify_cookie_device(&loaded, &metadata.paired_cookie_client_id, &fingerprint) {
            CookieDeviceLookup::Rebind(device_id) => {
                return self.bind_existing_device(device_id, &fingerprint, authority_rx);
            }
            CookieDeviceLookup::KeyMismatch => {
                return Err(DeviceEnrollmentError::KeyMismatchRequiresRepair);
            }
            CookieDeviceLookup::RevokedDenied => {
                return Err(DeviceEnrollmentError::Identity(
                    IdentityError::UnknownDevice,
                ));
            }
            CookieDeviceLookup::Absent => {}
        }
        if fingerprint_registered_under_other_cookie(
            &loaded,
            &metadata.paired_cookie_client_id,
            &fingerprint,
        ) {
            return Err(DeviceEnrollmentError::Identity(
                IdentityError::DuplicateDevice,
            ));
        }

        let expected_revision = loaded.revision();
        let command = IdentityCommand {
            command_id: CommandId::new(),
            expected_revision,
            op: IdentityOp::RegisterDevice(RegisterDevice {
                kind: DeviceKind::Browser,
                label: metadata.label.clone(),
                legacy_client_id: Some(metadata.paired_cookie_client_id.clone()),
                browser: Some(BrowserDeviceDto {
                    browser_install_id: metadata.browser_install_id.clone(),
                    nickname: metadata.nickname.clone(),
                    private_identity_storage:
                        BrowserPrivateStorage::WebCryptoNonExportableIndexedDb,
                    cleared_storage_requires_visible_repair: true,
                }),
            }),
        };
        // Authorize before retain so early vault validation errors neither leave a
        // retained claim nor poison a one-use enrollment slot into a blind retry.
        self.vault
            .authorize_device_enrollment(peer, DeviceKind::Browser)
            .map_err(|error| match error {
                IdentityError::InvalidDevice => DeviceEnrollmentError::HostClaimNotDevice,
                other => DeviceEnrollmentError::Identity(other),
            })?;
        // Retain after authorize so a crash mid-execute retries the exact payload
        // without calling authorize_device_enrollment again.
        self.retained_register = Some(RetainedRegister {
            command: command.clone(),
            peer_fingerprint: fingerprint.clone(),
            metadata: metadata.clone(),
        });
        self.register_browser_device(command, metadata, &fingerprint, authority_rx)
    }

    fn bind_existing_device(
        &mut self,
        device_id: DeviceId,
        expected_fingerprint: &str,
        authority_rx: watch::Receiver<u64>,
    ) -> Result<DeviceEnrollmentAuthority, DeviceEnrollmentError> {
        let session_epoch = self.next_session_epoch()?;
        let binding = self.binding.clone();
        // Capture BEFORE reading the credential. A revoke during proof mint
        // must not be adopted as this proof's new, apparently valid baseline.
        let authority_generation = self.store.authority_generation();
        if authority_generation == u64::MAX {
            return Err(DeviceEnrollmentError::IdentityNotLive);
        }
        let proof =
            self.store
                .bind_device_credential(&binding, &self.vault, device_id, session_epoch)?;
        let loaded = self.store.load(&binding, &self.vault)?;
        let identity = loaded
            .identity()
            .ok_or(DeviceEnrollmentError::IdentityNotLive)?;
        let device = identity
            .device(device_id)
            .ok_or(DeviceEnrollmentError::Identity(
                IdentityError::UnknownDevice,
            ))?;
        if device.public_key.fingerprint() != expected_fingerprint
            || device.revoked
            || device.requires_re_pair
        {
            return Err(DeviceEnrollmentError::KeyMismatchRequiresRepair);
        }
        if self.store.authority_generation() != authority_generation {
            return Err(DeviceEnrollmentError::IdentityNotLive);
        }
        self.retained_register = None;
        Ok(DeviceEnrollmentAuthority::Device {
            proof,
            device_id,
            session_epoch,
            authority_generation,
            authority_rx,
        })
    }

    fn register_browser_device(
        &mut self,
        command: IdentityCommand,
        metadata: &BrowserEnrollmentMetadata,
        expected_fingerprint: &str,
        authority_rx: watch::Receiver<u64>,
    ) -> Result<DeviceEnrollmentAuthority, DeviceEnrollmentError> {
        let binding = self.binding.clone();
        let receipt = match self.store.execute(&binding, &mut self.vault, command) {
            Ok(receipt) => receipt,
            Err(error) => {
                return self.map_register_execute_error(error, expected_fingerprint, metadata);
            }
        };

        // Execute settled (or recovered) the device. Clear retention on any
        // post-execute failure so the next call rebinds instead of re-registering.
        let clear_retained = |this: &mut Self| {
            this.retained_register = None;
        };
        let device = match receipt.registered_device().cloned() {
            Some(device) => device,
            None => {
                clear_retained(self);
                return Err(DeviceEnrollmentError::Identity(IdentityError::Corrupt));
            }
        };
        if device.public_key.fingerprint() != expected_fingerprint {
            clear_retained(self);
            return Err(DeviceEnrollmentError::KeyMismatchRequiresRepair);
        }
        let session_epoch = match self.next_session_epoch() {
            Ok(epoch) => epoch,
            Err(error) => {
                clear_retained(self);
                return Err(error);
            }
        };
        let authority_generation = self.store.authority_generation();
        if authority_generation == u64::MAX {
            clear_retained(self);
            return Err(DeviceEnrollmentError::IdentityNotLive);
        }
        let proof = match self.store.bind_device_credential(
            &binding,
            &self.vault,
            device.device_id,
            session_epoch,
        ) {
            Ok(proof) => proof,
            Err(error) => {
                clear_retained(self);
                return Err(DeviceEnrollmentError::Identity(error));
            }
        };
        clear_retained(self);
        if self.store.authority_generation() != authority_generation {
            return Err(DeviceEnrollmentError::IdentityNotLive);
        }
        Ok(DeviceEnrollmentAuthority::Device {
            proof,
            device_id: device.device_id,
            session_epoch,
            authority_generation,
            authority_rx,
        })
    }

    fn map_register_execute_error(
        &mut self,
        error: IdentityError,
        _expected_fingerprint: &str,
        _metadata: &BrowserEnrollmentMetadata,
    ) -> Result<DeviceEnrollmentAuthority, DeviceEnrollmentError> {
        match error {
            // Exact retry must keep the retained command; foreign denial is
            // handled before register and never clears retained either.
            IdentityError::TransitionPending
            | IdentityError::CommandConflict
            | IdentityError::PersistFailed => Err(DeviceEnrollmentError::Identity(error)),
            other => {
                self.retained_register = None;
                Err(DeviceEnrollmentError::Identity(other))
            }
        }
    }

    fn next_session_epoch(&self) -> Result<u64, DeviceEnrollmentError> {
        loop {
            let current = self.session_epochs.load(Ordering::Acquire);
            if current == 0 || current == u64::MAX {
                return Err(DeviceEnrollmentError::Identity(IdentityError::Overflow));
            }
            match self.session_epochs.compare_exchange(
                current,
                current + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(previous) => return Ok(previous),
                Err(_) => continue,
            }
        }
    }
}

/// Production startup-owned enrollment handle. Mutex is never held across await.
pub struct SharedDeviceEnrollment {
    inner: Mutex<DeviceEnrollmentContext<KernelIdentityPersistence, OsConnectHostVault>>,
}

impl SharedDeviceEnrollment {
    pub fn open_production() -> Result<Self, IdentityError> {
        Ok(Self {
            inner: Mutex::new(DeviceEnrollmentContext::open_production()?),
        })
    }

    pub fn lock(
        &self,
    ) -> MutexGuard<'_, DeviceEnrollmentContext<KernelIdentityPersistence, OsConnectHostVault>>
    {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub fn identity_live_state(&self) -> Result<ConnectIdentityLiveState, IdentityError> {
        self.lock().identity_live_state()
    }

    pub fn subscribe_authority_invalidation(&self) -> watch::Receiver<u64> {
        self.lock().subscribe_authority_invalidation()
    }

    pub fn read_store_clone(&self) -> IsolatedRemoteStore<KernelIdentityPersistence> {
        self.lock().store().clone()
    }

    /// Blocking enrollment entry used from `spawn_blocking`.
    pub fn enroll_or_rebind_paired_browser(
        &self,
        peer: AuthenticatedPeer,
        metadata: BrowserEnrollmentMetadata,
    ) -> Result<DeviceEnrollmentAuthority, DeviceEnrollmentError> {
        self.lock().enroll_or_rebind_paired_browser(peer, &metadata)
    }
}

impl fmt::Debug for SharedDeviceEnrollment {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SharedDeviceEnrollment(redacted)")
    }
}

fn validate_browser_metadata(
    metadata: &BrowserEnrollmentMetadata,
) -> Result<(), DeviceEnrollmentError> {
    if metadata.paired_cookie_client_id.is_empty()
        || metadata.paired_cookie_client_id.len() > MAX_ID_BYTES
        || metadata
            .paired_cookie_client_id
            .chars()
            .any(char::is_control)
    {
        return Err(DeviceEnrollmentError::InvalidMetadata);
    }
    if metadata.browser_install_id.is_empty()
        || metadata.browser_install_id.len() > MAX_ID_BYTES
        || metadata.browser_install_id.chars().any(char::is_control)
    {
        return Err(DeviceEnrollmentError::InvalidMetadata);
    }
    if metadata.label.is_empty()
        || metadata.label.len() > MAX_LABEL_BYTES
        || metadata.label.chars().any(char::is_control)
    {
        return Err(DeviceEnrollmentError::InvalidMetadata);
    }
    if metadata.nickname.as_deref().is_some_and(|nickname| {
        nickname.len() > MAX_LABEL_BYTES || nickname.chars().any(char::is_control)
    }) {
        return Err(DeviceEnrollmentError::InvalidMetadata);
    }
    Ok(())
}

fn noise_public_fingerprint(public: NoiseStaticPublicKey) -> String {
    hex_encode(&Sha256::digest(public.as_bytes()))
}

fn retained_matches_claim(
    retained: &RetainedRegister,
    fingerprint: &str,
    metadata: &BrowserEnrollmentMetadata,
) -> bool {
    if retained.peer_fingerprint != fingerprint || retained.metadata != *metadata {
        return false;
    }
    match &retained.command.op {
        IdentityOp::RegisterDevice(request) => {
            request.kind == DeviceKind::Browser
                && request.label == metadata.label
                && request.legacy_client_id.as_deref()
                    == Some(metadata.paired_cookie_client_id.as_str())
                && request.browser.as_ref().is_some_and(|browser| {
                    browser.browser_install_id == metadata.browser_install_id
                        && browser.nickname == metadata.nickname
                })
        }
        _ => false,
    }
}

enum CookieDeviceLookup {
    Rebind(DeviceId),
    KeyMismatch,
    RevokedDenied,
    Absent,
}

fn classify_cookie_device(
    loaded: &LoadedRemoteDocument,
    cookie: &str,
    fingerprint: &str,
) -> CookieDeviceLookup {
    let Some(identity) = loaded.identity() else {
        return CookieDeviceLookup::Absent;
    };
    let mut active_exact = None;
    let mut active_mismatch = false;
    let mut revoked_denial = false;
    for device in identity.devices() {
        let cookie_match = device.legacy_client_id.as_deref() == Some(cookie);
        let fingerprint_match = device.public_key.fingerprint() == fingerprint;
        if device.revoked {
            if cookie_match || fingerprint_match {
                revoked_denial = true;
            }
            continue;
        }
        if cookie_match {
            if fingerprint_match {
                active_exact = Some(device.device_id);
            } else {
                active_mismatch = true;
            }
        }
    }
    if let Some(device_id) = active_exact {
        return CookieDeviceLookup::Rebind(device_id);
    }
    if active_mismatch {
        return CookieDeviceLookup::KeyMismatch;
    }
    if revoked_denial {
        return CookieDeviceLookup::RevokedDenied;
    }
    CookieDeviceLookup::Absent
}

fn fingerprint_registered_under_other_cookie(
    loaded: &LoadedRemoteDocument,
    cookie: &str,
    fingerprint: &str,
) -> bool {
    let Some(identity) = loaded.identity() else {
        return false;
    };
    identity.devices().iter().any(|device| {
        !device.revoked
            && device.public_key.fingerprint() == fingerprint
            && device.legacy_client_id.as_deref() != Some(cookie)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connect::identity::{
        DeviceEstablishmentHandle, DeviceKeyProof, DeviceRepairHandle, HostEstablishmentHandle,
        HostKeyProof, HostPublicId, HostRotationHandle,
    };
    use crate::connect::identity_store::InMemoryIdentityPersistence;
    use crate::domain::id::CommandId;
    use crate::protocol::{
        instantiate_noise_channel, ChannelRole, CredentialPurpose, CryptoPrologue, NoiseCustody,
        NoiseIdentityBinding, NOISE_FIRST_PAIRING_PATTERN, PROTOCOL_MAJOR,
    };
    use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
    use std::sync::Arc;

    fn binding_for(profile: &str) -> MachineBinding {
        derive_machine_binding(profile).expect("binding")
    }

    fn open_vault(root: std::path::PathBuf, profile: &str) -> OsConnectHostVault {
        OsConnectHostVault::open(root, binding_for(profile)).expect("open vault")
    }

    fn tempfile_vault(label: &str) -> (tempfile::TempDir, OsConnectHostVault, MachineBinding) {
        let temp = tempfile::Builder::new()
            .prefix(&format!("devmanager-enroll-bridge-{label}-"))
            .tempdir()
            .expect("tempdir");
        let root = temp
            .path()
            .canonicalize()
            .unwrap_or_else(|_| temp.path().to_path_buf());
        let profile = format!("enroll-bridge-{label}");
        let binding = binding_for(&profile);
        let vault = open_vault(root, &profile);
        (temp, vault, binding)
    }

    fn enable_identity<P: IdentityPersistence + 'static>(
        store: &mut IsolatedRemoteStore<P>,
        vault: &mut OsConnectHostVault,
        binding: &MachineBinding,
    ) {
        store
            .execute(
                binding,
                vault,
                IdentityCommand {
                    command_id: CommandId::new(),
                    expected_revision: 0,
                    op: IdentityOp::Enable {
                        host_build: 1,
                        now_epoch_ms: 100,
                    },
                },
            )
            .expect("enable");
    }

    fn noise_device_peer() -> AuthenticatedPeer {
        let device_keys = NoiseCustody::generate().expect("device");
        let host_keys = NoiseCustody::generate().expect("host");
        let prologue = CryptoPrologue::new(
            PROTOCOL_MAJOR,
            CredentialPurpose::OwnerPairing,
            [9; 16],
            [8; 16],
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
            NoiseIdentityBinding::host_device([0x33; 16], [0x22; 16]),
            40,
            true,
        )
        .expect("device hs");
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
        .expect("host hs");
        let msg1 = device.write_message().expect("msg1");
        host.read_message(&msg1).expect("read1");
        let msg2 = host.write_message().expect("msg2");
        device.read_message(&msg2).expect("read2");
        let msg3 = device.write_message().expect("msg3");
        host.read_message(&msg3).expect("read3");
        let peer = host.finish().expect("finish").remote_peer();
        assert!(peer.is_device());
        peer
    }

    fn noise_host_peer() -> AuthenticatedPeer {
        let left = NoiseCustody::generate().expect("left");
        let right = NoiseCustody::generate().expect("right");
        let prologue = CryptoPrologue::new(
            PROTOCOL_MAJOR,
            CredentialPurpose::OwnerPairing,
            [7; 16],
            [6; 16],
        )
        .expect("prologue");
        let mut initiator = instantiate_noise_channel(
            NOISE_FIRST_PAIRING_PATTERN,
            true,
            left.private(),
            left.public(),
            None,
            prologue,
            ChannelRole::Initiator,
            NoiseIdentityBinding::host([0x44; 16]),
            40,
            true,
        )
        .expect("left");
        let mut responder = instantiate_noise_channel(
            NOISE_FIRST_PAIRING_PATTERN,
            true,
            right.private(),
            right.public(),
            None,
            prologue,
            ChannelRole::Responder,
            NoiseIdentityBinding::host([0x44; 16]),
            40,
            true,
        )
        .expect("right");
        let msg1 = initiator.write_message().expect("msg1");
        responder.read_message(&msg1).expect("read1");
        let msg2 = responder.write_message().expect("msg2");
        initiator.read_message(&msg2).expect("read2");
        let msg3 = initiator.write_message().expect("msg3");
        responder.read_message(&msg3).expect("read3");
        let peer = responder.finish().expect("finish").remote_peer();
        assert!(peer.is_host());
        peer
    }

    fn metadata(cookie: &str) -> BrowserEnrollmentMetadata {
        BrowserEnrollmentMetadata {
            paired_cookie_client_id: cookie.to_string(),
            label: "Phone".to_string(),
            browser_install_id: format!("{cookie}-install"),
            nickname: Some("Desk".to_string()),
        }
    }

    /// Fault-injection vault: first `commit_device_establishment` fails once.
    struct FailOnceCommitVault {
        inner: OsConnectHostVault,
        fail_once: Arc<AtomicBool>,
    }

    impl CredentialVault for FailOnceCommitVault {
        fn establish_host(
            &mut self,
            host_id: HostPublicId,
            transition_nonce: [u8; 16],
        ) -> Result<HostEstablishmentHandle, IdentityError> {
            self.inner.establish_host(host_id, transition_nonce)
        }
        fn commit_host_establishment(
            &mut self,
            handle: &HostEstablishmentHandle,
        ) -> Result<(), IdentityError> {
            self.inner.commit_host_establishment(handle)
        }
        fn rollback_host_establishment(
            &mut self,
            handle: &HostEstablishmentHandle,
        ) -> Result<(), IdentityError> {
            self.inner.rollback_host_establishment(handle)
        }
        fn recover_host_establishment(
            &mut self,
            host_id: HostPublicId,
            transition_nonce: [u8; 16],
        ) -> Result<Option<HostEstablishmentHandle>, IdentityError> {
            self.inner
                .recover_host_establishment(host_id, transition_nonce)
        }
        fn host_establishment_committed(
            &self,
            handle: &HostEstablishmentHandle,
        ) -> Result<bool, IdentityError> {
            self.inner.host_establishment_committed(handle)
        }
        fn prepare_host_rotation(
            &mut self,
            host_id: HostPublicId,
            transition_nonce: [u8; 16],
        ) -> Result<HostRotationHandle, IdentityError> {
            self.inner.prepare_host_rotation(host_id, transition_nonce)
        }
        fn commit_host_rotation(
            &mut self,
            handle: &HostRotationHandle,
        ) -> Result<(), IdentityError> {
            self.inner.commit_host_rotation(handle)
        }
        fn abort_host_rotation(
            &mut self,
            handle: &HostRotationHandle,
        ) -> Result<(), IdentityError> {
            self.inner.abort_host_rotation(handle)
        }
        fn recover_host_rotation(
            &mut self,
            host_id: HostPublicId,
            transition_nonce: [u8; 16],
        ) -> Result<Option<HostRotationHandle>, IdentityError> {
            self.inner.recover_host_rotation(host_id, transition_nonce)
        }
        fn verify_host(
            &self,
            host_id: HostPublicId,
            proof: &HostKeyProof,
        ) -> Result<(), IdentityError> {
            self.inner.verify_host(host_id, proof)
        }
        fn establish_device(
            &mut self,
            device_id: DeviceId,
            kind: DeviceKind,
            transition_nonce: [u8; 16],
        ) -> Result<DeviceEstablishmentHandle, IdentityError> {
            self.inner
                .establish_device(device_id, kind, transition_nonce)
        }
        fn commit_device_establishment(
            &mut self,
            handle: &DeviceEstablishmentHandle,
        ) -> Result<(), IdentityError> {
            if self.fail_once.swap(false, AtomicOrdering::SeqCst) {
                return Err(IdentityError::PersistFailed);
            }
            self.inner.commit_device_establishment(handle)
        }
        fn recover_device_establishment(
            &mut self,
            device_id: DeviceId,
            transition_nonce: [u8; 16],
        ) -> Result<Option<DeviceEstablishmentHandle>, IdentityError> {
            self.inner
                .recover_device_establishment(device_id, transition_nonce)
        }
        fn device_establishment_committed(
            &self,
            handle: &DeviceEstablishmentHandle,
        ) -> Result<bool, IdentityError> {
            self.inner.device_establishment_committed(handle)
        }
        fn prepare_device_repair(
            &mut self,
            device_id: DeviceId,
            kind: DeviceKind,
            transition_nonce: [u8; 16],
        ) -> Result<DeviceRepairHandle, IdentityError> {
            self.inner
                .prepare_device_repair(device_id, kind, transition_nonce)
        }
        fn commit_device_repair(
            &mut self,
            handle: &DeviceRepairHandle,
        ) -> Result<(), IdentityError> {
            self.inner.commit_device_repair(handle)
        }
        fn device_repair_committed(
            &self,
            handle: &DeviceRepairHandle,
        ) -> Result<bool, IdentityError> {
            self.inner.device_repair_committed(handle)
        }
        fn rollback_device_repair(
            &mut self,
            handle: &DeviceRepairHandle,
        ) -> Result<(), IdentityError> {
            self.inner.rollback_device_repair(handle)
        }
        fn abort_device_repair(
            &mut self,
            handle: &DeviceRepairHandle,
        ) -> Result<(), IdentityError> {
            self.inner.abort_device_repair(handle)
        }
        fn recover_device_repair(
            &mut self,
            device_id: DeviceId,
            transition_nonce: [u8; 16],
        ) -> Result<Option<DeviceRepairHandle>, IdentityError> {
            self.inner
                .recover_device_repair(device_id, transition_nonce)
        }
        fn invalidate_device_credential(
            &mut self,
            device_id: DeviceId,
            revocation_epoch: u64,
        ) -> Result<(), IdentityError> {
            self.inner
                .invalidate_device_credential(device_id, revocation_epoch)
        }
        fn restore_device_credential(
            &mut self,
            device_id: DeviceId,
            revocation_epoch: u64,
        ) -> Result<(), IdentityError> {
            self.inner
                .restore_device_credential(device_id, revocation_epoch)
        }
        fn rollback_device_establishment(
            &mut self,
            handle: &DeviceEstablishmentHandle,
        ) -> Result<(), IdentityError> {
            self.inner.rollback_device_establishment(handle)
        }
        fn verify_device(
            &self,
            device_id: DeviceId,
            proof: &DeviceKeyProof,
        ) -> Result<(), IdentityError> {
            self.inner.verify_device(device_id, proof)
        }
    }

    impl DeviceEnrollmentVault for FailOnceCommitVault {
        fn authorize_device_enrollment(
            &mut self,
            peer: AuthenticatedPeer,
            kind: DeviceKind,
        ) -> Result<(), IdentityError> {
            OsConnectHostVault::authorize_device_enrollment(&mut self.inner, peer, kind)
        }
    }

    #[cfg(windows)]
    #[test]
    fn device_credential_epoch_zero_does_not_enable_legacy() {
        let (_temp, mut vault, binding) = tempfile_vault("epoch0");
        let mut store =
            IsolatedRemoteStore::new(InMemoryIdentityPersistence::default()).expect("store");
        enable_identity(&mut store, &mut vault, &binding);
        let mut ctx = DeviceEnrollmentContext::from_parts(store, vault, binding);
        let peer = noise_device_peer();
        let authority = ctx
            .enroll_or_rebind_paired_browser(peer, &metadata("cookie-epoch0"))
            .expect("enroll");
        let DeviceEnrollmentAuthority::Device {
            proof,
            session_epoch,
            ..
        } = authority
        else {
            panic!("device");
        };
        assert_ne!(session_epoch, 0);
        let zero = crate::connect::ConnectDispatchSession::bind_paired(
            "web-paired-owner".to_owned(),
            ConnectIdentityLiveState::Live,
        )
        .with_device_credential(proof.clone(), 0);
        assert!(!zero.legacy_host_compat_for_test());
        assert!(!zero.has_device_credential_for_test());
        let live = crate::connect::ConnectDispatchSession::bind_paired(
            "web-paired-owner".to_owned(),
            ConnectIdentityLiveState::Live,
        )
        .with_device_credential(proof, session_epoch);
        assert!(live.has_device_credential_for_test());
        assert!(!live.legacy_host_compat_for_test());
    }

    #[cfg(windows)]
    #[test]
    fn register_once_reconnect_rebinds_same_device() {
        let (_temp, mut vault, binding) = tempfile_vault("once");
        let mut store =
            IsolatedRemoteStore::new(InMemoryIdentityPersistence::default()).expect("store");
        enable_identity(&mut store, &mut vault, &binding);
        let mut ctx = DeviceEnrollmentContext::from_parts(store, vault, binding);
        let peer = noise_device_peer();
        let meta = metadata("cookie-a");
        let first = ctx
            .enroll_or_rebind_paired_browser(peer, &meta)
            .expect("enroll");
        let DeviceEnrollmentAuthority::Device {
            device_id: first_id,
            ..
        } = first
        else {
            panic!("expected device authority");
        };
        let second = ctx
            .enroll_or_rebind_paired_browser(peer, &meta)
            .expect("rebind");
        let DeviceEnrollmentAuthority::Device {
            device_id: second_id,
            proof,
            session_epoch,
            authority_generation,
            ..
        } = second
        else {
            panic!("expected device authority on reconnect");
        };
        assert_eq!(first_id, second_id);
        assert_eq!(proof.device_id(), first_id);
        assert_ne!(session_epoch, 0);
        assert_eq!(authority_generation, ctx.store().authority_generation());
        assert_eq!(
            ctx.load_document()
                .expect("load")
                .identity()
                .expect("identity")
                .devices()
                .iter()
                .filter(|device| !device.revoked)
                .count(),
            1
        );
    }

    #[cfg(windows)]
    #[test]
    fn key_mismatch_on_same_cookie_fails_closed() {
        let (_temp, mut vault, binding) = tempfile_vault("mismatch");
        let mut store =
            IsolatedRemoteStore::new(InMemoryIdentityPersistence::default()).expect("store");
        enable_identity(&mut store, &mut vault, &binding);
        let mut ctx = DeviceEnrollmentContext::from_parts(store, vault, binding);
        let peer_a = noise_device_peer();
        let meta = metadata("cookie-b");
        ctx.enroll_or_rebind_paired_browser(peer_a, &meta)
            .expect("enroll a");
        let peer_b = noise_device_peer();
        let err = ctx
            .enroll_or_rebind_paired_browser(peer_b, &meta)
            .expect_err("key mismatch");
        assert!(matches!(
            err,
            DeviceEnrollmentError::KeyMismatchRequiresRepair
        ));
    }

    #[cfg(windows)]
    #[test]
    fn host_claim_is_legacy_compat_not_enrollment() {
        let (_temp, mut vault, binding) = tempfile_vault("host");
        let mut store =
            IsolatedRemoteStore::new(InMemoryIdentityPersistence::default()).expect("store");
        enable_identity(&mut store, &mut vault, &binding);
        let mut ctx = DeviceEnrollmentContext::from_parts(store, vault, binding);
        let host_peer = noise_host_peer();
        let authority = ctx
            .enroll_or_rebind_paired_browser(host_peer, &metadata("cookie-host"))
            .expect("legacy host");
        assert!(authority.is_legacy_host_compat());
        assert!(authority.device_credential().is_none());
        assert!(ctx
            .load_document()
            .expect("load")
            .identity()
            .expect("identity")
            .devices()
            .is_empty());
    }

    #[cfg(windows)]
    #[test]
    fn revoked_cookie_device_is_denied_not_reregistered() {
        let (_temp, mut vault, binding) = tempfile_vault("revoke");
        let mut store =
            IsolatedRemoteStore::new(InMemoryIdentityPersistence::default()).expect("store");
        enable_identity(&mut store, &mut vault, &binding);
        let mut ctx = DeviceEnrollmentContext::from_parts(store, vault, binding);
        let peer = noise_device_peer();
        let meta = metadata("cookie-rev");
        let authority = ctx
            .enroll_or_rebind_paired_browser(peer, &meta)
            .expect("enroll");
        let mut rx = authority.subscribe_authority();
        let before = *rx.borrow();
        let DeviceEnrollmentAuthority::Device { device_id, .. } = authority else {
            panic!("device");
        };
        let revision = ctx.load_document().expect("load").revision();
        ctx.execute_identity_command(IdentityCommand {
            command_id: CommandId::new(),
            expected_revision: revision,
            op: IdentityOp::RevokeDevice {
                device_id,
                now_epoch_ms: 999,
            },
        })
        .expect("revoke");
        assert_ne!(*rx.borrow(), before);
        let err = ctx
            .enroll_or_rebind_paired_browser(peer, &meta)
            .expect_err("revoked must fail closed");
        assert!(
            matches!(
                err,
                DeviceEnrollmentError::Identity(IdentityError::UnknownDevice)
            ),
            "expected UnknownDevice, got {err:?}"
        );
    }

    #[cfg(windows)]
    #[test]
    fn fault_injected_pending_exact_retry_and_foreign_peer_denied() {
        let (_temp, inner, binding) = tempfile_vault("fault");
        let fail_once = Arc::new(AtomicBool::new(true));
        let mut vault = FailOnceCommitVault {
            inner,
            fail_once: Arc::clone(&fail_once),
        };
        let mut store =
            IsolatedRemoteStore::new(InMemoryIdentityPersistence::default()).expect("store");
        enable_identity(&mut store, &mut vault.inner, &binding);
        let mut ctx = DeviceEnrollmentContext::from_parts(store, vault, binding);
        let peer = noise_device_peer();
        let meta = metadata("cookie-fault");
        let first_err = ctx
            .enroll_or_rebind_paired_browser(peer, &meta)
            .expect_err("first enroll must fail vault commit");
        assert!(
            matches!(
                first_err,
                DeviceEnrollmentError::Identity(IdentityError::PersistFailed)
            ),
            "expected PersistFailed, got {first_err:?}"
        );
        assert!(ctx.retained_register_command().is_some());

        let foreign = noise_device_peer();
        let foreign_err = ctx
            .enroll_or_rebind_paired_browser(foreign, &meta)
            .expect_err("foreign peer denied while retained");
        assert!(matches!(
            foreign_err,
            DeviceEnrollmentError::Identity(IdentityError::TransitionPending)
        ));
        assert!(ctx.retained_register_command().is_some());

        let second = ctx
            .enroll_or_rebind_paired_browser(peer, &meta)
            .expect("exact retry");
        let DeviceEnrollmentAuthority::Device { device_id, .. } = second else {
            panic!("device");
        };
        assert!(ctx.retained_register_command().is_none());

        let third = ctx
            .enroll_or_rebind_paired_browser(peer, &meta)
            .expect("rebind after settle");
        let DeviceEnrollmentAuthority::Device {
            device_id: again_id,
            ..
        } = third
        else {
            panic!("device");
        };
        assert_eq!(device_id, again_id);
    }

    #[cfg(windows)]
    #[test]
    fn authority_watch_wakes_on_revocation() {
        let (_temp, mut vault, binding) = tempfile_vault("wake");
        let mut store =
            IsolatedRemoteStore::new(InMemoryIdentityPersistence::default()).expect("store");
        enable_identity(&mut store, &mut vault, &binding);
        let mut ctx = DeviceEnrollmentContext::from_parts(store, vault, binding);
        let peer = noise_device_peer();
        let authority = ctx
            .enroll_or_rebind_paired_browser(peer, &metadata("cookie-wake"))
            .expect("enroll");
        let mut rx = authority.subscribe_authority();
        let DeviceEnrollmentAuthority::Device { device_id, .. } = &authority else {
            panic!("device");
        };
        let revision = ctx.load_document().expect("load").revision();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let changed = runtime.block_on(async {
            let wait = tokio::spawn(async move { rx.changed().await.is_ok() });
            ctx.execute_identity_command(IdentityCommand {
                command_id: CommandId::new(),
                expected_revision: revision,
                op: IdentityOp::RevokeDevice {
                    device_id: *device_id,
                    now_epoch_ms: 50,
                },
            })
            .expect("revoke");
            wait.await.expect("join")
        });
        assert!(changed, "idle duplex must observe identity revocation wake");
    }

    #[cfg(windows)]
    #[test]
    fn shared_authority_notifier_across_same_kernel_path() {
        let (_temp, mut vault, binding) = tempfile_vault("shared-auth");
        let persistence = InMemoryIdentityPersistence::default();
        let mut store_a = IsolatedRemoteStore::new(persistence.clone()).expect("store a");
        let mut store_b = IsolatedRemoteStore::new(persistence).expect("store b");
        enable_identity(&mut store_a, &mut vault, &binding);

        let mut rx_b = store_b.subscribe_authority_invalidation();
        let before = *rx_b.borrow();

        // Register + revoke through store_a; store_b must observe the shared wake.
        let mut ctx = DeviceEnrollmentContext::from_parts(store_a, vault, binding.clone());
        let peer = noise_device_peer();
        let meta = metadata("cookie-shared");
        let authority = ctx
            .enroll_or_rebind_paired_browser(peer, &meta)
            .expect("enroll");
        let DeviceEnrollmentAuthority::Device { device_id, .. } = authority else {
            panic!("device");
        };
        let revision = ctx.load_document().expect("load").revision();
        ctx.execute_identity_command(IdentityCommand {
            command_id: CommandId::new(),
            expected_revision: revision,
            op: IdentityOp::RevokeDevice {
                device_id,
                now_epoch_ms: 77,
            },
        })
        .expect("revoke");
        assert_ne!(*rx_b.borrow(), before);

        // Independent default stores must not share a notifier.
        let mut other_a =
            IsolatedRemoteStore::new(InMemoryIdentityPersistence::default()).expect("other a");
        let other_b =
            IsolatedRemoteStore::new(InMemoryIdentityPersistence::default()).expect("other b");
        let mut other_rx = other_b.subscribe_authority_invalidation();
        let other_before = *other_rx.borrow();
        let (_temp2, mut vault2, binding2) = tempfile_vault("shared-auth-indep");
        enable_identity(&mut other_a, &mut vault2, &binding2);
        let revision2 = other_a.load(&binding2, &vault2).expect("load").revision();
        // NoteHostBuild does not bump authority; revoke-all on empty devices still bumps.
        other_a
            .execute(
                &binding2,
                &mut vault2,
                IdentityCommand {
                    command_id: CommandId::new(),
                    expected_revision: revision2,
                    op: IdentityOp::RevokeAllDevices { now_epoch_ms: 88 },
                },
            )
            .expect("revoke all");
        assert_eq!(
            *other_rx.borrow(),
            other_before,
            "distinct InMemory authority_id values must not share notifiers"
        );
        let _ = other_rx.has_changed();
    }

    /// Persistence that fails once when clearing a durable marker key from JSON.
    struct FailOnceClearMarkerPersistence {
        inner: InMemoryIdentityPersistence,
        marker: &'static str,
        fail_once: Arc<AtomicBool>,
    }

    impl IdentityPersistence for FailOnceClearMarkerPersistence {
        fn current_revision(&self) -> u64 {
            self.inner.current_revision()
        }

        fn read_bounded(&self, max_bytes: usize) -> Result<Option<Vec<u8>>, IdentityError> {
            self.inner.read_bounded(max_bytes)
        }

        fn compare_and_swap(
            &mut self,
            expected_revision: u64,
            bytes: &[u8],
        ) -> Result<u64, IdentityError> {
            let expected = self.inner.snapshot_bytes();
            self.compare_and_swap_exact(expected_revision, expected.as_deref(), bytes)
        }

        fn compare_and_swap_exact(
            &mut self,
            expected_revision: u64,
            expected_bytes: Option<&[u8]>,
            bytes: &[u8],
        ) -> Result<u64, IdentityError> {
            let old_has = expected_bytes
                .and_then(|raw| std::str::from_utf8(raw).ok())
                .is_some_and(|text| text.contains(self.marker));
            let new_has = std::str::from_utf8(bytes)
                .ok()
                .is_some_and(|text| text.contains(self.marker));
            if old_has && !new_has && self.fail_once.swap(false, AtomicOrdering::SeqCst) {
                return Err(IdentityError::PersistFailed);
            }
            self.inner
                .compare_and_swap_exact(expected_revision, expected_bytes, bytes)
        }

        fn replace_pending(
            &mut self,
            expected_revision: u64,
            bytes: &[u8],
        ) -> Result<u64, IdentityError> {
            self.inner.replace_pending(expected_revision, bytes)
        }

        fn authority_storage_key(&self) -> super::super::identity_store::AuthorityStorageKey {
            self.inner.authority_storage_key()
        }
    }

    #[cfg(windows)]
    #[test]
    fn orphaned_register_prepared_fresh_owner_removes_marker_device_preserves_older() {
        use super::super::identity::PendingIdentityTransitionKind;

        let (_temp, inner, binding) = tempfile_vault("orphan-prep");
        let fail_once = Arc::new(AtomicBool::new(false));
        let mut vault = FailOnceCommitVault {
            inner,
            fail_once: Arc::clone(&fail_once),
        };
        let persistence = InMemoryIdentityPersistence::default();
        let mut store = IsolatedRemoteStore::new(persistence).expect("store");
        enable_identity(&mut store, &mut vault.inner, &binding);
        let mut ctx = DeviceEnrollmentContext::from_parts(store, vault, binding);

        let older = ctx
            .enroll_or_rebind_paired_browser(noise_device_peer(), &metadata("cookie-older"))
            .expect("older enroll");
        let DeviceEnrollmentAuthority::Device {
            device_id: older_id,
            ..
        } = older
        else {
            panic!("older device");
        };

        fail_once.store(true, AtomicOrdering::SeqCst);
        let pending_err = ctx
            .enroll_or_rebind_paired_browser(noise_device_peer(), &metadata("cookie-orphan"))
            .expect_err("prepared commit must fail");
        assert!(matches!(
            pending_err,
            DeviceEnrollmentError::Identity(IdentityError::PersistFailed)
        ));
        let pending = ctx
            .store()
            .pending_transition_for_test()
            .expect("pending read")
            .expect("pending marker");
        assert_eq!(pending.kind, PendingIdentityTransitionKind::RegisterDevice);
        let pending_device_id = pending.device_id.expect("register device id");

        // Crash: retained command + in-process claim token are gone.
        ctx.simulate_crash_lost_retained_register_for_test();
        assert!(ctx.retained_register_command().is_none());
        assert!(ctx
            .recover_orphaned_register_device_pending()
            .expect("recover prepared"));
        assert!(matches!(
            ctx.identity_live_state().expect("live"),
            ConnectIdentityLiveState::Live
        ));
        let loaded = ctx.load_document().expect("load");
        assert!(!loaded.has_pending_transition());
        let identity = loaded.identity().expect("identity");
        assert!(identity.device(older_id).is_some());
        assert!(identity.device(pending_device_id).is_none());
        assert!(loaded.receipt_for_command(pending.command_id).is_none());
    }

    #[cfg(windows)]
    #[test]
    fn orphaned_register_committed_preserves_exact_device_receipt_fingerprint() {
        use super::super::identity::PendingIdentityTransitionKind;

        let (_temp, vault, binding) = tempfile_vault("orphan-commit");
        let fail_clear = Arc::new(AtomicBool::new(false));
        let persistence = FailOnceClearMarkerPersistence {
            inner: InMemoryIdentityPersistence::default(),
            marker: "connectPendingTransition",
            fail_once: Arc::clone(&fail_clear),
        };
        let mut store = IsolatedRemoteStore::new(persistence).expect("store");
        let mut vault = vault;
        enable_identity(&mut store, &mut vault, &binding);
        fail_clear.store(true, AtomicOrdering::SeqCst);
        let mut ctx = DeviceEnrollmentContext::from_parts(store, vault, binding);
        let peer = noise_device_peer();
        let meta = metadata("cookie-committed");
        let first_err = ctx
            .enroll_or_rebind_paired_browser(peer, &meta)
            .expect_err("clear-pending fault");
        assert!(matches!(
            first_err,
            DeviceEnrollmentError::Identity(IdentityError::PersistFailed)
        ));
        let pending = ctx
            .store()
            .pending_transition_for_test()
            .expect("pending")
            .expect("marker retained after vault commit");
        assert_eq!(pending.kind, PendingIdentityTransitionKind::RegisterDevice);
        let device_id = pending.device_id.expect("device id");
        let command_id = pending.command_id;
        let loaded_before = ctx.load_document().expect("load");
        let fingerprint = loaded_before
            .identity()
            .expect("identity")
            .device(device_id)
            .expect("committed device row")
            .public_key
            .fingerprint()
            .to_string();
        assert!(loaded_before.receipt_for_command(command_id).is_some());

        ctx.simulate_crash_lost_retained_register_for_test();
        assert!(ctx
            .recover_orphaned_register_device_pending()
            .expect("recover committed"));
        let loaded = ctx.load_document().expect("load");
        assert!(!loaded.has_pending_transition());
        let device_after = loaded
            .identity()
            .expect("identity")
            .device(device_id)
            .expect("preserved device");
        assert_eq!(device_after.device_id, device_id);
        assert_eq!(device_after.public_key.fingerprint(), fingerprint);
        let receipt_after = loaded
            .receipt_for_command(command_id)
            .expect("preserved receipt");
        assert_eq!(
            receipt_after
                .registered_device()
                .map(|device| device.device_id),
            Some(device_id)
        );
    }

    #[cfg(windows)]
    #[test]
    fn orphaned_register_recovery_refuses_enable_pending_and_revocation() {
        let (_temp, mut vault, binding) = tempfile_vault("orphan-refuse");
        let fail_clear = Arc::new(AtomicBool::new(true));
        let persistence = FailOnceClearMarkerPersistence {
            inner: InMemoryIdentityPersistence::default(),
            marker: "connectPendingTransition",
            fail_once: Arc::clone(&fail_clear),
        };
        let mut store = IsolatedRemoteStore::new(persistence).expect("store");
        let enable_err = store
            .execute(
                &binding,
                &mut vault,
                IdentityCommand {
                    command_id: CommandId::new(),
                    expected_revision: 0,
                    op: IdentityOp::Enable {
                        host_build: 1,
                        now_epoch_ms: 100,
                    },
                },
            )
            .expect_err("enable clear fault leaves Enable pending");
        assert!(matches!(enable_err, IdentityError::PersistFailed));
        let mut ctx = DeviceEnrollmentContext::from_parts(store, vault, binding.clone());
        let enable_refuse = ctx
            .recover_orphaned_register_device_pending()
            .expect_err("Enable pending must fail closed");
        assert!(matches!(
            enable_refuse,
            DeviceEnrollmentError::Identity(IdentityError::UnsupportedOperation)
        ));

        // Explicit Enable abandon is fixture-only (not automatic Register recovery).
        let settled = ctx
            .abandon_pending_transition_for_test()
            .expect("explicit enable abandon for fixture");
        assert!(settled.identity().is_some());
        let peer = noise_device_peer();
        let authority = ctx
            .enroll_or_rebind_paired_browser(peer, &metadata("cookie-rev-pend"))
            .expect("enroll");
        let DeviceEnrollmentAuthority::Device { device_id, .. } = authority else {
            panic!("device");
        };

        let (_old_store, vault, binding) = ctx.into_parts_for_test();
        let live_bytes = _old_store
            .persistence()
            .inner
            .snapshot_bytes()
            .expect("live identity bytes");
        let rev_persistence = FailOnceClearMarkerPersistence {
            inner: InMemoryIdentityPersistence::from_bytes(&live_bytes).expect("bytes"),
            marker: "connectPendingRevocation",
            fail_once: Arc::new(AtomicBool::new(true)),
        };
        let mut rev_store = IsolatedRemoteStore::new(rev_persistence).expect("rev store");
        let mut vault = vault;
        let revision = rev_store.load(&binding, &vault).expect("load").revision();
        let revoke_err = rev_store
            .execute(
                &binding,
                &mut vault,
                IdentityCommand {
                    command_id: CommandId::new(),
                    expected_revision: revision,
                    op: IdentityOp::RevokeDevice {
                        device_id,
                        now_epoch_ms: 200,
                    },
                },
            )
            .expect_err("revocation clear fault");
        assert!(matches!(revoke_err, IdentityError::PersistFailed));
        assert!(rev_store
            .pending_revocation_for_test()
            .expect("revocation read")
            .is_some());
        let mut rev_ctx = DeviceEnrollmentContext::from_parts(rev_store, vault, binding);
        let rev_refuse = rev_ctx
            .recover_orphaned_register_device_pending()
            .expect_err("revocation must fail closed");
        assert!(matches!(
            rev_refuse,
            DeviceEnrollmentError::Identity(IdentityError::UnsupportedOperation)
        ));
    }
}
