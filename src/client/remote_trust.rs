//! Native desktop device custody and per-host trust for remote Connect.
//!
//! Explicit profile-rooted store under the process-unique test config root in
//! unit tests. Device static secrets and pairing cookies are DPAPI-wrapped with
//! authenticated metadata in the entropy scope. Host pin + cookie persist only
//! after Noise + Hello. Disk work uses `remote::blocking_work::RemoteBlockingWork`
//! (bounded OS worker + reaper); mutation admission is one-shot under the store
//! lock immediately before durable writes. An admitted write remains owned even
//! if the socket/deadline later fails — that is `PersistenceUncertain`, not
//! proof of cancellation.

use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use zeroize::{Zeroize, Zeroizing};

use crate::client::connect_client::ConnectClientConfig;
use crate::client::host_client::HostClient;
use crate::client::remote_transport::{
    get_bounded_until, hex_encode, open_remote_connect_ws_until, parse_devmanager_connect_meta,
    parse_host_public_id, parse_host_public_key_hex, post_pair_collect_cookie_until,
    validate_additional_ca_pem, validate_http_header_value, validate_remote_endpoint,
    PublishedHostIdentity, RemoteEndpoint, RemoteTlsOptions, RemoteTransportError,
    REMOTE_TRANSPORT_DEFAULT_DEADLINE,
};
use crate::connect::{ConnectNoiseCustody, ConnectNoiseStaticPublicKey};
use crate::domain::id::ClientId;
use crate::providers::settings::secret::{protect_bytes, reveal_bytes, SecretCustodyError};
use crate::remote::blocking_work::{RemoteBlockingWork, RemoteWorkAdmission, RemoteWorkError};

const DEVICE_SCHEMA: &str = "devmanager.remote-native-device/v1";
const HOST_SCHEMA: &str = "devmanager.remote-native-host/v1";
const STORE_DIR_NAME: &str = "remote-native-trust";
const DEVICE_FILE_NAME: &str = "device.json";
const HOSTS_DIR_NAME: &str = "hosts";
const STORE_LOCK_FILE_NAME: &str = "trust-mutex.lock";
const MAX_LABEL_BYTES: usize = 64;
const MAX_PAIRING_CODE_BYTES: usize = 128;
const MAX_STORE_FILE_BYTES: u64 = 256 * 1024;
const MAX_HOST_DIR_ENTRIES: usize = 256;
/// Max remotes in the native Settings roster (fleet self + remotes ≤ 16).
const MAX_TRUSTED_REMOTE_HOSTS: usize = 15;
const DEVICE_PUBLIC_ID_BYTES: usize = 16;
const NOISE_STATIC_KEY_BYTES: usize = 32;
const CUSTODY_SCOPE: &[u8] = b"DevManagerRemoteNativeTrust/v1\0";

/// Clear production errors without secrets, URLs, or cookie material.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum RemoteTrustError {
    Unauthorized,
    Timeout,
    Unavailable,
    Corrupt,
    Unsupported,
    PinChanged,
    Endpoint,
    Custody,
    PairingFailed,
    PersistFailed,
    NotFound,
    Cancelled,
    Busy,
    /// Admitted durable write may still settle; not proof of cancel/unwritten.
    PersistenceUncertain,
}

impl fmt::Debug for RemoteTrustError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl fmt::Display for RemoteTrustError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl std::error::Error for RemoteTrustError {}

impl RemoteTrustError {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Unauthorized => "remote trust unauthorized",
            Self::Timeout => "remote trust deadline exceeded",
            Self::Unavailable => "remote trust unavailable",
            Self::Corrupt => "remote trust custody corrupt",
            Self::Unsupported => "remote trust unsupported on this platform",
            Self::PinChanged => "remote host identity or key pin changed",
            Self::Endpoint => "remote trust endpoint rejected",
            Self::Custody => "remote trust custody unavailable",
            Self::PairingFailed => "remote pairing failed",
            Self::PersistFailed => "remote trust persist failed",
            Self::NotFound => "remote trusted host not found",
            Self::Cancelled => "remote trust operation cancelled",
            Self::Busy => "remote trust store busy",
            Self::PersistenceUncertain => "remote trust persistence uncertain",
        }
    }
}

impl From<RemoteTransportError> for RemoteTrustError {
    fn from(error: RemoteTransportError) -> Self {
        match error {
            RemoteTransportError::Unauthorized => Self::Unauthorized,
            RemoteTransportError::Timeout => Self::Timeout,
            RemoteTransportError::Unavailable => Self::Unavailable,
            RemoteTransportError::Corrupt => Self::Corrupt,
            RemoteTransportError::Unsupported => Self::Unsupported,
            RemoteTransportError::RedirectForbidden => Self::PairingFailed,
            RemoteTransportError::Oversized => Self::Corrupt,
            RemoteTransportError::Endpoint => Self::Endpoint,
            RemoteTransportError::Tls => Self::Unauthorized,
            RemoteTransportError::Header => Self::Unauthorized,
            RemoteTransportError::Cancelled => Self::Cancelled,
        }
    }
}

impl From<SecretCustodyError> for RemoteTrustError {
    fn from(error: SecretCustodyError) -> Self {
        match error {
            SecretCustodyError::Unsupported => Self::Unsupported,
            SecretCustodyError::ProtectFailed
            | SecretCustodyError::UnprotectFailed
            | SecretCustodyError::TooLarge
            | SecretCustodyError::Empty => Self::Custody,
        }
    }
}

/// Explicit profile-rooted native remote trust store.
pub struct RemoteTrustStore {
    root: PathBuf,
}

impl fmt::Debug for RemoteTrustStore {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RemoteTrustStore")
            .field("root", &self.root)
            .finish()
    }
}

/// Public device claim used as `browserInstallId` / Hello device binding.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct RemoteDevicePublicId(pub [u8; DEVICE_PUBLIC_ID_BYTES]);

impl fmt::Debug for RemoteDevicePublicId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RemoteDevicePublicId")
            .field("hex", &hex_encode(&self.0))
            .finish()
    }
}

impl RemoteDevicePublicId {
    pub fn as_bytes(&self) -> &[u8; DEVICE_PUBLIC_ID_BYTES] {
        &self.0
    }

    pub fn to_browser_install_id(self) -> String {
        hex_encode(&self.0)
    }
}

/// Loaded device custody. Private key material is never Debug-printed.
pub struct RemoteDeviceCustody {
    pub device_public_id: RemoteDevicePublicId,
    custody: ConnectNoiseCustody,
}

impl fmt::Debug for RemoteDeviceCustody {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RemoteDeviceCustody")
            .field("device_public_id", &self.device_public_id)
            .field("public_key", &hex_encode(&self.custody.public().as_bytes()))
            .finish()
    }
}

impl RemoteDeviceCustody {
    pub fn noise_custody(&self) -> &ConnectNoiseCustody {
        &self.custody
    }

    pub fn public_key(&self) -> ConnectNoiseStaticPublicKey {
        self.custody.public()
    }
}

/// Public view of a persisted trusted host (no cookie plaintext).
#[derive(Clone, PartialEq, Eq)]
pub struct TrustedHostRecord {
    pub host_public_id: [u8; 16],
    pub host_key_pin: ConnectNoiseStaticPublicKey,
    pub endpoint: String,
    pub connect_path: String,
    pub assigned_client_id: ClientId,
    /// Optional LAN/custom CA PEM captured at successful enroll. Debug redacts body.
    pub additional_ca_pem: Option<String>,
}

impl fmt::Debug for TrustedHostRecord {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TrustedHostRecord")
            .field("host_public_id", &hex_encode(&self.host_public_id))
            .field("host_key_pin_len", &32usize)
            .field(
                "endpoint_scheme_host",
                &redact_endpoint_for_debug(&self.endpoint),
            )
            .field("connect_path", &self.connect_path)
            .field("assigned_client_id", &self.assigned_client_id)
            .field(
                "additional_ca_pem_bytes",
                &self.additional_ca_pem.as_ref().map(String::len),
            )
            .finish()
    }
}

fn redact_endpoint_for_debug(endpoint: &str) -> String {
    url::Url::parse(endpoint)
        .ok()
        .map(|parsed| {
            format!(
                "{}://{}",
                parsed.scheme(),
                parsed.host_str().unwrap_or("invalid")
            )
        })
        .unwrap_or_else(|| "invalid".to_string())
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct DeviceFile {
    schema: String,
    device_public_id: String,
    public_key: String,
    private_key_protected: String,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct HostFile {
    schema: String,
    host_public_id: String,
    host_public_key: String,
    endpoint: String,
    connect_path: String,
    assigned_client_id: String,
    pairing_cookie_protected: String,
    /// Absent on legacy records; must not alter cookie entropy when None.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    additional_ca_pem: Option<String>,
}

/// Explicit enrollment request. Pairing code is zeroized after use; no Clone.
pub struct PairEnrollRequest {
    pub endpoint: String,
    pub pairing_code: Zeroizing<String>,
    pub label: Option<String>,
    pub additional_ca_pem: Option<String>,
    pub deadline: Duration,
}

impl fmt::Debug for PairEnrollRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PairEnrollRequest")
            .field("endpoint", &redact_endpoint_for_debug(&self.endpoint))
            .field("pairing_code_present", &!self.pairing_code.is_empty())
            .field("label_present", &self.label.is_some())
            .field(
                "additional_ca_pem_bytes",
                &self.additional_ca_pem.as_ref().map(String::len),
            )
            .field("deadline_ms", &self.deadline.as_millis())
            .finish()
    }
}

impl Default for PairEnrollRequest {
    fn default() -> Self {
        Self {
            endpoint: String::new(),
            pairing_code: Zeroizing::new(String::new()),
            label: None,
            additional_ca_pem: None,
            deadline: REMOTE_TRANSPORT_DEFAULT_DEADLINE,
        }
    }
}

/// Options for reconnecting an already-trusted host.
#[derive(Clone)]
pub struct ConnectTrustedOptions {
    pub additional_ca_pem: Option<String>,
    pub deadline: Duration,
}

impl Default for ConnectTrustedOptions {
    fn default() -> Self {
        Self {
            additional_ca_pem: None,
            deadline: REMOTE_TRANSPORT_DEFAULT_DEADLINE,
        }
    }
}

impl fmt::Debug for ConnectTrustedOptions {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ConnectTrustedOptions")
            .field(
                "additional_ca_pem_bytes",
                &self.additional_ca_pem.as_ref().map(String::len),
            )
            .field("deadline_ms", &self.deadline.as_millis())
            .finish()
    }
}

/// Exclusive path-bound store lock. Drop releases; never hold across await.
struct StoreLock {
    _file: File,
}

impl RemoteTrustStore {
    /// Open an explicit absolute profile root. Creates the trust subdirectory.
    pub fn open(explicit_root: PathBuf) -> Result<Self, RemoteTrustError> {
        if !explicit_root.is_absolute() || explicit_root.as_os_str().is_empty() {
            return Err(RemoteTrustError::Corrupt);
        }
        #[cfg(test)]
        {
            let config =
                crate::persistence::app_config_dir().map_err(|_| RemoteTrustError::Corrupt)?;
            if !explicit_root.starts_with(&config) {
                return Err(RemoteTrustError::Corrupt);
            }
        }
        validate_path_no_reparse(&explicit_root)?;
        let root = explicit_root.join(STORE_DIR_NAME);
        validate_existing_ancestors_no_reparse(&root)?;
        fs::create_dir_all(&root).map_err(|_| RemoteTrustError::PersistFailed)?;
        fs::create_dir_all(root.join(HOSTS_DIR_NAME))
            .map_err(|_| RemoteTrustError::PersistFailed)?;
        validate_path_no_reparse(&root)?;
        Ok(Self { root })
    }

    /// Open under the active profile config directory (process-unique in tests).
    pub fn open_under_app_config() -> Result<Self, RemoteTrustError> {
        let config = crate::persistence::app_config_dir().map_err(|_| RemoteTrustError::Custody)?;
        Self::open(config)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn acquire_lock(&self) -> Result<StoreLock, RemoteTrustError> {
        #[cfg(not(windows))]
        {
            let _ = self;
            // Desktop production is Windows; do not pretend flock is exclusive.
            return Err(RemoteTrustError::Unsupported);
        }
        #[cfg(windows)]
        {
            self.revalidate_store_layout()?;
            let lock_path = self.root.join(STORE_LOCK_FILE_NAME);
            let mut options = OpenOptions::new();
            options.read(true).write(true).create(true);
            use std::os::windows::fs::OpenOptionsExt;
            use windows::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT;
            options
                .share_mode(0)
                .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT.0);
            let file = options.open(&lock_path).map_err(|err| {
                if err.kind() == std::io::ErrorKind::PermissionDenied
                    || err.raw_os_error() == Some(32)
                    || err.raw_os_error() == Some(33)
                {
                    RemoteTrustError::Busy
                } else {
                    RemoteTrustError::PersistFailed
                }
            })?;
            let meta = file
                .metadata()
                .map_err(|_| RemoteTrustError::PersistFailed)?;
            if !meta.is_file() || metadata_is_reparse(&meta) {
                return Err(RemoteTrustError::Corrupt);
            }
            Ok(StoreLock { _file: file })
        }
    }

    fn revalidate_store_layout(&self) -> Result<(), RemoteTrustError> {
        validate_existing_ancestors_no_reparse(&self.root)?;
        validate_path_no_reparse(&self.root)?;
        let hosts = self.root.join(HOSTS_DIR_NAME);
        validate_existing_ancestors_no_reparse(&hosts)?;
        match fs::symlink_metadata(&hosts) {
            Ok(meta) => {
                if !meta.is_dir() || metadata_is_reparse(&meta) {
                    return Err(RemoteTrustError::Corrupt);
                }
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return Err(RemoteTrustError::PersistFailed),
        }
        Ok(())
    }

    /// Blocking load-or-create. Call only from `RemoteBlockingWork` with admission.
    pub(crate) fn load_or_create_device_blocking(
        &self,
        deadline_at: Instant,
        admission: &RemoteWorkAdmission,
    ) -> Result<RemoteDeviceCustody, RemoteTrustError> {
        loop {
            remaining_until(deadline_at)?;
            match self.acquire_lock() {
                Ok(_lock) => {
                    remaining_until(deadline_at)?;
                    if !admission.try_admit() {
                        return Err(RemoteTrustError::Cancelled);
                    }
                    return match self.load_device_unlocked() {
                        Ok(device) => Ok(device),
                        Err(RemoteTrustError::NotFound) => {
                            remaining_until(deadline_at)?;
                            self.create_device_unlocked(deadline_at)
                        }
                        Err(error) => Err(error),
                    };
                }
                Err(RemoteTrustError::Busy) => {
                    remaining_until(deadline_at)?;
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(error) => return Err(error),
            }
        }
    }

    /// Load device custody or mint once (blocking via owned remote worker).
    pub fn load_or_create_device(&self) -> Result<RemoteDeviceCustody, RemoteTrustError> {
        let deadline_at = Instant::now() + REMOTE_TRANSPORT_DEFAULT_DEADLINE;
        let root = self.root.clone();
        run_remote_blocking_sync(
            "remote-trust-device-load-or-create",
            deadline_at,
            BlockingWorkKind::Mutation,
            move |admission| {
                RemoteTrustStore { root }.load_or_create_device_blocking(deadline_at, &admission)
            },
        )
    }

    pub fn load_device(&self) -> Result<RemoteDeviceCustody, RemoteTrustError> {
        let _lock = self.acquire_lock()?;
        self.load_device_unlocked()
    }

    fn load_device_unlocked(&self) -> Result<RemoteDeviceCustody, RemoteTrustError> {
        self.revalidate_store_layout()?;
        let path = self.device_path();
        let bytes = read_bounded_file_nofollow(&path)?;
        crate::remote::verify_remote_state_file_permissions(&path)
            .map_err(|_| RemoteTrustError::PersistFailed)?;
        let file: DeviceFile =
            serde_json::from_slice(&bytes).map_err(|_| RemoteTrustError::Corrupt)?;
        if file.schema != DEVICE_SCHEMA {
            return Err(RemoteTrustError::Corrupt);
        }
        let device_public_id = parse_device_public_id(&file.device_public_id)?;
        let public_bytes =
            parse_host_public_key_hex(&file.public_key).map_err(|_| RemoteTrustError::Corrupt)?;
        let public = ConnectNoiseStaticPublicKey::from_bytes(public_bytes)
            .map_err(|_| RemoteTrustError::Corrupt)?;
        let scope = device_custody_scope(
            self.root.as_path(),
            &file.schema,
            &file.device_public_id,
            &file.public_key,
        );
        let mut plain = reveal_bytes(&file.private_key_protected, &scope)?;
        if plain.len() != NOISE_STATIC_KEY_BYTES {
            plain.zeroize();
            return Err(RemoteTrustError::Corrupt);
        }
        let mut key_bytes = [0_u8; NOISE_STATIC_KEY_BYTES];
        key_bytes.copy_from_slice(&plain);
        plain.zeroize();
        let private = crate::protocol::NoiseStaticPrivateKey::from_vault_bytes(key_bytes)
            .map_err(|_| RemoteTrustError::Corrupt)?;
        key_bytes.fill(0);
        let custody = ConnectNoiseCustody::from_vault(private, public)
            .map_err(|_| RemoteTrustError::Corrupt)?;
        Ok(RemoteDeviceCustody {
            device_public_id: RemoteDevicePublicId(device_public_id),
            custody,
        })
    }

    fn create_device_unlocked(
        &self,
        deadline_at: Instant,
    ) -> Result<RemoteDeviceCustody, RemoteTrustError> {
        remaining_until(deadline_at)?;
        // Under exclusive lock: retry load in case a prior winner landed.
        match self.load_device_unlocked() {
            Ok(device) => return Ok(device),
            Err(RemoteTrustError::NotFound) => {}
            Err(error) => return Err(error),
        }
        remaining_until(deadline_at)?;
        let custody = ConnectNoiseCustody::generate().map_err(|_| RemoteTrustError::Custody)?;
        let mut device_public_id = [0_u8; DEVICE_PUBLIC_ID_BYTES];
        loop {
            getrandom::fill(&mut device_public_id).map_err(|_| RemoteTrustError::Custody)?;
            if device_public_id != [0_u8; DEVICE_PUBLIC_ID_BYTES] {
                break;
            }
        }
        let device_public_id_hex = hex_encode(&device_public_id);
        let public_key_hex = hex_encode(&custody.public().as_bytes());
        let mut private = Zeroizing::new(*custody.private().as_bytes());
        let scope = device_custody_scope(
            self.root.as_path(),
            DEVICE_SCHEMA,
            &device_public_id_hex,
            &public_key_hex,
        );
        let protected = protect_bytes(private.as_slice(), &scope)?;
        private.zeroize();
        let file = DeviceFile {
            schema: DEVICE_SCHEMA.to_string(),
            device_public_id: device_public_id_hex,
            public_key: public_key_hex,
            private_key_protected: protected,
        };
        let bytes = serde_json::to_vec(&file).map_err(|_| RemoteTrustError::PersistFailed)?;
        remaining_until(deadline_at)?;
        write_store_file_atomic(&self.device_path(), &bytes)?;
        let loaded = self.load_device_unlocked()?;
        if loaded.device_public_id.0 != device_public_id {
            return Err(RemoteTrustError::Corrupt);
        }
        Ok(loaded)
    }

    pub fn load_trusted_host(
        &self,
        host_public_id: [u8; 16],
    ) -> Result<(TrustedHostRecord, Zeroizing<String>), RemoteTrustError> {
        let _lock = self.acquire_lock()?;
        self.load_trusted_host_unlocked(host_public_id)
    }

    fn load_trusted_host_unlocked(
        &self,
        host_public_id: [u8; 16],
    ) -> Result<(TrustedHostRecord, Zeroizing<String>), RemoteTrustError> {
        self.revalidate_store_layout()?;
        let path = self.host_path(host_public_id);
        let bytes = read_bounded_file_nofollow(&path)?;
        crate::remote::verify_remote_state_file_permissions(&path)
            .map_err(|_| RemoteTrustError::PersistFailed)?;
        let file: HostFile =
            serde_json::from_slice(&bytes).map_err(|_| RemoteTrustError::Corrupt)?;
        if file.schema != HOST_SCHEMA {
            return Err(RemoteTrustError::Corrupt);
        }
        let stored_id =
            parse_host_public_id(&file.host_public_id).map_err(|_| RemoteTrustError::Corrupt)?;
        if stored_id != host_public_id {
            return Err(RemoteTrustError::Corrupt);
        }
        let pin_bytes = parse_host_public_key_hex(&file.host_public_key)
            .map_err(|_| RemoteTrustError::Corrupt)?;
        let host_key_pin = ConnectNoiseStaticPublicKey::from_bytes(pin_bytes)
            .map_err(|_| RemoteTrustError::Corrupt)?;
        let assigned_client_id =
            ClientId::parse(&file.assigned_client_id).map_err(|_| RemoteTrustError::Corrupt)?;
        validate_remote_endpoint(&file.endpoint).map_err(|_| RemoteTrustError::Corrupt)?;
        validate_remote_endpoint(&format!(
            "{}{}",
            if file.endpoint.starts_with("https://") {
                file.endpoint.replacen("https://", "wss://", 1)
            } else {
                file.endpoint.replacen("http://", "ws://", 1)
            },
            file.connect_path
        ))
        .map_err(|_| RemoteTrustError::Corrupt)?;
        if let Some(pem) = file.additional_ca_pem.as_deref() {
            validate_additional_ca_pem(pem).map_err(|_| RemoteTrustError::Corrupt)?;
        }
        let scope = host_cookie_custody_scope(
            self.root.as_path(),
            &file.schema,
            &file.host_public_id,
            &file.host_public_key,
            &file.endpoint,
            &file.connect_path,
            &file.assigned_client_id,
            file.additional_ca_pem.as_deref(),
        );
        let cookie = reveal_bytes(&file.pairing_cookie_protected, &scope)?;
        let cookie_text = std::str::from_utf8(&cookie).map_err(|_| RemoteTrustError::Corrupt)?;
        let cookie = Zeroizing::new(cookie_text.to_string());
        Ok((
            TrustedHostRecord {
                host_public_id,
                host_key_pin,
                endpoint: file.endpoint,
                connect_path: file.connect_path,
                assigned_client_id,
                additional_ca_pem: file.additional_ca_pem,
            },
            cookie,
        ))
    }

    pub fn list_trusted_host_ids(&self) -> Result<Vec<[u8; 16]>, RemoteTrustError> {
        let _lock = self.acquire_lock()?;
        self.list_trusted_host_ids_unlocked()
    }

    fn list_trusted_host_ids_unlocked(&self) -> Result<Vec<[u8; 16]>, RemoteTrustError> {
        self.revalidate_store_layout()?;
        let dir = self.root.join(HOSTS_DIR_NAME);
        let mut ids = Vec::new();
        let mut scanned = 0usize;
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(_) => return Err(RemoteTrustError::PersistFailed),
        };
        for entry in entries {
            let entry = entry.map_err(|_| RemoteTrustError::PersistFailed)?;
            scanned = scanned.saturating_add(1);
            if scanned > MAX_HOST_DIR_ENTRIES {
                return Err(RemoteTrustError::Corrupt);
            }
            let name = entry
                .file_name()
                .into_string()
                .map_err(|_| RemoteTrustError::Corrupt)?;
            let Some(stem) = name.strip_suffix(".json") else {
                return Err(RemoteTrustError::Corrupt);
            };
            let id = decode_hex16(stem)?;
            ids.push(id);
        }
        ids.sort_unstable();
        Ok(ids)
    }

    fn find_trusted_by_endpoint_base_unlocked(
        &self,
        http_base: &str,
    ) -> Result<Option<TrustedHostRecord>, RemoteTrustError> {
        for id in self.list_trusted_host_ids_unlocked()? {
            match self.load_trusted_host_unlocked(id) {
                Ok((record, _)) => {
                    if record.endpoint == http_base {
                        return Ok(Some(record));
                    }
                }
                Err(RemoteTrustError::NotFound) => continue,
                Err(error) => return Err(error),
            }
        }
        Ok(None)
    }

    fn find_trusted_by_endpoint_unlocked(
        &self,
        endpoint: &RemoteEndpoint,
    ) -> Result<Option<TrustedHostRecord>, RemoteTrustError> {
        self.find_trusted_by_endpoint_base_unlocked(endpoint.http_base())
    }

    /// Persist host pin/cookie/assigned id under lock after Hello. Rejects
    /// concurrent pin/identity/endpoint mapping changes without overwriting.
    ///
    /// Admission point: `try_admit` runs under the store lock immediately before
    /// the atomic host-record write. Prep/loads before that point may be
    /// cancelled without publishing. After admit, the durable write is owned;
    /// a later deadline reports `PersistenceUncertain`, not unwritten/cancelled.
    fn persist_trusted_host_transactional(
        &self,
        record: &TrustedHostRecord,
        pairing_cookie: &str,
        deadline_at: Instant,
        admission: &RemoteWorkAdmission,
    ) -> Result<(), RemoteTrustError> {
        remaining_until(deadline_at)?;
        validate_trusted_host_record(record)?;
        validate_http_header_value(pairing_cookie).map_err(|_| RemoteTrustError::Unauthorized)?;
        let _lock = loop {
            remaining_until(deadline_at)?;
            match self.acquire_lock() {
                Ok(lock) => break lock,
                Err(RemoteTrustError::Busy) => std::thread::sleep(Duration::from_millis(5)),
                Err(error) => return Err(error),
            }
        };
        remaining_until(deadline_at)?;
        // Same-endpoint mapping under this lock: two different host IDs cannot both commit.
        if let Some(by_endpoint) = self.find_trusted_by_endpoint_base_unlocked(&record.endpoint)? {
            if by_endpoint.host_public_id != record.host_public_id {
                return Err(RemoteTrustError::PinChanged);
            }
            if by_endpoint.host_key_pin != record.host_key_pin {
                return Err(RemoteTrustError::PinChanged);
            }
            if by_endpoint.additional_ca_pem != record.additional_ca_pem {
                return Err(RemoteTrustError::PinChanged);
            }
        }
        match self.load_trusted_host_unlocked(record.host_public_id) {
            Ok((existing, _)) => {
                if existing.host_key_pin != record.host_key_pin
                    || existing.host_public_id != record.host_public_id
                {
                    return Err(RemoteTrustError::PinChanged);
                }
                if existing.endpoint != record.endpoint
                    || existing.connect_path != record.connect_path
                {
                    return Err(RemoteTrustError::PinChanged);
                }
                if existing.additional_ca_pem != record.additional_ca_pem {
                    return Err(RemoteTrustError::PinChanged);
                }
            }
            Err(RemoteTrustError::NotFound) => {}
            Err(error) => return Err(error),
        }
        remaining_until(deadline_at)?;
        let host_public_id = uuid::Uuid::from_bytes(record.host_public_id).to_string();
        let host_public_key = hex_encode(&record.host_key_pin.as_bytes());
        let assigned_client_id = record.assigned_client_id.to_string();
        let scope = host_cookie_custody_scope(
            self.root.as_path(),
            HOST_SCHEMA,
            &host_public_id,
            &host_public_key,
            &record.endpoint,
            &record.connect_path,
            &assigned_client_id,
            record.additional_ca_pem.as_deref(),
        );
        let protected = protect_bytes(pairing_cookie.as_bytes(), &scope)?;
        let file = HostFile {
            schema: HOST_SCHEMA.to_string(),
            host_public_id,
            host_public_key,
            endpoint: record.endpoint.clone(),
            connect_path: record.connect_path.clone(),
            assigned_client_id,
            pairing_cookie_protected: protected,
            additional_ca_pem: record.additional_ca_pem.clone(),
        };
        let bytes = serde_json::to_vec(&file).map_err(|_| RemoteTrustError::PersistFailed)?;
        remaining_until(deadline_at)?;
        // Mutation admission immediately before publish; cancel-before-admit leaves no host record.
        if !admission.try_admit() {
            return Err(RemoteTrustError::Cancelled);
        }
        #[cfg(test)]
        persist_after_admit_seam();
        write_store_file_atomic(&self.host_path(record.host_public_id), &bytes)?;
        let _ = self.load_trusted_host_unlocked(record.host_public_id)?;
        Ok(())
    }

    fn device_path(&self) -> PathBuf {
        self.root.join(DEVICE_FILE_NAME)
    }

    fn host_path(&self, host_public_id: [u8; 16]) -> PathBuf {
        self.root
            .join(HOSTS_DIR_NAME)
            .join(format!("{}.json", hex_encode(&host_public_id)))
    }

    /// Read-only roster load under the fail-fast store lock. No `try_admit`.
    fn list_trusted_hosts_blocking(
        &self,
        deadline_at: Instant,
        admission: &RemoteWorkAdmission,
    ) -> Result<Vec<TrustedHostRecord>, RemoteTrustError> {
        if admission.cancellation_requested() {
            return Err(RemoteTrustError::Cancelled);
        }
        remaining_until(deadline_at)?;
        // Fail-fast share_mode(0): Busy is typed, not a lock wait loop.
        let _lock = self.acquire_lock()?;
        if admission.cancellation_requested() {
            return Err(RemoteTrustError::Cancelled);
        }
        remaining_until(deadline_at)?;
        let ids = self.list_trusted_host_ids_unlocked()?;
        if ids.len() > MAX_TRUSTED_REMOTE_HOSTS {
            return Err(RemoteTrustError::Corrupt);
        }
        let mut records = Vec::with_capacity(ids.len());
        for id in ids {
            if admission.cancellation_requested() {
                return Err(RemoteTrustError::Cancelled);
            }
            remaining_until(deadline_at)?;
            let (record, mut cookie) = self.load_trusted_host_unlocked(id)?;
            cookie.zeroize();
            drop(cookie);
            records.push(record);
        }
        records.sort_by(|left, right| left.host_public_id.cmp(&right.host_public_id));
        Ok(records)
    }

    /// Exact-record forget under the store lock. `try_admit` immediately before remove.
    fn forget_trusted_host_blocking(
        &self,
        expected: &TrustedHostRecord,
        deadline_at: Instant,
        admission: &RemoteWorkAdmission,
    ) -> Result<(), RemoteTrustError> {
        remaining_until(deadline_at)?;
        validate_trusted_host_record(expected)?;
        let _lock = self.acquire_lock()?;
        remaining_until(deadline_at)?;
        self.revalidate_store_layout()?;
        let path = self.host_path(expected.host_public_id);
        validate_existing_ancestors_no_reparse(&path)?;
        validate_path_no_reparse(&path)?;
        match self.load_trusted_host_unlocked(expected.host_public_id) {
            Ok((loaded, mut cookie)) => {
                cookie.zeroize();
                drop(cookie);
                if &loaded != expected {
                    return Err(RemoteTrustError::PinChanged);
                }
            }
            Err(RemoteTrustError::NotFound) => {
                // Idempotent: nothing to admit or delete.
                return Ok(());
            }
            Err(error) => return Err(error),
        }
        remaining_until(deadline_at)?;
        if !admission.try_admit() {
            return Err(RemoteTrustError::Cancelled);
        }
        #[cfg(test)]
        persist_after_admit_seam();
        remove_host_file_nofollow(&path)?;
        Ok(())
    }
}

fn device_custody_scope(
    root: &Path,
    schema: &str,
    device_public_id_hex: &str,
    public_key_hex: &str,
) -> Vec<u8> {
    let mut scope = Vec::with_capacity(160);
    scope.extend_from_slice(CUSTODY_SCOPE);
    scope.extend_from_slice(root.to_string_lossy().as_bytes());
    scope.push(0);
    scope.extend_from_slice(schema.as_bytes());
    scope.push(0);
    scope.extend_from_slice(device_public_id_hex.as_bytes());
    scope.push(0);
    scope.extend_from_slice(public_key_hex.as_bytes());
    scope.extend_from_slice(b"\0device-static");
    scope
}

fn host_cookie_custody_scope(
    root: &Path,
    schema: &str,
    host_public_id: &str,
    host_public_key: &str,
    endpoint: &str,
    connect_path: &str,
    assigned_client_id: &str,
    additional_ca_pem: Option<&str>,
) -> Vec<u8> {
    let mut scope = Vec::with_capacity(256);
    scope.extend_from_slice(CUSTODY_SCOPE);
    scope.extend_from_slice(root.to_string_lossy().as_bytes());
    scope.push(0);
    scope.extend_from_slice(schema.as_bytes());
    scope.push(0);
    scope.extend_from_slice(host_public_id.as_bytes());
    scope.push(0);
    scope.extend_from_slice(host_public_key.as_bytes());
    scope.push(0);
    scope.extend_from_slice(endpoint.as_bytes());
    scope.push(0);
    scope.extend_from_slice(connect_path.as_bytes());
    scope.push(0);
    scope.extend_from_slice(assigned_client_id.as_bytes());
    // Legacy records with no CA keep the historical entropy suffix exactly.
    if let Some(pem) = additional_ca_pem {
        scope.push(0);
        scope.extend_from_slice(b"additional-ca-pem\0");
        scope.extend_from_slice(pem.as_bytes());
    }
    scope.extend_from_slice(b"\0host-cookie");
    scope
}

fn parse_device_public_id(raw: &str) -> Result<[u8; 16], RemoteTrustError> {
    decode_hex16(raw)
}

fn decode_hex16(raw: &str) -> Result<[u8; 16], RemoteTrustError> {
    if raw.len() != 32 {
        return Err(RemoteTrustError::Corrupt);
    }
    let mut out = [0_u8; 16];
    for (index, chunk) in raw.as_bytes().chunks(2).enumerate() {
        let hi = hex_nibble(chunk[0])?;
        let lo = hex_nibble(chunk[1])?;
        out[index] = (hi << 4) | lo;
    }
    if out == [0_u8; 16] {
        return Err(RemoteTrustError::Corrupt);
    }
    Ok(out)
}

fn hex_nibble(byte: u8) -> Result<u8, RemoteTrustError> {
    match byte {
        b'0'..=b'9' => Ok(byte - b'0'),
        b'a'..=b'f' => Ok(byte - b'a' + 10),
        b'A'..=b'F' => Ok(byte - b'A' + 10),
        _ => Err(RemoteTrustError::Corrupt),
    }
}

fn metadata_is_reparse(meta: &fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
        meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
    }
    #[cfg(not(windows))]
    {
        meta.file_type().is_symlink()
    }
}

fn validate_path_no_reparse(path: &Path) -> Result<(), RemoteTrustError> {
    if !path.is_absolute() {
        return Err(RemoteTrustError::Corrupt);
    }
    match fs::symlink_metadata(path) {
        Ok(meta) => {
            if metadata_is_reparse(&meta) {
                return Err(RemoteTrustError::Corrupt);
            }
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(_) => return Err(RemoteTrustError::PersistFailed),
    }
    Ok(())
}

fn validate_existing_ancestors_no_reparse(path: &Path) -> Result<(), RemoteTrustError> {
    if !path.is_absolute() {
        return Err(RemoteTrustError::Corrupt);
    }
    let mut ancestors = path
        .ancestors()
        .filter(|ancestor| !ancestor.as_os_str().is_empty())
        .collect::<Vec<_>>();
    ancestors.reverse();
    for ancestor in ancestors {
        match fs::symlink_metadata(ancestor) {
            Ok(meta) => {
                if metadata_is_reparse(&meta) {
                    return Err(RemoteTrustError::Corrupt);
                }
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => continue,
            Err(_) => return Err(RemoteTrustError::PersistFailed),
        }
    }
    Ok(())
}

fn read_bounded_file_nofollow(path: &Path) -> Result<Vec<u8>, RemoteTrustError> {
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
    let mut file = options.open(path).map_err(|err| {
        if err.kind() == std::io::ErrorKind::NotFound {
            RemoteTrustError::NotFound
        } else {
            RemoteTrustError::PersistFailed
        }
    })?;
    let meta = file
        .metadata()
        .map_err(|_| RemoteTrustError::PersistFailed)?;
    if !meta.is_file() || metadata_is_reparse(&meta) || meta.len() > MAX_STORE_FILE_BYTES {
        return Err(RemoteTrustError::Corrupt);
    }
    let mut bytes = Vec::new();
    file.by_ref()
        .take(MAX_STORE_FILE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| RemoteTrustError::PersistFailed)?;
    if bytes.len() as u64 > MAX_STORE_FILE_BYTES {
        return Err(RemoteTrustError::Corrupt);
    }
    Ok(bytes)
}

/// Single-file host removal after layout/reparse checks. Never recursive.
fn remove_host_file_nofollow(path: &Path) -> Result<(), RemoteTrustError> {
    validate_existing_ancestors_no_reparse(path)?;
    validate_path_no_reparse(path)?;
    let meta = fs::symlink_metadata(path).map_err(|err| {
        if err.kind() == std::io::ErrorKind::NotFound {
            RemoteTrustError::NotFound
        } else {
            RemoteTrustError::PersistFailed
        }
    })?;
    if !meta.is_file() || metadata_is_reparse(&meta) {
        return Err(RemoteTrustError::Corrupt);
    }
    crate::remote::verify_remote_state_file_permissions(path)
        .map_err(|_| RemoteTrustError::PersistFailed)?;
    fs::remove_file(path).map_err(|err| {
        if err.kind() == std::io::ErrorKind::NotFound {
            RemoteTrustError::NotFound
        } else {
            RemoteTrustError::PersistFailed
        }
    })?;
    Ok(())
}

fn write_store_file_atomic(path: &Path, bytes: &[u8]) -> Result<(), RemoteTrustError> {
    if bytes.len() as u64 > MAX_STORE_FILE_BYTES {
        return Err(RemoteTrustError::PersistFailed);
    }
    crate::remote::atomic_write_remote_state_bytes(path, bytes)
        .map_err(|_| RemoteTrustError::PersistFailed)
}

fn validate_trusted_host_record(record: &TrustedHostRecord) -> Result<(), RemoteTrustError> {
    if record.host_public_id == [0_u8; 16] {
        return Err(RemoteTrustError::Corrupt);
    }
    if record.host_key_pin.as_bytes() == [0_u8; 32] {
        return Err(RemoteTrustError::Corrupt);
    }
    validate_remote_endpoint(&record.endpoint).map_err(|_| RemoteTrustError::Corrupt)?;
    let ws_base = if record.endpoint.starts_with("https://") {
        record.endpoint.replacen("https://", "wss://", 1)
    } else {
        record.endpoint.replacen("http://", "ws://", 1)
    };
    validate_remote_endpoint(&format!("{}{}", ws_base, record.connect_path))
        .map_err(|_| RemoteTrustError::Corrupt)?;
    if let Some(pem) = record.additional_ca_pem.as_deref() {
        validate_additional_ca_pem(pem)?;
    }
    Ok(())
}

fn remaining_until(deadline_at: Instant) -> Result<Duration, RemoteTrustError> {
    let remaining = deadline_at.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        Err(RemoteTrustError::Timeout)
    } else {
        Ok(remaining)
    }
}

#[derive(Clone, Copy)]
enum BlockingWorkKind {
    /// Deadline after settle is timeout only (no durable mutation uncertainty).
    Read,
    /// Deadline after mutation admit is PersistenceUncertain; write remains owned.
    Mutation,
}

fn map_remote_work_error(error: RemoteWorkError, kind: BlockingWorkKind) -> RemoteTrustError {
    match error {
        RemoteWorkError::Unavailable => RemoteTrustError::Unavailable,
        RemoteWorkError::Deadline { admitted: false } => RemoteTrustError::Timeout,
        RemoteWorkError::Deadline { admitted: true } => match kind {
            BlockingWorkKind::Read => RemoteTrustError::Timeout,
            BlockingWorkKind::Mutation => RemoteTrustError::PersistenceUncertain,
        },
    }
}

/// Owned remote OS-worker rendezvous. Never uses `spawn_blocking`.
async fn run_remote_blocking_until<T, F>(
    name: &'static str,
    deadline_at: Instant,
    kind: BlockingWorkKind,
    work: F,
) -> Result<T, RemoteTrustError>
where
    T: Send + 'static,
    F: FnOnce(RemoteWorkAdmission) -> Result<T, RemoteTrustError> + Send + 'static,
{
    remaining_until(deadline_at)?;
    let mut job = RemoteBlockingWork::spawn(name, deadline_at, work)
        .map_err(|_| RemoteTrustError::Unavailable)?;
    match job.wait().await {
        Ok(inner) => inner,
        Err(error) => Err(map_remote_work_error(error, kind)),
    }
}

fn run_remote_blocking_sync<T, F>(
    name: &'static str,
    deadline_at: Instant,
    kind: BlockingWorkKind,
    work: F,
) -> Result<T, RemoteTrustError>
where
    T: Send + 'static,
    F: FnOnce(RemoteWorkAdmission) -> Result<T, RemoteTrustError> + Send + 'static,
{
    remaining_until(deadline_at)?;
    let mut job = RemoteBlockingWork::spawn(name, deadline_at, work)
        .map_err(|_| RemoteTrustError::Unavailable)?;
    match job.wait_blocking() {
        Ok(inner) => inner,
        Err(error) => Err(map_remote_work_error(error, kind)),
    }
}

/// Discover published host identity via bounded GET `/` meta (explicit enroll only).
pub async fn fetch_published_host_identity_until(
    endpoint: &RemoteEndpoint,
    tls: &RemoteTlsOptions,
    deadline_at: Instant,
) -> Result<PublishedHostIdentity, RemoteTrustError> {
    let response = get_bounded_until(endpoint, "/", tls, deadline_at).await?;
    if !response.status.is_success() {
        return Err(RemoteTrustError::Unavailable);
    }
    let text = std::str::from_utf8(&response.body).map_err(|_| RemoteTrustError::Corrupt)?;
    Ok(parse_devmanager_connect_meta(text)?)
}

pub async fn fetch_published_host_identity(
    endpoint: &RemoteEndpoint,
    tls: &RemoteTlsOptions,
    deadline: Duration,
) -> Result<PublishedHostIdentity, RemoteTrustError> {
    fetch_published_host_identity_until(endpoint, tls, Instant::now() + deadline).await
}

fn load_prior_for_enroll(
    store: &RemoteTrustStore,
    endpoint: &RemoteEndpoint,
    published: &PublishedHostIdentity,
    deadline_at: Instant,
    admission: &RemoteWorkAdmission,
) -> Result<Option<TrustedHostRecord>, RemoteTrustError> {
    remaining_until(deadline_at)?;
    let _lock = store.acquire_lock()?;
    remaining_until(deadline_at)?;
    if !admission.try_admit() {
        return Err(RemoteTrustError::Cancelled);
    }
    if let Some(by_endpoint) = store.find_trusted_by_endpoint_unlocked(endpoint)? {
        if by_endpoint.host_public_id != published.host_public_id {
            return Err(RemoteTrustError::PinChanged);
        }
        if by_endpoint.host_key_pin.as_bytes() != published.host_public_key {
            return Err(RemoteTrustError::PinChanged);
        }
        return Ok(Some(by_endpoint));
    }
    match store.load_trusted_host_unlocked(published.host_public_id) {
        Ok((record, _)) => {
            if record.host_key_pin.as_bytes() != published.host_public_key {
                return Err(RemoteTrustError::PinChanged);
            }
            Ok(Some(record))
        }
        Err(RemoteTrustError::NotFound) => Ok(None),
        Err(error) => Err(error),
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PairPostBody<'a> {
    t: &'a str,
    browser_install_id: String,
    label: &'a str,
}

/// Explicit pair + Noise/Hello enroll under one absolute deadline.
///
/// Disk uses `RemoteBlockingWork`. Device create may leave safe new custody after
/// cancel. Host trust publishes only after mutation `try_admit` immediately before
/// the atomic write. After that admit, timeout is `PersistenceUncertain` (write
/// still owned) — never claimed cancelled/unwritten. HostClient is never returned
/// after the absolute deadline.
pub async fn pair_enroll_and_connect(
    store: &RemoteTrustStore,
    mut request: PairEnrollRequest,
) -> Result<(HostClient, TrustedHostRecord), RemoteTrustError> {
    validate_pair_request(&request)?;
    let deadline_at = Instant::now() + request.deadline;
    let endpoint = validate_remote_endpoint(&request.endpoint)?;
    let additional_ca_pem = request.additional_ca_pem.take();
    let tls = RemoteTlsOptions {
        additional_ca_pem: additional_ca_pem.clone(),
    };
    let store_disk = RemoteTrustStore {
        root: store.root.clone(),
    };
    let device = run_remote_blocking_until(
        "remote-trust-enroll-device",
        deadline_at,
        BlockingWorkKind::Mutation,
        move |admission| store_disk.load_or_create_device_blocking(deadline_at, &admission),
    )
    .await?;

    let published = fetch_published_host_identity_until(&endpoint, &tls, deadline_at).await?;
    let store_prior = RemoteTrustStore {
        root: store.root.clone(),
    };
    let endpoint_prior = endpoint.clone();
    let published_prior = published.clone();
    let prior = run_remote_blocking_until(
        "remote-trust-enroll-prior",
        deadline_at,
        BlockingWorkKind::Read,
        move |admission| {
            load_prior_for_enroll(
                &store_prior,
                &endpoint_prior,
                &published_prior,
                deadline_at,
                &admission,
            )
        },
    )
    .await?;

    let mut pairing_code = std::mem::take(&mut request.pairing_code);
    let label = request.label.as_deref().unwrap_or("");
    let install_id = device.device_public_id.to_browser_install_id();
    let body_result = (|| {
        let body = PairPostBody {
            t: pairing_code.as_str(),
            browser_install_id: install_id,
            label,
        };
        serde_json::to_vec(&body)
    })();
    pairing_code.zeroize();
    let mut body_bytes = Zeroizing::new(body_result.map_err(|_| RemoteTrustError::PairingFailed)?);
    let cookie =
        post_pair_collect_cookie_until(&endpoint, body_bytes.as_slice(), &tls, deadline_at).await?;
    body_bytes.zeroize();

    remaining_until(deadline_at)?;
    let connect_endpoint = endpoint.with_connect_path(endpoint.path())?;
    let socket =
        open_remote_connect_ws_until(&connect_endpoint, Some(cookie.as_str()), &tls, deadline_at)
            .await?;
    let (sink, stream) = socket.split();
    let pin = ConnectNoiseStaticPublicKey::from_bytes(published.host_public_key)
        .map_err(|_| RemoteTrustError::Corrupt)?;
    let mut config = ConnectClientConfig::for_browser_fleet(
        published.host_public_id,
        pin,
        Some(*device.device_public_id.as_bytes()),
    );
    if let Some(prior_record) = prior.as_ref() {
        config.requested_client_id = Some(prior_record.assigned_client_id);
    }
    remaining_until(deadline_at)?;
    let client = match tokio::time::timeout(
        remaining_until(deadline_at)?,
        HostClient::connect_connect(config, device.noise_custody(), sink, stream),
    )
    .await
    {
        Ok(Ok(client)) => client,
        Ok(Err(_)) => return Err(RemoteTrustError::Unauthorized),
        Err(_) => return Err(RemoteTrustError::Timeout),
    };
    remaining_until(deadline_at)?;
    let session = client
        .metadata()
        .as_connect()
        .ok_or(RemoteTrustError::Corrupt)?;
    if session.host_public_id() != published.host_public_id
        || session.host_key_pin().as_bytes() != published.host_public_key
    {
        return Err(RemoteTrustError::PinChanged);
    }
    let record = TrustedHostRecord {
        host_public_id: published.host_public_id,
        host_key_pin: pin,
        endpoint: connect_endpoint.http_base().to_string(),
        connect_path: connect_endpoint.path().to_string(),
        assigned_client_id: session.assigned_client_id(),
        additional_ca_pem,
    };
    remaining_until(deadline_at)?;
    let store_persist = RemoteTrustStore {
        root: store.root.clone(),
    };
    let record_persist = record.clone();
    let cookie_persist = Zeroizing::new(cookie.as_str().to_string());
    run_remote_blocking_until(
        "remote-trust-enroll-persist",
        deadline_at,
        BlockingWorkKind::Mutation,
        move |admission| {
            store_persist.persist_trusted_host_transactional(
                &record_persist,
                cookie_persist.as_str(),
                deadline_at,
                &admission,
            )
        },
    )
    .await?;
    remaining_until(deadline_at)?;
    Ok((client, record))
}

/// Reconnect using persisted custody, host pin, cookie, and assigned client id.
pub async fn connect_trusted_host(
    store: &RemoteTrustStore,
    host_public_id: [u8; 16],
    options: ConnectTrustedOptions,
) -> Result<HostClient, RemoteTrustError> {
    let deadline_at = Instant::now() + options.deadline;
    let store_device = RemoteTrustStore {
        root: store.root.clone(),
    };
    let device = run_remote_blocking_until(
        "remote-trust-reconnect-device",
        deadline_at,
        BlockingWorkKind::Read,
        move |admission| {
            let _lock = store_device.acquire_lock()?;
            remaining_until(deadline_at)?;
            if !admission.try_admit() {
                return Err(RemoteTrustError::Cancelled);
            }
            store_device.load_device_unlocked()
        },
    )
    .await?;
    let store_host = RemoteTrustStore {
        root: store.root.clone(),
    };
    let (record, cookie) = run_remote_blocking_until(
        "remote-trust-reconnect-host",
        deadline_at,
        BlockingWorkKind::Read,
        move |admission| {
            let _lock = store_host.acquire_lock()?;
            remaining_until(deadline_at)?;
            if !admission.try_admit() {
                return Err(RemoteTrustError::Cancelled);
            }
            store_host.load_trusted_host_unlocked(host_public_id)
        },
    )
    .await?;
    if record.host_public_id != host_public_id {
        return Err(RemoteTrustError::Corrupt);
    }
    let tls = tls_options_for_trusted_reconnect(&record, &options)?;
    let endpoint = validate_remote_endpoint(&format!(
        "{}{}",
        record_endpoint_ws_base(&record),
        record.connect_path
    ))?;
    remaining_until(deadline_at)?;
    let socket =
        open_remote_connect_ws_until(&endpoint, Some(cookie.as_str()), &tls, deadline_at).await?;
    let (sink, stream) = socket.split();
    let mut config = ConnectClientConfig::for_browser_fleet(
        record.host_public_id,
        record.host_key_pin,
        Some(*device.device_public_id.as_bytes()),
    );
    config.requested_client_id = Some(record.assigned_client_id);
    match tokio::time::timeout(
        remaining_until(deadline_at)?,
        HostClient::connect_connect(config, device.noise_custody(), sink, stream),
    )
    .await
    {
        Ok(Ok(client)) => {
            remaining_until(deadline_at)?;
            Ok(client)
        }
        Ok(Err(_)) => Err(RemoteTrustError::Unauthorized),
        Err(_) => Err(RemoteTrustError::Timeout),
    }
}

/// List trusted remote host public records under an explicit store root.
///
/// Bounds the ID list to [`MAX_TRUSTED_REMOTE_HOSTS`] (15 remotes; fleet self +
/// remotes ≤ 16) before any DPAPI decrypt. Cookies are zeroized and never
/// returned. Does not create or mutate device custody. An empty hosts directory
/// is a legitimate empty roster; corrupt, oversized, or lock failures are typed
/// errors (never an empty "forget").
pub async fn list_trusted_hosts(
    store: &RemoteTrustStore,
    deadline: Duration,
) -> Result<Vec<TrustedHostRecord>, RemoteTrustError> {
    let deadline_at = Instant::now() + deadline;
    let root = store.root.clone();
    run_remote_blocking_until(
        "remote-trust-list-hosts",
        deadline_at,
        BlockingWorkKind::Read,
        move |admission| {
            RemoteTrustStore { root }.list_trusted_hosts_blocking(deadline_at, &admission)
        },
    )
    .await
}

/// Remove one exact trusted-host file after reload-and-compare of `expected`.
///
/// Absent records are idempotent success. A changed pin, endpoint, client id, or
/// CA fails closed without deleting the replacement. Device custody and other
/// host files are never touched.
///
/// Caller must first fence/join the exact HostFleet driver for this host and must
/// not activate late enrollment while forget runs. This function does not remove
/// HostFleet runtime state.
pub async fn forget_trusted_host(
    store: &RemoteTrustStore,
    expected: TrustedHostRecord,
    deadline: Duration,
) -> Result<(), RemoteTrustError> {
    let deadline_at = Instant::now() + deadline;
    let root = store.root.clone();
    run_remote_blocking_until(
        "remote-trust-forget-host",
        deadline_at,
        BlockingWorkKind::Mutation,
        move |admission| {
            RemoteTrustStore { root }.forget_trusted_host_blocking(
                &expected,
                deadline_at,
                &admission,
            )
        },
    )
    .await
}

fn record_endpoint_ws_base(record: &TrustedHostRecord) -> String {
    if record.endpoint.starts_with("https://") {
        record.endpoint.replacen("https://", "wss://", 1)
    } else {
        record.endpoint.replacen("http://", "ws://", 1)
    }
}

/// Default reconnect uses the stored per-host CA. An explicit override must match
/// the trusted record exactly; root replacement requires re-pair.
fn tls_options_for_trusted_reconnect(
    record: &TrustedHostRecord,
    options: &ConnectTrustedOptions,
) -> Result<RemoteTlsOptions, RemoteTrustError> {
    if let Some(override_pem) = options.additional_ca_pem.as_deref() {
        validate_additional_ca_pem(override_pem)?;
        match record.additional_ca_pem.as_deref() {
            Some(stored) if stored == override_pem => {}
            _ => return Err(RemoteTrustError::PinChanged),
        }
        return Ok(RemoteTlsOptions {
            additional_ca_pem: Some(override_pem.to_string()),
        });
    }
    Ok(RemoteTlsOptions {
        additional_ca_pem: record.additional_ca_pem.clone(),
    })
}

fn validate_pair_request(request: &PairEnrollRequest) -> Result<(), RemoteTrustError> {
    if request.endpoint.is_empty() {
        return Err(RemoteTrustError::Endpoint);
    }
    if request.pairing_code.is_empty() || request.pairing_code.len() > MAX_PAIRING_CODE_BYTES {
        return Err(RemoteTrustError::PairingFailed);
    }
    if request
        .pairing_code
        .bytes()
        .any(|byte| byte < 0x20 || byte == 0x7f)
    {
        return Err(RemoteTrustError::PairingFailed);
    }
    if let Some(label) = request.label.as_deref() {
        if label.len() > MAX_LABEL_BYTES || label.bytes().any(|byte| byte < 0x20 || byte == 0x7f) {
            return Err(RemoteTrustError::PairingFailed);
        }
    }
    if let Some(pem) = request.additional_ca_pem.as_deref() {
        validate_additional_ca_pem(pem)?;
    }
    Ok(())
}

/// Test helper: record a trusted host as if Hello already succeeded.
#[cfg(test)]
pub(crate) fn persist_trusted_host_for_test(
    store: &RemoteTrustStore,
    record: &TrustedHostRecord,
    cookie: &str,
) -> Result<(), RemoteTrustError> {
    let deadline_at = Instant::now() + REMOTE_TRANSPORT_DEFAULT_DEADLINE;
    let root = store.root.clone();
    let record = record.clone();
    let cookie = cookie.to_string();
    run_remote_blocking_sync(
        "remote-trust-persist-test",
        deadline_at,
        BlockingWorkKind::Mutation,
        move |admission| {
            RemoteTrustStore { root }.persist_trusted_host_transactional(
                &record,
                &cookie,
                deadline_at,
                &admission,
            )
        },
    )
}

#[cfg(test)]
fn persist_after_admit_seam() {
    if let Ok(guard) = PERSIST_AFTER_ADMIT_SEAM.lock() {
        if let Some(hook) = guard.as_ref() {
            hook();
        }
    }
}

#[cfg(test)]
static PERSIST_AFTER_ADMIT_SEAM: std::sync::Mutex<Option<Box<dyn Fn() + Send>>> =
    std::sync::Mutex::new(None);

#[cfg(test)]
fn set_persist_after_admit_seam(hook: Option<Box<dyn Fn() + Send>>) {
    *PERSIST_AFTER_ADMIT_SEAM.lock().expect("seam lock") = hook;
}

#[cfg(test)]
fn hold_store_lock_exclusive(store: &RemoteTrustStore) -> File {
    #[cfg(windows)]
    {
        store.revalidate_store_layout().expect("layout");
        let lock_path = store.root.join(STORE_LOCK_FILE_NAME);
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true);
        use std::os::windows::fs::OpenOptionsExt;
        use windows::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT;
        options
            .share_mode(0)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT.0);
        options.open(&lock_path).expect("hold lock")
    }
    #[cfg(not(windows))]
    {
        let _ = store;
        panic!("store lock tests require Windows exclusive share_mode(0)");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connect::ConnectNoiseCustody;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::mpsc;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn test_store_root() -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let base = crate::persistence::app_config_dir().expect("test config");
        let root = base.join(format!("remote-native-trust-case-{n}"));
        fs::create_dir_all(&root).expect("mkdir");
        root
    }

    fn test_store() -> RemoteTrustStore {
        RemoteTrustStore::open(test_store_root()).expect("store")
    }

    fn fixture_host_id(seed: u8) -> [u8; 16] {
        let mut id = [seed; 16];
        id[6] = 0x70;
        id[8] = 0x80;
        id
    }

    fn fixture_client_id(seed: u8) -> ClientId {
        ClientId::from_bytes({
            let mut id = [seed; 16];
            id[6] = 0x70;
            id[8] = 0x80;
            id
        })
        .expect("client")
    }

    async fn serve_once(response: &'static [u8]) -> u16 {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept");
            let mut buf = vec![0_u8; 8192];
            let _ = stream.read(&mut buf).await;
            let _ = stream.write_all(response).await;
        });
        port
    }

    #[test]
    fn device_custody_stable_reload_and_corrupt_failclosed() {
        let store = test_store();
        let first = match store.load_or_create_device() {
            Ok(device) => device,
            Err(RemoteTrustError::Unsupported) => return,
            Err(error) => panic!("create device: {error:?}"),
        };
        let second = store.load_or_create_device().expect("reload");
        assert_eq!(first.device_public_id.0, second.device_public_id.0);

        let path = store.device_path();
        fs::write(&path, br#"{"schema":"devmanager.remote-native-device/v1","devicePublicId":"11","publicKey":"22","privateKeyProtected":"aa"}"#)
            .expect("corrupt");
        assert_eq!(store.load_device().unwrap_err(), RemoteTrustError::Corrupt);
        assert_eq!(
            store.load_or_create_device().unwrap_err(),
            RemoteTrustError::Corrupt
        );
    }

    #[test]
    fn moved_encrypted_custody_fails_at_new_root() {
        let store = test_store();
        match store.load_or_create_device() {
            Ok(_) => {}
            Err(RemoteTrustError::Unsupported) => return,
            Err(error) => panic!("{error:?}"),
        }
        let bytes = fs::read(store.device_path()).expect("read device");
        let other_root = test_store_root();
        let other = RemoteTrustStore::open(other_root).expect("other store");
        fs::create_dir_all(other.root()).expect("other root");
        write_store_file_atomic(&other.device_path(), &bytes)
            .expect("copy encrypted with private ACL");
        // Root path is bound into DPAPI entropy; moved ciphertext must fail closed.
        assert!(matches!(
            other.load_device(),
            Err(RemoteTrustError::Custody
                | RemoteTrustError::Corrupt
                | RemoteTrustError::Unsupported)
        ));
    }

    #[test]
    fn concurrent_device_create_converges_on_one_identity() {
        let root = test_store_root();
        let store_a = RemoteTrustStore::open(root.clone()).expect("a");
        let store_b = RemoteTrustStore::open(root).expect("b");
        let handle_a = std::thread::spawn(move || store_a.load_or_create_device());
        let handle_b = std::thread::spawn(move || store_b.load_or_create_device());
        let a = match handle_a.join().expect("join a") {
            Ok(device) => device,
            Err(RemoteTrustError::Unsupported) => return,
            Err(error) => panic!("{error:?}"),
        };
        let b = match handle_b.join().expect("join b") {
            Ok(device) => device,
            Err(RemoteTrustError::Unsupported) => return,
            Err(error) => panic!("{error:?}"),
        };
        assert_eq!(a.device_public_id.0, b.device_public_id.0);
        assert_eq!(a.public_key().as_bytes(), b.public_key().as_bytes());
    }

    #[test]
    fn metadata_tamper_breaks_cookie_unprotect() {
        let store = test_store();
        match store.load_or_create_device() {
            Ok(_) => {}
            Err(RemoteTrustError::Unsupported) => return,
            Err(error) => panic!("{error:?}"),
        }
        let host = ConnectNoiseCustody::generate().expect("host");
        let host_id = fixture_host_id(7);
        let record = TrustedHostRecord {
            host_public_id: host_id,
            host_key_pin: host.public(),
            endpoint: "http://127.0.0.1:9".to_string(),
            connect_path: "/api/connect".to_string(),
            assigned_client_id: fixture_client_id(3),
            additional_ca_pem: None,
        };
        persist_trusted_host_for_test(&store, &record, "dm_web=old-cookie").expect("persist");
        let path = store.host_path(host_id);
        let mut text = fs::read_to_string(&path).expect("read");
        text = text.replace("/api/connect", "/api/other");
        fs::write(&path, text).expect("tamper path");
        assert!(matches!(
            store.load_trusted_host(host_id),
            Err(RemoteTrustError::Custody | RemoteTrustError::Corrupt)
        ));
    }

    #[test]
    fn corrupt_prior_host_is_not_treated_as_absence() {
        let store = test_store();
        match store.load_or_create_device() {
            Ok(_) => {}
            Err(RemoteTrustError::Unsupported) => return,
            Err(error) => panic!("{error:?}"),
        }
        let host_id = fixture_host_id(8);
        let path = store.host_path(host_id);
        write_store_file_atomic(&path, b"{not-json").expect("corrupt with private ACL");
        let published = PublishedHostIdentity {
            host_public_id: host_id,
            host_public_key: [9; 32],
        };
        let endpoint = validate_remote_endpoint("http://127.0.0.1:9/").unwrap();
        let deadline_at = Instant::now() + REMOTE_TRANSPORT_DEFAULT_DEADLINE;
        let root = store.root.clone();
        let err = run_remote_blocking_sync(
            "remote-trust-prior-corrupt-test",
            deadline_at,
            BlockingWorkKind::Read,
            move |admission| {
                load_prior_for_enroll(
                    &RemoteTrustStore { root },
                    &endpoint,
                    &published,
                    deadline_at,
                    &admission,
                )
            },
        )
        .unwrap_err();
        assert_eq!(err, RemoteTrustError::Corrupt);
        assert_eq!(fs::read(&path).unwrap(), b"{not-json");
    }

    #[tokio::test]
    async fn cancel_queued_before_host_commit_leaves_old_record_bytes_exact() {
        let store = test_store();
        match store.load_or_create_device() {
            Ok(_) => {}
            Err(RemoteTrustError::Unsupported) => return,
            Err(error) => panic!("{error:?}"),
        }
        let host = ConnectNoiseCustody::generate().expect("host");
        let host_id = fixture_host_id(11);
        let record = TrustedHostRecord {
            host_public_id: host_id,
            host_key_pin: host.public(),
            endpoint: "http://127.0.0.1:9".to_string(),
            connect_path: "/api/connect".to_string(),
            assigned_client_id: fixture_client_id(11),
            additional_ca_pem: None,
        };
        persist_trusted_host_for_test(&store, &record, "dm_web=old-cookie").expect("persist");
        let path = store.host_path(host_id);
        let old_bytes = fs::read(&path).expect("old bytes");

        let held = hold_store_lock_exclusive(&store);
        let deadline_at = Instant::now() + Duration::from_secs(5);
        let root = store.root.clone();
        let mut updated = record.clone();
        updated.assigned_client_id = fixture_client_id(12);
        let job = RemoteBlockingWork::spawn(
            "remote-trust-cancel-before-commit",
            deadline_at,
            move |admission| {
                RemoteTrustStore { root }.persist_trusted_host_transactional(
                    &updated,
                    "dm_web=new-cookie",
                    deadline_at,
                    &admission,
                )
            },
        )
        .expect("spawn");
        // Worker should block on the held exclusive lock before try_admit.
        tokio::time::sleep(Duration::from_millis(80)).await;
        drop(job);
        drop(held);
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(fs::read(&path).expect("still old"), old_bytes);
        let (_loaded, cookie) = store.load_trusted_host(host_id).expect("old trust");
        assert_eq!(cookie.as_str(), "dm_web=old-cookie");
    }

    #[tokio::test]
    async fn admitted_slow_host_commit_returns_persistence_uncertain_and_still_owned() {
        let store = test_store();
        match store.load_or_create_device() {
            Ok(_) => {}
            Err(RemoteTrustError::Unsupported) => return,
            Err(error) => panic!("{error:?}"),
        }
        let host = ConnectNoiseCustody::generate().expect("host");
        let host_id = fixture_host_id(13);
        let record = TrustedHostRecord {
            host_public_id: host_id,
            host_key_pin: host.public(),
            endpoint: "http://127.0.0.1:9".to_string(),
            connect_path: "/api/connect".to_string(),
            assigned_client_id: fixture_client_id(13),
            additional_ca_pem: None,
        };
        let path = store.host_path(host_id);
        let (release, hold) = mpsc::sync_channel::<()>(1);
        set_persist_after_admit_seam(Some(Box::new(move || {
            let _ = hold.recv_timeout(Duration::from_secs(5));
        })));
        let deadline_at = Instant::now() + Duration::from_millis(250);
        let root = store.root.clone();
        let error = run_remote_blocking_until(
            "remote-trust-admitted-slow-persist",
            deadline_at,
            BlockingWorkKind::Mutation,
            move |admission| {
                RemoteTrustStore { root }.persist_trusted_host_transactional(
                    &record,
                    "dm_web=slow-commit",
                    Instant::now() + Duration::from_secs(5),
                    &admission,
                )
            },
        )
        .await
        .expect_err("must be uncertain");
        assert_eq!(error, RemoteTrustError::PersistenceUncertain);
        let _ = release.send(());
        set_persist_after_admit_seam(None);
        let mut saw = false;
        for _ in 0..50 {
            if let Ok((_loaded, cookie)) = store.load_trusted_host(host_id) {
                if cookie.as_str() == "dm_web=slow-commit" {
                    saw = true;
                    break;
                }
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(
            saw,
            "admitted commit must remain owned and settle: {path:?}"
        );
    }

    #[tokio::test]
    async fn failed_pair_leaves_old_trust_via_production_api() {
        let store = test_store();
        match store.load_or_create_device() {
            Ok(_) => {}
            Err(RemoteTrustError::Unsupported) => return,
            Err(error) => panic!("{error:?}"),
        }
        let host = ConnectNoiseCustody::generate().expect("host");
        let host_id = fixture_host_id(5);
        let record = TrustedHostRecord {
            host_public_id: host_id,
            host_key_pin: host.public(),
            endpoint: "http://127.0.0.1:9".to_string(),
            connect_path: "/api/connect".to_string(),
            assigned_client_id: fixture_client_id(4),
            additional_ca_pem: None,
        };
        persist_trusted_host_for_test(&store, &record, "dm_web=old-cookie").expect("persist");

        let key = hex_encode(&host.public().as_bytes());
        let id = uuid::Uuid::from_bytes(host_id).to_string();
        let meta =
            format!(r#"{{"transport":"connect","hostPublicId":"{id}","hostPublicKey":"{key}"}}"#);
        let html = format!(
            "<html><head><meta name=\"devmanager-connect\" content=\"{}\"></head></html>",
            meta.replace('"', "&quot;")
        );
        let body = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{html}",
            html.len()
        );
        // Leak static response for the short-lived mock (test-only).
        let response: &'static [u8] = Box::leak(body.into_bytes().into_boxed_slice());
        let port = serve_once(response).await;
        // Pair will fail: mock only answers one GET; POST never gets a cookie.
        let request = PairEnrollRequest {
            endpoint: format!("http://127.0.0.1:{port}/"),
            pairing_code: Zeroizing::new("ABCD1234".to_string()),
            deadline: Duration::from_secs(2),
            ..PairEnrollRequest::default()
        };
        let error = pair_enroll_and_connect(&store, request)
            .await
            .err()
            .expect("pair must fail");
        assert!(matches!(
            error,
            RemoteTrustError::Unavailable
                | RemoteTrustError::PairingFailed
                | RemoteTrustError::Unauthorized
                | RemoteTrustError::Timeout
                | RemoteTrustError::Corrupt
                | RemoteTrustError::PinChanged
        ));
        let (loaded, cookie) = store.load_trusted_host(host_id).expect("old trust");
        assert_eq!(loaded.host_key_pin.as_bytes(), host.public().as_bytes());
        assert_eq!(cookie.as_str(), "dm_web=old-cookie");
    }

    #[tokio::test]
    async fn changed_published_host_id_same_endpoint_rejects() {
        let store = test_store();
        match store.load_or_create_device() {
            Ok(_) => {}
            Err(RemoteTrustError::Unsupported) => return,
            Err(error) => panic!("{error:?}"),
        }
        let host = ConnectNoiseCustody::generate().expect("host");
        let host_id = fixture_host_id(6);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let port = listener.local_addr().unwrap().port();
        let record = TrustedHostRecord {
            host_public_id: host_id,
            host_key_pin: host.public(),
            endpoint: format!("http://127.0.0.1:{port}"),
            connect_path: "/api/connect".to_string(),
            assigned_client_id: fixture_client_id(6),
            additional_ca_pem: None,
        };
        persist_trusted_host_for_test(&store, &record, "dm_web=old-cookie").expect("persist");

        let other_id = fixture_host_id(9);
        let other_key = hex_encode(&[0xab; 32]);
        let id = uuid::Uuid::from_bytes(other_id).to_string();
        let meta = format!(
            r#"{{"transport":"connect","hostPublicId":"{id}","hostPublicKey":"{other_key}"}}"#
        );
        let html = format!(
            "<html><head><meta name=\"devmanager-connect\" content=\"{}\"></head></html>",
            meta.replace('"', "&quot;")
        );
        let body = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{html}",
            html.len()
        );
        let response: &'static [u8] = Box::leak(body.into_bytes().into_boxed_slice());
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept");
            let mut buf = vec![0_u8; 8192];
            let _ = stream.read(&mut buf).await;
            let _ = stream.write_all(response).await;
        });

        let request = PairEnrollRequest {
            endpoint: format!("http://127.0.0.1:{port}/"),
            pairing_code: Zeroizing::new("ABCD1234".to_string()),
            deadline: Duration::from_secs(2),
            ..PairEnrollRequest::default()
        };
        let error = pair_enroll_and_connect(&store, request)
            .await
            .err()
            .expect("host id change");
        assert_eq!(error, RemoteTrustError::PinChanged);
        let (loaded, cookie) = store.load_trusted_host(host_id).expect("preserved");
        assert_eq!(loaded.host_public_id, host_id);
        assert_eq!(cookie.as_str(), "dm_web=old-cookie");
    }

    #[test]
    fn trusted_host_stable_reload() {
        let store = test_store();
        match store.load_or_create_device() {
            Ok(_) => {}
            Err(RemoteTrustError::Unsupported) => return,
            Err(error) => panic!("{error:?}"),
        }
        let host = ConnectNoiseCustody::generate().expect("host");
        let host_id = fixture_host_id(9);
        let record = TrustedHostRecord {
            host_public_id: host_id,
            host_key_pin: host.public(),
            endpoint: "https://example.test:8443".to_string(),
            connect_path: "/api/connect".to_string(),
            assigned_client_id: fixture_client_id(4),
            additional_ca_pem: None,
        };
        persist_trusted_host_for_test(&store, &record, "dm_web=stable").expect("persist");
        let (a, ca) = store.load_trusted_host(host_id).expect("a");
        let (b, cb) = store.load_trusted_host(host_id).expect("b");
        assert_eq!(a, b);
        assert_eq!(ca.as_str(), cb.as_str());
    }

    fn fixture_ca_pem() -> String {
        let certified = rcgen::generate_simple_self_signed(vec!["lan.test".to_string()])
            .expect("self-signed CA");
        certified.cert.pem()
    }

    #[test]
    fn additional_ca_pem_roundtrip_bound_into_cookie_entropy() {
        let store = test_store();
        match store.load_or_create_device() {
            Ok(_) => {}
            Err(RemoteTrustError::Unsupported) => return,
            Err(error) => panic!("{error:?}"),
        }
        let ca = fixture_ca_pem();
        let host = ConnectNoiseCustody::generate().expect("host");
        let host_id = fixture_host_id(21);
        let record = TrustedHostRecord {
            host_public_id: host_id,
            host_key_pin: host.public(),
            endpoint: "https://lan.test:8443".to_string(),
            connect_path: "/api/connect".to_string(),
            assigned_client_id: fixture_client_id(21),
            additional_ca_pem: Some(ca.clone()),
        };
        persist_trusted_host_for_test(&store, &record, "dm_web=lan-cookie").expect("persist");
        let (loaded, cookie) = store.load_trusted_host(host_id).expect("load");
        assert_eq!(loaded.additional_ca_pem.as_deref(), Some(ca.as_str()));
        assert_eq!(cookie.as_str(), "dm_web=lan-cookie");
        let debug = format!("{loaded:?}");
        assert!(debug.contains("additional_ca_pem_bytes"));
        assert!(!debug.contains("BEGIN CERTIFICATE"));
    }

    #[test]
    fn legacy_host_file_without_ca_field_keeps_cookie_entropy() {
        let store = test_store();
        match store.load_or_create_device() {
            Ok(_) => {}
            Err(RemoteTrustError::Unsupported) => return,
            Err(error) => panic!("{error:?}"),
        }
        let host = ConnectNoiseCustody::generate().expect("host");
        let host_id = fixture_host_id(22);
        let host_public_id = uuid::Uuid::from_bytes(host_id).to_string();
        let host_public_key = hex_encode(&host.public().as_bytes());
        let assigned = fixture_client_id(22).to_string();
        let endpoint = "http://127.0.0.1:9".to_string();
        let connect_path = "/api/connect".to_string();
        let scope = host_cookie_custody_scope(
            store.root(),
            HOST_SCHEMA,
            &host_public_id,
            &host_public_key,
            &endpoint,
            &connect_path,
            &assigned,
            None,
        );
        let protected = protect_bytes(b"dm_web=legacy", &scope).expect("protect");
        let file = HostFile {
            schema: HOST_SCHEMA.to_string(),
            host_public_id,
            host_public_key,
            endpoint,
            connect_path,
            assigned_client_id: assigned,
            pairing_cookie_protected: protected,
            additional_ca_pem: None,
        };
        let bytes = serde_json::to_vec(&file).expect("json");
        assert!(
            !std::str::from_utf8(&bytes)
                .unwrap()
                .contains("additionalCaPem"),
            "legacy serialize must omit CA field"
        );
        write_store_file_atomic(&store.host_path(host_id), &bytes).expect("write");
        let (loaded, cookie) = store.load_trusted_host(host_id).expect("legacy load");
        assert!(loaded.additional_ca_pem.is_none());
        assert_eq!(cookie.as_str(), "dm_web=legacy");
    }

    #[test]
    fn tampered_additional_ca_pem_breaks_cookie_unprotect() {
        let store = test_store();
        match store.load_or_create_device() {
            Ok(_) => {}
            Err(RemoteTrustError::Unsupported) => return,
            Err(error) => panic!("{error:?}"),
        }
        let ca = fixture_ca_pem();
        let other = fixture_ca_pem();
        let host = ConnectNoiseCustody::generate().expect("host");
        let host_id = fixture_host_id(23);
        let record = TrustedHostRecord {
            host_public_id: host_id,
            host_key_pin: host.public(),
            endpoint: "https://lan.test:8443".to_string(),
            connect_path: "/api/connect".to_string(),
            assigned_client_id: fixture_client_id(23),
            additional_ca_pem: Some(ca),
        };
        persist_trusted_host_for_test(&store, &record, "dm_web=bound").expect("persist");
        let path = store.host_path(host_id);
        let bytes = fs::read(&path).expect("read");
        let mut file: HostFile = serde_json::from_slice(&bytes).expect("parse");
        file.additional_ca_pem = Some(other);
        fs::write(&path, serde_json::to_vec(&file).expect("rewrite")).expect("tamper");
        assert!(matches!(
            store.load_trusted_host(host_id),
            Err(RemoteTrustError::Custody | RemoteTrustError::Corrupt)
        ));
    }

    #[test]
    fn default_reconnect_tls_uses_stored_ca_override_must_match() {
        let ca = fixture_ca_pem();
        let other = fixture_ca_pem();
        let host = ConnectNoiseCustody::generate().expect("host");
        let record = TrustedHostRecord {
            host_public_id: fixture_host_id(24),
            host_key_pin: host.public(),
            endpoint: "https://lan.test:8443".to_string(),
            connect_path: "/api/connect".to_string(),
            assigned_client_id: fixture_client_id(24),
            additional_ca_pem: Some(ca.clone()),
        };
        let default_tls =
            tls_options_for_trusted_reconnect(&record, &ConnectTrustedOptions::default())
                .expect("default");
        assert_eq!(default_tls.additional_ca_pem.as_deref(), Some(ca.as_str()));
        let matched = tls_options_for_trusted_reconnect(
            &record,
            &ConnectTrustedOptions {
                additional_ca_pem: Some(ca.clone()),
                ..ConnectTrustedOptions::default()
            },
        )
        .expect("match");
        assert_eq!(matched.additional_ca_pem.as_deref(), Some(ca.as_str()));
        assert_eq!(
            tls_options_for_trusted_reconnect(
                &record,
                &ConnectTrustedOptions {
                    additional_ca_pem: Some(other),
                    ..ConnectTrustedOptions::default()
                },
            )
            .unwrap_err(),
            RemoteTrustError::PinChanged
        );
        let legacy = TrustedHostRecord {
            additional_ca_pem: None,
            ..record.clone()
        };
        assert_eq!(
            tls_options_for_trusted_reconnect(
                &legacy,
                &ConnectTrustedOptions {
                    additional_ca_pem: Some(ca),
                    ..ConnectTrustedOptions::default()
                },
            )
            .unwrap_err(),
            RemoteTrustError::PinChanged
        );
    }

    #[test]
    fn invalid_or_oversized_additional_ca_rejected_before_network() {
        let oversized = "A".repeat(crate::client::remote_transport::REMOTE_CA_PEM_MAX_BYTES + 1);
        let bad = PairEnrollRequest {
            endpoint: "https://lan.test:8443/".to_string(),
            pairing_code: Zeroizing::new("ABCD1234".to_string()),
            additional_ca_pem: Some(oversized),
            ..PairEnrollRequest::default()
        };
        assert!(matches!(
            validate_pair_request(&bad),
            Err(RemoteTrustError::Corrupt | RemoteTrustError::Unauthorized)
        ));
        let invalid = PairEnrollRequest {
            endpoint: "https://lan.test:8443/".to_string(),
            pairing_code: Zeroizing::new("ABCD1234".to_string()),
            additional_ca_pem: Some("not-a-pem".to_string()),
            ..PairEnrollRequest::default()
        };
        assert!(matches!(
            validate_pair_request(&invalid),
            Err(RemoteTrustError::Corrupt | RemoteTrustError::Unauthorized)
        ));
        let host = ConnectNoiseCustody::generate().expect("host");
        let record = TrustedHostRecord {
            host_public_id: fixture_host_id(25),
            host_key_pin: host.public(),
            endpoint: "https://lan.test:8443".to_string(),
            connect_path: "/api/connect".to_string(),
            assigned_client_id: fixture_client_id(25),
            additional_ca_pem: Some(
                "-----BEGIN CERTIFICATE-----\nnotcert\n-----END CERTIFICATE-----\n".into(),
            ),
        };
        assert!(matches!(
            validate_trusted_host_record(&record),
            Err(RemoteTrustError::Corrupt | RemoteTrustError::Unauthorized)
        ));
    }

    #[tokio::test]
    async fn list_trusted_hosts_returns_sorted_records_without_device_or_cookie_leak() {
        let store = test_store();
        assert!(!store.device_path().exists());
        let host_a = ConnectNoiseCustody::generate().expect("host a");
        let host_b = ConnectNoiseCustody::generate().expect("host b");
        let id_lo = fixture_host_id(0x21);
        let id_hi = fixture_host_id(0x42);
        let record_hi = TrustedHostRecord {
            host_public_id: id_hi,
            host_key_pin: host_a.public(),
            endpoint: "http://127.0.0.1:9".to_string(),
            connect_path: "/api/connect".to_string(),
            assigned_client_id: fixture_client_id(0x21),
            additional_ca_pem: None,
        };
        let record_lo = TrustedHostRecord {
            host_public_id: id_lo,
            host_key_pin: host_b.public(),
            endpoint: "http://127.0.0.1:10".to_string(),
            connect_path: "/api/connect".to_string(),
            assigned_client_id: fixture_client_id(0x22),
            additional_ca_pem: None,
        };
        match persist_trusted_host_for_test(&store, &record_hi, "dm_web=cookie-hi") {
            Ok(()) => {}
            Err(RemoteTrustError::Unsupported) => return,
            Err(error) => panic!("{error:?}"),
        }
        persist_trusted_host_for_test(&store, &record_lo, "dm_web=cookie-lo").expect("persist lo");
        assert!(
            !store.device_path().exists(),
            "list must not create device custody"
        );

        let listed = list_trusted_hosts(&store, REMOTE_TRANSPORT_DEFAULT_DEADLINE)
            .await
            .expect("list");
        assert_eq!(listed.len(), 2);
        assert!(listed[0].host_public_id < listed[1].host_public_id);
        assert_eq!(listed[0], record_lo);
        assert_eq!(listed[1], record_hi);
        let debug = format!("{listed:?}");
        assert!(!debug.contains("cookie-hi"));
        assert!(!debug.contains("cookie-lo"));

        let (_a, cookie_hi) = store.load_trusted_host(id_hi).expect("hi still stored");
        let (_b, cookie_lo) = store.load_trusted_host(id_lo).expect("lo still stored");
        assert_eq!(cookie_hi.as_str(), "dm_web=cookie-hi");
        assert_eq!(cookie_lo.as_str(), "dm_web=cookie-lo");
        assert!(!store.device_path().exists());
    }

    #[tokio::test]
    async fn list_trusted_hosts_empty_hosts_dir_is_empty_roster() {
        let store = test_store();
        let listed = list_trusted_hosts(&store, REMOTE_TRANSPORT_DEFAULT_DEADLINE)
            .await
            .expect("empty");
        assert!(listed.is_empty());
        assert!(!store.device_path().exists());
    }

    #[tokio::test]
    async fn list_trusted_hosts_accepts_exactly_the_protected_record_limit() {
        let store = test_store();
        let mut expected = Vec::new();
        for index in 1..=MAX_TRUSTED_REMOTE_HOSTS {
            let record = TrustedHostRecord {
                host_public_id: fixture_host_id(index as u8),
                host_key_pin: ConnectNoiseCustody::generate().expect("host").public(),
                endpoint: format!("http://127.0.0.1:{}", 9000 + index),
                connect_path: "/api/connect".into(),
                assigned_client_id: fixture_client_id(index as u8),
                additional_ca_pem: None,
            };
            match persist_trusted_host_for_test(&store, &record, "dm_web=boundary-fixture") {
                Ok(()) => expected.push(record),
                Err(RemoteTrustError::Unsupported) => return,
                Err(error) => panic!("{error:?}"),
            }
        }
        expected.sort_by_key(|record| record.host_public_id);
        let listed = list_trusted_hosts(&store, REMOTE_TRANSPORT_DEFAULT_DEADLINE)
            .await
            .expect("all protected records at the exact limit must load");
        assert_eq!(listed, expected);
        assert!(!store.device_path().exists());
        assert!(!format!("{listed:?}").contains("boundary-fixture"));
    }

    #[tokio::test]
    async fn list_trusted_hosts_over_cap_fails_before_decrypt() {
        let store = test_store();
        let hosts = store.root.join(HOSTS_DIR_NAME);
        fs::create_dir_all(&hosts).expect("hosts dir");
        for index in 0..=MAX_TRUSTED_REMOTE_HOSTS {
            let id = fixture_host_id(index as u8);
            let path = store.host_path(id);
            // Valid ID filename, intentionally corrupt body — must fail on count, not decrypt.
            write_store_file_atomic(&path, b"{not-a-valid-host").expect("fixture");
        }
        let error = list_trusted_hosts(&store, REMOTE_TRANSPORT_DEFAULT_DEADLINE)
            .await
            .expect_err("over cap");
        assert_eq!(error, RemoteTrustError::Corrupt);
        // Corrupt body must remain untouched (proves no successful decrypt rewrite).
        let survivor = store.host_path(fixture_host_id(0));
        assert_eq!(fs::read(&survivor).unwrap(), b"{not-a-valid-host");
    }

    #[tokio::test]
    async fn list_trusted_hosts_corrupt_record_is_not_empty_forget() {
        let store = test_store();
        match persist_trusted_host_for_test(
            &store,
            &TrustedHostRecord {
                host_public_id: fixture_host_id(0x31),
                host_key_pin: ConnectNoiseCustody::generate().expect("host").public(),
                endpoint: "http://127.0.0.1:9".to_string(),
                connect_path: "/api/connect".to_string(),
                assigned_client_id: fixture_client_id(0x31),
                additional_ca_pem: None,
            },
            "dm_web=ok",
        ) {
            Ok(()) => {}
            Err(RemoteTrustError::Unsupported) => return,
            Err(error) => panic!("{error:?}"),
        }
        let bad_id = fixture_host_id(0x32);
        write_store_file_atomic(&store.host_path(bad_id), b"{broken").expect("corrupt sibling");
        let error = list_trusted_hosts(&store, REMOTE_TRANSPORT_DEFAULT_DEADLINE)
            .await
            .expect_err("corrupt");
        assert_eq!(error, RemoteTrustError::Corrupt);
        assert!(store.host_path(fixture_host_id(0x31)).exists());
        assert!(store.host_path(bad_id).exists());
    }

    #[tokio::test]
    async fn forget_trusted_host_removes_exact_a_leaves_b_and_device() {
        let store = test_store();
        let device = match store.load_or_create_device() {
            Ok(device) => device,
            Err(RemoteTrustError::Unsupported) => return,
            Err(error) => panic!("{error:?}"),
        };
        let device_bytes = fs::read(store.device_path()).expect("device bytes");
        let host_a = ConnectNoiseCustody::generate().expect("a");
        let host_b = ConnectNoiseCustody::generate().expect("b");
        let record_a = TrustedHostRecord {
            host_public_id: fixture_host_id(0x41),
            host_key_pin: host_a.public(),
            endpoint: "http://127.0.0.1:9".to_string(),
            connect_path: "/api/connect".to_string(),
            assigned_client_id: fixture_client_id(0x41),
            additional_ca_pem: None,
        };
        let record_b = TrustedHostRecord {
            host_public_id: fixture_host_id(0x42),
            host_key_pin: host_b.public(),
            endpoint: "http://127.0.0.1:10".to_string(),
            connect_path: "/api/connect".to_string(),
            assigned_client_id: fixture_client_id(0x42),
            additional_ca_pem: None,
        };
        persist_trusted_host_for_test(&store, &record_a, "dm_web=a").expect("a");
        persist_trusted_host_for_test(&store, &record_b, "dm_web=b").expect("b");

        forget_trusted_host(&store, record_a.clone(), REMOTE_TRANSPORT_DEFAULT_DEADLINE)
            .await
            .expect("forget a");
        assert!(!store.host_path(record_a.host_public_id).exists());
        let (_b, cookie_b) = store
            .load_trusted_host(record_b.host_public_id)
            .expect("b remains");
        assert_eq!(cookie_b.as_str(), "dm_web=b");
        assert_eq!(fs::read(store.device_path()).unwrap(), device_bytes);
        assert_eq!(
            store.load_device().unwrap().device_public_id.0,
            device.device_public_id.0
        );
    }

    #[tokio::test]
    async fn forget_trusted_host_unknown_is_idempotent() {
        let store = test_store();
        let host = ConnectNoiseCustody::generate().expect("host");
        let missing = TrustedHostRecord {
            host_public_id: fixture_host_id(0x51),
            host_key_pin: host.public(),
            endpoint: "http://127.0.0.1:9".to_string(),
            connect_path: "/api/connect".to_string(),
            assigned_client_id: fixture_client_id(0x51),
            additional_ca_pem: None,
        };
        forget_trusted_host(&store, missing, REMOTE_TRANSPORT_DEFAULT_DEADLINE)
            .await
            .expect("idempotent");
    }

    #[tokio::test]
    async fn forget_trusted_host_stale_expected_preserves_replacement() {
        let store = test_store();
        let host = ConnectNoiseCustody::generate().expect("host");
        let host_id = fixture_host_id(0x61);
        let old = TrustedHostRecord {
            host_public_id: host_id,
            host_key_pin: host.public(),
            endpoint: "http://127.0.0.1:9".to_string(),
            connect_path: "/api/connect".to_string(),
            assigned_client_id: fixture_client_id(0x61),
            additional_ca_pem: None,
        };
        match persist_trusted_host_for_test(&store, &old, "dm_web=old") {
            Ok(()) => {}
            Err(RemoteTrustError::Unsupported) => return,
            Err(error) => panic!("{error:?}"),
        }
        let mut replacement = old.clone();
        replacement.assigned_client_id = fixture_client_id(0x62);
        persist_trusted_host_for_test(&store, &replacement, "dm_web=new").expect("replace");
        let error = forget_trusted_host(&store, old, REMOTE_TRANSPORT_DEFAULT_DEADLINE)
            .await
            .expect_err("stale");
        assert_eq!(error, RemoteTrustError::PinChanged);
        let (_loaded, cookie) = store.load_trusted_host(host_id).expect("preserved");
        assert_eq!(cookie.as_str(), "dm_web=new");
        assert_eq!(
            store
                .load_trusted_host(host_id)
                .unwrap()
                .0
                .assigned_client_id,
            replacement.assigned_client_id
        );
    }

    #[tokio::test]
    async fn forget_queued_cancel_before_admission_never_removes() {
        let store = test_store();
        let host = ConnectNoiseCustody::generate().expect("host");
        let host_id = fixture_host_id(0x71);
        let record = TrustedHostRecord {
            host_public_id: host_id,
            host_key_pin: host.public(),
            endpoint: "http://127.0.0.1:9".to_string(),
            connect_path: "/api/connect".to_string(),
            assigned_client_id: fixture_client_id(0x71),
            additional_ca_pem: None,
        };
        match persist_trusted_host_for_test(&store, &record, "dm_web=keep") {
            Ok(()) => {}
            Err(RemoteTrustError::Unsupported) => return,
            Err(error) => panic!("{error:?}"),
        }
        let path = store.host_path(host_id);
        let old_bytes = fs::read(&path).expect("bytes");
        let held = hold_store_lock_exclusive(&store);
        let deadline_at = Instant::now() + Duration::from_secs(5);
        let root = store.root.clone();
        let expected = record.clone();
        let job = RemoteBlockingWork::spawn(
            "remote-trust-forget-cancel-before-admit",
            deadline_at,
            move |admission| {
                RemoteTrustStore { root }.forget_trusted_host_blocking(
                    &expected,
                    deadline_at,
                    &admission,
                )
            },
        )
        .expect("spawn");
        tokio::time::sleep(Duration::from_millis(80)).await;
        drop(job);
        drop(held);
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(fs::read(&path).expect("still present"), old_bytes);
        let (_loaded, cookie) = store.load_trusted_host(host_id).expect("kept");
        assert_eq!(cookie.as_str(), "dm_web=keep");
    }

    #[tokio::test]
    async fn forget_admitted_timeout_is_persistence_uncertain_not_cancelled() {
        let store = test_store();
        let host = ConnectNoiseCustody::generate().expect("host");
        let host_id = fixture_host_id(0x81);
        let record = TrustedHostRecord {
            host_public_id: host_id,
            host_key_pin: host.public(),
            endpoint: "http://127.0.0.1:9".to_string(),
            connect_path: "/api/connect".to_string(),
            assigned_client_id: fixture_client_id(0x81),
            additional_ca_pem: None,
        };
        match persist_trusted_host_for_test(&store, &record, "dm_web=will-forget") {
            Ok(()) => {}
            Err(RemoteTrustError::Unsupported) => return,
            Err(error) => panic!("{error:?}"),
        }
        let (release, hold) = mpsc::sync_channel::<()>(1);
        set_persist_after_admit_seam(Some(Box::new(move || {
            let _ = hold.recv_timeout(Duration::from_secs(5));
        })));
        let deadline_at = Instant::now() + Duration::from_millis(250);
        let root = store.root.clone();
        let expected = record.clone();
        let error = run_remote_blocking_until(
            "remote-trust-forget-admitted-slow",
            deadline_at,
            BlockingWorkKind::Mutation,
            move |admission| {
                RemoteTrustStore { root }.forget_trusted_host_blocking(
                    &expected,
                    Instant::now() + Duration::from_secs(5),
                    &admission,
                )
            },
        )
        .await
        .expect_err("uncertain");
        assert_eq!(error, RemoteTrustError::PersistenceUncertain);
        let _ = release.send(());
        set_persist_after_admit_seam(None);
        let mut gone = false;
        for _ in 0..50 {
            if !store.host_path(host_id).exists() {
                gone = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(gone, "admitted forget must remain owned and settle");
    }
}
