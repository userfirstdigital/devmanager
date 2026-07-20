use super::replay_repair::{
    BrowserReplayRepairRetentionAuthority, BrowserReplayRepairRetentionAuthorityKey,
};
use super::{BrowserError, BrowserResourceId, BrowserWorkspaceKey};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::fmt::Write as _;
use std::fs::File;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, Weak};
use std::time::{SystemTime, UNIX_EPOCH};

const RESOURCE_PREFIX: &str = "res-";
const RESOURCE_HEX_LEN: usize = 32;
const ROOT_LOCK_FILE: &str = ".devmanager-browser-resources.lock";
const MAX_PENDING_CLEANUP_RETRIES: usize = 32;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[serde(rename_all = "camelCase")]
pub enum BrowserResourceKind {
    DomSnapshot,
    Screenshot,
    ReplayRepairSnapshot,
    ReplayRepairScreenshot,
    AnnotationScreenshot,
    AnnotationDetails,
    NetworkBody,
    CdpResult,
    PerformanceTrace,
    ConsoleLog,
    NetworkLog,
    WorkflowReview,
    WorkflowRecipe,
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BrowserResourceLimits {
    pub max_temporary_count: usize,
    pub max_temporary_bytes: u64,
    pub max_resource_bytes: u64,
}

impl Default for BrowserResourceLimits {
    fn default() -> Self {
        Self {
            max_temporary_count: 128,
            max_temporary_bytes: 64 * 1024 * 1024,
            max_resource_bytes: 16 * 1024 * 1024,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct BrowserResourceMetadata {
    pub id: BrowserResourceId,
    pub owner: BrowserWorkspaceKey,
    pub mime_type: String,
    pub kind: BrowserResourceKind,
    pub byte_size: u64,
    pub created_at_epoch_ms: u64,
    pub pinned: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct BrowserResourceHandle {
    pub id: BrowserResourceId,
    pub uri: String,
    pub mime_type: String,
    pub kind: BrowserResourceKind,
    pub byte_size: u64,
    pub created_at_epoch_ms: u64,
    pub pinned: bool,
}

impl From<&BrowserResourceMetadata> for BrowserResourceHandle {
    fn from(metadata: &BrowserResourceMetadata) -> Self {
        Self {
            id: metadata.id.clone(),
            uri: resource_uri(&metadata.id),
            mime_type: metadata.mime_type.clone(),
            kind: metadata.kind,
            byte_size: metadata.byte_size,
            created_at_epoch_ms: metadata.created_at_epoch_ms,
            pinned: metadata.pinned,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrowserResource {
    pub metadata: BrowserResourceMetadata,
    pub bytes: Vec<u8>,
}

struct BrowserResourceRootRuntime {
    root: PathBuf,
    limits: BrowserResourceLimits,
    gate: Mutex<BrowserResourceRuntimeState>,
    last_created_at: AtomicU64,
    _lock_file: File,
}

#[derive(Default)]
struct BrowserResourceRuntimeState {
    // Task 2's domain slice will consume this private retention seam and remove the allowances.
    #[cfg_attr(not(test), allow(dead_code))]
    next_lease_id: u64,
    #[cfg_attr(not(test), allow(dead_code))]
    leases: HashMap<u64, BrowserRepairRetentionRecord>,
    #[cfg_attr(not(test), allow(dead_code))]
    active_authorities: HashSet<BrowserReplayRepairRetentionAuthorityKey>,
    retained_ids: HashSet<BrowserResourceId>,
    pending_cleanup_retries: usize,
}

#[cfg_attr(not(test), allow(dead_code))]
struct BrowserRepairRetentionRecord {
    authority: BrowserReplayRepairRetentionAuthorityKey,
    resource_ids: BTreeSet<BrowserResourceId>,
    kinds: BTreeSet<BrowserResourceKind>,
}

#[cfg_attr(not(test), allow(dead_code))]
pub(crate) struct BrowserReplayRepairRetentionLease {
    store: Arc<BrowserResourceStoreInner>,
    lease_id: u64,
    authority: BrowserReplayRepairRetentionAuthorityKey,
}

struct BrowserResourceStoreInner {
    root: PathBuf,
    trusted_root: Option<PathBuf>,
    runtime: Arc<BrowserResourceRootRuntime>,
}

#[derive(Clone)]
pub struct BrowserResourceStore {
    inner: Arc<BrowserResourceStoreInner>,
}

impl BrowserResourceStore {
    pub fn open(
        root: impl AsRef<Path>,
        limits: BrowserResourceLimits,
    ) -> Result<Self, BrowserError> {
        let root = root.as_ref();
        if root.exists() {
            let metadata = std::fs::symlink_metadata(root)
                .map_err(|error| io_error("inspect resource root", root, error))?;
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                return Err(BrowserError::OutsideWorkspace {
                    path: root.to_path_buf(),
                });
            }
        } else {
            std::fs::create_dir_all(root)
                .map_err(|error| io_error("create resource root", root, error))?;
        }
        let root = root
            .canonicalize()
            .map_err(|error| io_error("canonicalize resource root", root, error))?;
        let max_created = scan_metadata(&root)
            .into_iter()
            .map(|metadata| metadata.created_at_epoch_ms)
            .max()
            .unwrap_or_default();
        Ok(Self {
            inner: Arc::new(BrowserResourceStoreInner {
                runtime: root_runtime(&root, limits, max_created)?,
                root,
                trusted_root: None,
            }),
        })
    }

    pub fn open_verified(
        app_config_dir: impl AsRef<Path>,
        project_id: impl AsRef<str>,
        limits: BrowserResourceLimits,
    ) -> Result<Self, BrowserError> {
        let (trusted_root, root) = super::downloads::prepare_verified_resource_root(
            app_config_dir.as_ref(),
            project_id.as_ref(),
        )?;
        let max_created = scan_metadata(&root)
            .into_iter()
            .map(|metadata| metadata.created_at_epoch_ms)
            .max()
            .unwrap_or_default();
        Ok(Self {
            inner: Arc::new(BrowserResourceStoreInner {
                runtime: root_runtime(&root, limits, max_created)?,
                root,
                trusted_root: Some(trusted_root),
            }),
        })
    }

    pub fn root(&self) -> &Path {
        &self.inner.root
    }

    pub(crate) fn same_runtime(&self, other: &Self) -> bool {
        self.inner.root == other.inner.root
            && Arc::ptr_eq(&self.inner.runtime, &other.inner.runtime)
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn begin_repair_retention(
        &self,
        authority: &BrowserReplayRepairRetentionAuthority,
    ) -> Result<BrowserReplayRepairRetentionLease, BrowserError> {
        #[cfg(not(target_os = "windows"))]
        {
            let _ = authority;
            return Err(BrowserError::UnavailablePlatform {
                platform: std::env::consts::OS.to_string(),
            });
        }

        #[cfg(target_os = "windows")]
        {
            self.verify_root()?;
            let mut state = lock(&self.inner.runtime.gate);
            self.retry_pending_cleanup_locked(&mut state);
            let authority = authority.key();
            if state.active_authorities.contains(&authority) {
                return Err(BrowserError::BlockedPermission {
                    permission: "repair retention".to_string(),
                });
            }
            let lease_id = state.next_lease_id.checked_add(1).ok_or_else(|| {
                BrowserError::InvalidInvocation {
                    field: "repairLease".to_string(),
                }
            })?;
            state.next_lease_id = lease_id;
            state.leases.insert(
                lease_id,
                BrowserRepairRetentionRecord {
                    authority: authority.clone(),
                    resource_ids: BTreeSet::new(),
                    kinds: BTreeSet::new(),
                },
            );
            state.active_authorities.insert(authority.clone());
            Ok(BrowserReplayRepairRetentionLease {
                store: Arc::clone(&self.inner),
                lease_id,
                authority,
            })
        }
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn put_repair_retained(
        &self,
        lease: &mut BrowserReplayRepairRetentionLease,
        kind: BrowserResourceKind,
        mime_type: impl Into<String>,
        bytes: impl AsRef<[u8]>,
    ) -> Result<BrowserResourceHandle, BrowserError> {
        if !is_repair_resource_kind(kind) {
            return Err(BrowserError::InvalidInvocation {
                field: "resourceKind".to_string(),
            });
        }
        if lease.store.root != self.inner.root
            || !Arc::ptr_eq(&lease.store.runtime, &self.inner.runtime)
        {
            return Err(BrowserError::BlockedPermission {
                permission: "repair retention".to_string(),
            });
        }
        let mime_type = mime_type.into();
        if mime_type.trim().is_empty() {
            return Err(BrowserError::InvalidInvocation {
                field: "mimeType".to_string(),
            });
        }
        let bytes = bytes.as_ref();
        let byte_size = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
        if byte_size > self.inner.runtime.limits.max_resource_bytes {
            return Err(BrowserError::ResourceTooLarge {
                byte_size,
                limit: self.inner.runtime.limits.max_resource_bytes,
            });
        }

        self.verify_root()?;
        let mut state = lock(&self.inner.runtime.gate);
        self.retry_pending_cleanup_locked(&mut state);
        let Some(record) = state.leases.get(&lease.lease_id) else {
            return Err(BrowserError::BlockedPermission {
                permission: "repair retention".to_string(),
            });
        };
        if record.authority != lease.authority || record.kinds.contains(&kind) {
            return Err(BrowserError::BlockedPermission {
                permission: "repair retention".to_string(),
            });
        }

        let id = generate_resource_id()?;
        let created_at_epoch_ms = self.next_created_at();
        let metadata = BrowserResourceMetadata {
            id: id.clone(),
            owner: lease.authority.owner().clone(),
            mime_type,
            kind,
            byte_size,
            created_at_epoch_ms,
            pinned: false,
        };
        self.write_resource_locked(&metadata, bytes)?;
        let record = state
            .leases
            .get_mut(&lease.lease_id)
            .expect("repair lease was checked while the root gate was held");
        record.kinds.insert(kind);
        record.resource_ids.insert(id.clone());
        state.retained_ids.insert(id);
        let cleanup = self.cleanup_locked(&mut state);
        let mut handle = BrowserResourceHandle::from(&metadata);
        handle.pinned = true;
        cleanup?;
        Ok(handle)
    }

    pub fn put(
        &self,
        owner: &BrowserWorkspaceKey,
        kind: BrowserResourceKind,
        mime_type: impl Into<String>,
        bytes: impl AsRef<[u8]>,
        pinned: bool,
    ) -> Result<BrowserResourceHandle, BrowserError> {
        let mime_type = mime_type.into();
        if mime_type.trim().is_empty() {
            return Err(BrowserError::InvalidInvocation {
                field: "mimeType".to_string(),
            });
        }
        let bytes = bytes.as_ref();
        let byte_size = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
        if byte_size > self.inner.runtime.limits.max_resource_bytes {
            return Err(BrowserError::ResourceTooLarge {
                byte_size,
                limit: self.inner.runtime.limits.max_resource_bytes,
            });
        }
        let mut state = lock(&self.inner.runtime.gate);
        self.verify_root()?;
        self.retry_pending_cleanup_locked(&mut state);
        if is_repair_resource_kind(kind) {
            return Err(BrowserError::InvalidInvocation {
                field: "resourceKind".to_string(),
            });
        }
        let id = generate_resource_id()?;
        let created_at_epoch_ms = self.next_created_at();
        let metadata = BrowserResourceMetadata {
            id: id.clone(),
            owner: owner.clone(),
            mime_type,
            kind,
            byte_size,
            created_at_epoch_ms,
            pinned,
        };
        self.write_resource_locked(&metadata, bytes)?;
        self.cleanup_locked(&mut state)?;
        Ok(BrowserResourceHandle::from(&metadata))
    }

    pub fn list(
        &self,
        owner: &BrowserWorkspaceKey,
    ) -> Result<Vec<BrowserResourceHandle>, BrowserError> {
        let mut state = lock(&self.inner.runtime.gate);
        self.verify_root()?;
        self.retry_pending_cleanup_locked(&mut state);
        let mut resources: Vec<_> = scan_metadata(&self.inner.root)
            .into_iter()
            .filter(|metadata| &metadata.owner == owner)
            .filter(|metadata| {
                data_path(&self.inner.root, &metadata.id)
                    .ok()
                    .is_some_and(|path| is_direct_regular_file(&self.inner.root, &path))
            })
            .collect();
        resources.sort_by(|left, right| {
            (left.created_at_epoch_ms, &left.id.0).cmp(&(right.created_at_epoch_ms, &right.id.0))
        });
        let handles = resources
            .iter()
            .map(|metadata| self.effective_handle(&state, metadata))
            .collect();
        self.retry_pending_cleanup_locked(&mut state);
        Ok(handles)
    }

    pub fn read(
        &self,
        owner: &BrowserWorkspaceKey,
        id: &BrowserResourceId,
    ) -> Result<BrowserResource, BrowserError> {
        validate_resource_id(id)?;
        let mut state = lock(&self.inner.runtime.gate);
        self.verify_root()?;
        self.retry_pending_cleanup_locked(&mut state);
        let metadata_path = metadata_path(&self.inner.root, id)?;
        if !is_direct_regular_file(&self.inner.root, &metadata_path) {
            return Err(BrowserError::MissingResource { id: id.clone() });
        }
        let encoded = std::fs::read(&metadata_path)
            .map_err(|error| io_error("read resource metadata", &metadata_path, error))?;
        let metadata: BrowserResourceMetadata = serde_json::from_slice(&encoded)
            .map_err(|_| BrowserError::MissingResource { id: id.clone() })?;
        if metadata.id != *id {
            return Err(BrowserError::MissingResource { id: id.clone() });
        }
        if &metadata.owner != owner {
            return Err(BrowserError::BlockedPermission {
                permission: "resource ownership".to_string(),
            });
        }
        let data_path = data_path(&self.inner.root, id)?;
        if !is_direct_regular_file(&self.inner.root, &data_path) {
            return Err(BrowserError::MissingResource { id: id.clone() });
        }
        let bytes = std::fs::read(&data_path)
            .map_err(|error| io_error("read resource bytes", &data_path, error))?;
        if u64::try_from(bytes.len()).unwrap_or(u64::MAX) != metadata.byte_size {
            return Err(BrowserError::MissingResource { id: id.clone() });
        }
        let mut metadata = metadata;
        metadata.pinned |= state.retained_ids.contains(&metadata.id);
        self.retry_pending_cleanup_locked(&mut state);
        Ok(BrowserResource { metadata, bytes })
    }

    pub fn handle(
        &self,
        owner: &BrowserWorkspaceKey,
        id: &BrowserResourceId,
    ) -> Result<BrowserResourceHandle, BrowserError> {
        validate_resource_id(id)?;
        let mut state = lock(&self.inner.runtime.gate);
        self.verify_root()?;
        self.retry_pending_cleanup_locked(&mut state);
        let metadata_path = metadata_path(&self.inner.root, id)?;
        if !is_direct_regular_file(&self.inner.root, &metadata_path) {
            return Err(BrowserError::MissingResource { id: id.clone() });
        }
        let encoded = std::fs::read(&metadata_path)
            .map_err(|error| io_error("read resource metadata", &metadata_path, error))?;
        let metadata: BrowserResourceMetadata = serde_json::from_slice(&encoded)
            .map_err(|_| BrowserError::MissingResource { id: id.clone() })?;
        if metadata.id != *id || &metadata.owner != owner {
            return Err(BrowserError::MissingResource { id: id.clone() });
        }
        let data_path = data_path(&self.inner.root, id)?;
        if !is_direct_regular_file(&self.inner.root, &data_path) {
            return Err(BrowserError::MissingResource { id: id.clone() });
        }
        let actual_size = std::fs::metadata(&data_path)
            .map_err(|_| BrowserError::MissingResource { id: id.clone() })?
            .len();
        if actual_size != metadata.byte_size {
            return Err(BrowserError::MissingResource { id: id.clone() });
        }
        let handle = self.effective_handle(&state, &metadata);
        self.retry_pending_cleanup_locked(&mut state);
        Ok(handle)
    }

    pub fn set_pinned(
        &self,
        owner: &BrowserWorkspaceKey,
        id: &BrowserResourceId,
        pinned: bool,
    ) -> Result<BrowserResourceHandle, BrowserError> {
        validate_resource_id(id)?;
        let mut state = lock(&self.inner.runtime.gate);
        self.verify_root()?;
        self.retry_pending_cleanup_locked(&mut state);
        let metadata_path = metadata_path(&self.inner.root, id)?;
        if !is_direct_regular_file(&self.inner.root, &metadata_path) {
            return Err(BrowserError::MissingResource { id: id.clone() });
        }
        let encoded = std::fs::read(&metadata_path)
            .map_err(|error| io_error("read resource metadata", &metadata_path, error))?;
        let mut metadata: BrowserResourceMetadata = serde_json::from_slice(&encoded)
            .map_err(|_| BrowserError::MissingResource { id: id.clone() })?;
        if metadata.id != *id {
            return Err(BrowserError::MissingResource { id: id.clone() });
        }
        if &metadata.owner != owner {
            return Err(BrowserError::BlockedPermission {
                permission: "resource ownership".to_string(),
            });
        }
        if is_repair_resource_kind(metadata.kind) {
            return Err(BrowserError::BlockedPermission {
                permission: "repair retention".to_string(),
            });
        }
        metadata.pinned = pinned;
        write_metadata(&metadata_path, &metadata)?;
        self.retry_pending_cleanup_locked(&mut state);
        Ok(BrowserResourceHandle::from(&metadata))
    }

    pub fn reconcile_annotation_pins(
        &self,
        owner: &BrowserWorkspaceKey,
        pinned_ids: &BTreeSet<BrowserResourceId>,
    ) -> Result<(), BrowserError> {
        let mut state = lock(&self.inner.runtime.gate);
        self.verify_root()?;
        self.retry_pending_cleanup_locked(&mut state);
        for mut metadata in scan_metadata(&self.inner.root)
            .into_iter()
            .filter(|metadata| {
                &metadata.owner == owner
                    && matches!(
                        metadata.kind,
                        BrowserResourceKind::AnnotationScreenshot
                            | BrowserResourceKind::AnnotationDetails
                    )
            })
        {
            let pinned = pinned_ids.contains(&metadata.id);
            if metadata.pinned == pinned {
                continue;
            }
            metadata.pinned = pinned;
            let path = metadata_path(&self.inner.root, &metadata.id)?;
            write_metadata(&path, &metadata)?;
        }
        self.retry_pending_cleanup_locked(&mut state);
        Ok(())
    }

    fn verify_root(&self) -> Result<(), BrowserError> {
        if self.inner.runtime.root != self.inner.root {
            return Err(BrowserError::OutsideWorkspace {
                path: self.inner.root.clone(),
            });
        }
        if let Some(trusted_root) = &self.inner.trusted_root {
            super::downloads::verify_prepared_storage_root(trusted_root, &self.inner.root)?;
        }
        Ok(())
    }

    fn next_created_at(&self) -> u64 {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
            .min(u128::from(u64::MAX)) as u64;
        let previous = self.inner.runtime.last_created_at.load(Ordering::Acquire);
        let next = now.max(previous.saturating_add(1));
        self.inner
            .runtime
            .last_created_at
            .store(next, Ordering::Release);
        next
    }

    fn write_resource_locked(
        &self,
        metadata: &BrowserResourceMetadata,
        bytes: &[u8],
    ) -> Result<(), BrowserError> {
        self.write_resource_locked_with(metadata, bytes, write_metadata_create_new)
    }

    fn write_resource_locked_with(
        &self,
        metadata: &BrowserResourceMetadata,
        bytes: &[u8],
        write_metadata_file: impl FnOnce(
            &Path,
            &BrowserResourceMetadata,
        ) -> Result<(), (BrowserError, bool)>,
    ) -> Result<(), BrowserError> {
        let data_path = data_path(&self.inner.root, &metadata.id)?;
        let metadata_path = metadata_path(&self.inner.root, &metadata.id)?;
        let mut data = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&data_path)
            .map_err(|error| io_error("create resource bytes", &data_path, error))?;
        if let Err(error) = data.write_all(bytes) {
            let _ = std::fs::remove_file(&data_path);
            return Err(io_error("write resource bytes", &data_path, error));
        }
        drop(data);
        if let Err((error, metadata_created)) = write_metadata_file(&metadata_path, metadata) {
            let _ = remove_direct_regular_file(&self.inner.root, &data_path);
            if metadata_created {
                let _ = remove_direct_regular_file(&self.inner.root, &metadata_path);
            }
            return Err(error);
        }
        Ok(())
    }

    fn effective_handle(
        &self,
        state: &BrowserResourceRuntimeState,
        metadata: &BrowserResourceMetadata,
    ) -> BrowserResourceHandle {
        let mut handle = BrowserResourceHandle::from(metadata);
        handle.pinned |= state.retained_ids.contains(&metadata.id);
        handle
    }

    fn retry_pending_cleanup_locked(&self, state: &mut BrowserResourceRuntimeState) {
        if state.pending_cleanup_retries == 0 {
            return;
        }
        let _ = self.cleanup_locked(state);
    }

    fn cleanup_locked(&self, state: &mut BrowserResourceRuntimeState) -> Result<(), BrowserError> {
        let mut temporary: Vec<_> = scan_metadata(&self.inner.root)
            .into_iter()
            .filter(|metadata| !metadata.pinned && !state.retained_ids.contains(&metadata.id))
            .collect();
        temporary.sort_by(|left, right| {
            (left.created_at_epoch_ms, &left.id.0).cmp(&(right.created_at_epoch_ms, &right.id.0))
        });
        let mut count = temporary.len();
        let mut bytes = temporary
            .iter()
            .fold(0_u64, |total, item| total.saturating_add(item.byte_size));
        for metadata in temporary {
            if count <= self.inner.runtime.limits.max_temporary_count
                && bytes <= self.inner.runtime.limits.max_temporary_bytes
            {
                break;
            }
            let metadata_path = metadata_path(&self.inner.root, &metadata.id)?;
            let data_path = data_path(&self.inner.root, &metadata.id)?;
            if let Err(error) = remove_direct_regular_file(&self.inner.root, &data_path)
                .and_then(|_| remove_direct_regular_file(&self.inner.root, &metadata_path))
            {
                state.pending_cleanup_retries = state
                    .pending_cleanup_retries
                    .saturating_add(1)
                    .min(MAX_PENDING_CLEANUP_RETRIES);
                return Err(error);
            }
            count = count.saturating_sub(1);
            bytes = bytes.saturating_sub(metadata.byte_size);
        }
        state.pending_cleanup_retries = 0;
        Ok(())
    }
}

impl Drop for BrowserReplayRepairRetentionLease {
    fn drop(&mut self) {
        let mut state = lock(&self.store.runtime.gate);
        let Some(record) = state.leases.remove(&self.lease_id) else {
            return;
        };
        state.active_authorities.remove(&record.authority);
        for id in record.resource_ids {
            state.retained_ids.remove(&id);
        }
        let store = BrowserResourceStore {
            inner: Arc::clone(&self.store),
        };
        let _ = store.cleanup_locked(&mut state);
    }
}

fn root_runtimes() -> &'static Mutex<HashMap<PathBuf, Weak<BrowserResourceRootRuntime>>> {
    static RUNTIMES: OnceLock<Mutex<HashMap<PathBuf, Weak<BrowserResourceRootRuntime>>>> =
        OnceLock::new();
    RUNTIMES.get_or_init(|| Mutex::new(HashMap::new()))
}

fn root_runtime(
    root: &Path,
    limits: BrowserResourceLimits,
    max_created_at: u64,
) -> Result<Arc<BrowserResourceRootRuntime>, BrowserError> {
    let mut runtimes = lock(root_runtimes());
    if let Some(runtime) = runtimes.get(root).and_then(Weak::upgrade) {
        if runtime.limits != limits {
            return Err(BrowserError::ResourceRootUnavailable);
        }
        return Ok(runtime);
    }
    let stale_handoff = runtimes.remove(root).is_some();
    runtimes.retain(|_, runtime| runtime.strong_count() > 0);
    let lock_file = open_root_lock_file_after_stale_handoff(root, stale_handoff)?;
    let runtime = Arc::new(BrowserResourceRootRuntime {
        root: root.to_path_buf(),
        limits,
        gate: Mutex::new(BrowserResourceRuntimeState::default()),
        last_created_at: AtomicU64::new(max_created_at),
        _lock_file: lock_file,
    });
    runtimes.insert(root.to_path_buf(), Arc::downgrade(&runtime));
    Ok(runtime)
}

fn open_root_lock_file_after_stale_handoff(
    root: &Path,
    stale_handoff: bool,
) -> Result<File, BrowserError> {
    const STALE_HANDOFF_ATTEMPTS: usize = 4;
    let attempts = if stale_handoff {
        STALE_HANDOFF_ATTEMPTS
    } else {
        1
    };
    let mut result = open_root_lock_file(root);
    for _ in 1..attempts {
        if !matches!(result, Err(BrowserError::ResourceRootBusy)) {
            break;
        }
        std::thread::yield_now();
        result = open_root_lock_file(root);
    }
    result
}

fn open_root_lock_file(root: &Path) -> Result<File, BrowserError> {
    let path = root.join(ROOT_LOCK_FILE);
    if path.exists() && !is_direct_regular_file(root, &path) {
        return Err(BrowserError::ResourceRootUnavailable);
    }

    #[cfg(target_os = "windows")]
    let (result, expected_identity) = {
        use std::os::windows::fs::OpenOptionsExt;
        let create = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .share_mode(0)
            .custom_flags(windows::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT.0)
            .open(&path);
        match create {
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                let shared = std::fs::OpenOptions::new()
                    .read(true)
                    .share_mode(
                        windows::Win32::Storage::FileSystem::FILE_SHARE_READ.0
                            | windows::Win32::Storage::FileSystem::FILE_SHARE_WRITE.0
                            | windows::Win32::Storage::FileSystem::FILE_SHARE_DELETE.0,
                    )
                    .custom_flags(
                        windows::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT.0,
                    )
                    .open(&path);
                match shared {
                    Ok(shared) => match windows_file_information(&shared) {
                        Ok(expected) => {
                            drop(shared);
                            (
                                std::fs::OpenOptions::new()
                                    .read(true)
                                    .write(true)
                                    .share_mode(0)
                                    .custom_flags(
                                        windows::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT.0,
                                    )
                                    .open(&path),
                                Some(expected),
                            )
                        }
                        Err(_) => (
                            Err(std::io::Error::from(std::io::ErrorKind::InvalidData)),
                            None,
                        ),
                    },
                    Err(error) => (Err(error), None),
                }
            }
            result => (result, None),
        }
    };

    #[cfg(not(target_os = "windows"))]
    let result = match std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(&path)
    {
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&path)
        }
        result => result,
    };

    #[cfg(not(target_os = "windows"))]
    let expected_identity = ();

    let file = result.map_err(|error| {
        #[cfg(target_os = "windows")]
        if matches!(error.raw_os_error(), Some(32 | 33)) {
            return BrowserError::ResourceRootBusy;
        }
        BrowserError::ResourceRootUnavailable
    })?;
    validate_opened_root_lock(root, &path, &file, expected_identity)?;
    Ok(file)
}

#[cfg(target_os = "windows")]
fn windows_file_information(
    file: &File,
) -> Result<windows::Win32::Storage::FileSystem::BY_HANDLE_FILE_INFORMATION, BrowserError> {
    use std::os::windows::io::AsRawHandle;
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::Storage::FileSystem::{
        GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION,
    };

    let mut information = BY_HANDLE_FILE_INFORMATION::default();
    let handle = HANDLE(file.as_raw_handle());
    unsafe { GetFileInformationByHandle(handle, &mut information) }
        .map_err(|_| BrowserError::ResourceRootUnavailable)?;
    Ok(information)
}

fn validate_opened_root_lock(
    root: &Path,
    path: &Path,
    file: &File,
    #[cfg(target_os = "windows")] expected_identity: Option<
        windows::Win32::Storage::FileSystem::BY_HANDLE_FILE_INFORMATION,
    >,
    #[cfg(not(target_os = "windows"))] _expected_identity: (),
) -> Result<(), BrowserError> {
    if !is_direct_regular_file(root, path) {
        return Err(BrowserError::ResourceRootUnavailable);
    }

    #[cfg(target_os = "windows")]
    {
        let opened = windows_file_information(file)?;
        let path_metadata =
            std::fs::symlink_metadata(path).map_err(|_| BrowserError::ResourceRootUnavailable)?;
        let reparse = windows::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT.0;
        use std::os::windows::fs::MetadataExt;
        if opened.dwFileAttributes & reparse != 0
            || path_metadata.file_attributes() & reparse != 0
            || opened.nNumberOfLinks != 1
            || expected_identity.is_some_and(|expected| {
                expected.dwVolumeSerialNumber != opened.dwVolumeSerialNumber
                    || expected.nFileIndexHigh != opened.nFileIndexHigh
                    || expected.nFileIndexLow != opened.nFileIndexLow
                    || expected.nNumberOfLinks != opened.nNumberOfLinks
            })
        {
            return Err(BrowserError::ResourceRootUnavailable);
        }
    }

    #[cfg(not(target_os = "windows"))]
    let _ = file;
    Ok(())
}

fn is_repair_resource_kind(kind: BrowserResourceKind) -> bool {
    matches!(
        kind,
        BrowserResourceKind::ReplayRepairSnapshot | BrowserResourceKind::ReplayRepairScreenshot
    )
}

pub fn resource_uri(id: &BrowserResourceId) -> String {
    format!("devmanager-browser://resource/{}", id.0)
}

pub fn resource_id_from_uri(uri: &str) -> Result<BrowserResourceId, BrowserError> {
    let id = uri
        .strip_prefix("devmanager-browser://resource/")
        .ok_or_else(|| BrowserError::BlockedPermission {
            permission: "resource URI".to_string(),
        })?;
    let id = BrowserResourceId(id.to_string());
    validate_resource_id(&id)?;
    Ok(id)
}

fn generate_resource_id() -> Result<BrowserResourceId, BrowserError> {
    let mut random = [0_u8; RESOURCE_HEX_LEN / 2];
    getrandom::fill(&mut random).map_err(|error| BrowserError::CrashedView {
        message: format!("could not generate browser resource id: {error}"),
    })?;
    let mut id = String::with_capacity(RESOURCE_PREFIX.len() + RESOURCE_HEX_LEN);
    id.push_str(RESOURCE_PREFIX);
    for byte in random {
        let _ = write!(id, "{byte:02x}");
    }
    Ok(BrowserResourceId(id))
}

fn validate_resource_id(id: &BrowserResourceId) -> Result<(), BrowserError> {
    let valid = id.0.len() == RESOURCE_PREFIX.len() + RESOURCE_HEX_LEN
        && id.0.starts_with(RESOURCE_PREFIX)
        && id.0[RESOURCE_PREFIX.len()..]
            .chars()
            .all(|character| character.is_ascii_hexdigit() && !character.is_ascii_uppercase());
    if valid {
        Ok(())
    } else {
        Err(BrowserError::BlockedPermission {
            permission: "resource id".to_string(),
        })
    }
}

fn metadata_path(root: &Path, id: &BrowserResourceId) -> Result<PathBuf, BrowserError> {
    validate_resource_id(id)?;
    Ok(root.join(format!("{}.json", id.0)))
}

fn data_path(root: &Path, id: &BrowserResourceId) -> Result<PathBuf, BrowserError> {
    validate_resource_id(id)?;
    Ok(root.join(format!("{}.bin", id.0)))
}

fn scan_metadata(root: &Path) -> Vec<BrowserResourceMetadata> {
    let Ok(entries) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let path = entry.path();
            let file_metadata = std::fs::symlink_metadata(&path).ok()?;
            if file_metadata.file_type().is_symlink() || !file_metadata.is_file() {
                return None;
            }
            let file_name = path.file_name()?.to_str()?;
            let id_text = file_name.strip_suffix(".json")?;
            let id = BrowserResourceId(id_text.to_string());
            validate_resource_id(&id).ok()?;
            let encoded = std::fs::read(&path).ok()?;
            let metadata: BrowserResourceMetadata = serde_json::from_slice(&encoded).ok()?;
            (metadata.id == id).then_some(metadata)
        })
        .collect()
}

fn is_direct_regular_file(root: &Path, path: &Path) -> bool {
    path.parent() == Some(root)
        && std::fs::symlink_metadata(path)
            .is_ok_and(|metadata| metadata.is_file() && !metadata.file_type().is_symlink())
}

fn remove_direct_regular_file(root: &Path, path: &Path) -> Result<(), BrowserError> {
    if !path.exists() {
        return Ok(());
    }
    if !is_direct_regular_file(root, path) {
        return Err(BrowserError::OutsideWorkspace {
            path: path.to_path_buf(),
        });
    }
    std::fs::remove_file(path).map_err(|error| io_error("clean resource", path, error))
}

fn io_error(operation: &str, path: &Path, error: std::io::Error) -> BrowserError {
    BrowserError::Io {
        operation: operation.to_string(),
        path: path.to_path_buf(),
        message: error.to_string(),
    }
}

fn write_metadata(path: &Path, metadata: &BrowserResourceMetadata) -> Result<(), BrowserError> {
    let encoded = serde_json::to_vec_pretty(metadata).map_err(|error| BrowserError::Io {
        operation: "encode resource metadata".to_string(),
        path: path.to_path_buf(),
        message: error.to_string(),
    })?;
    std::fs::write(path, encoded).map_err(|error| io_error("write resource metadata", path, error))
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn write_metadata_create_new(
    path: &Path,
    metadata: &BrowserResourceMetadata,
) -> Result<(), (BrowserError, bool)> {
    let encoded = serde_json::to_vec_pretty(metadata).map_err(|error| {
        (
            BrowserError::Io {
                operation: "encode resource metadata".to_string(),
                path: path.to_path_buf(),
                message: error.to_string(),
            },
            false,
        )
    })?;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|error| (io_error("create resource metadata", path, error), false))?;
    file.write_all(&encoded)
        .map_err(|error| (io_error("write resource metadata", path, error), true))
}

#[cfg(test)]
mod tests {
    use super::*;
    use static_assertions::assert_not_impl_any;
    use std::time::{SystemTime, UNIX_EPOCH};

    assert_not_impl_any!(BrowserReplayRepairRetentionAuthority: Clone, std::fmt::Debug, serde::Serialize);
    assert_not_impl_any!(BrowserReplayRepairRetentionLease: Clone, std::fmt::Debug, serde::Serialize);

    fn test_root(label: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "devmanager-browser-resource-{label}-{}-{nanos:x}",
            std::process::id()
        ))
    }

    fn test_limits(count: usize) -> BrowserResourceLimits {
        BrowserResourceLimits {
            max_temporary_count: count,
            max_temporary_bytes: 1024 * 1024,
            max_resource_bytes: 1024 * 1024,
        }
    }

    fn test_owner(label: &str) -> BrowserWorkspaceKey {
        BrowserWorkspaceKey::new("resource-authority", label).unwrap()
    }

    #[test]
    #[cfg(target_os = "windows")]
    fn exact_authority_allows_only_one_live_lease_and_carries_owner_and_scope() {
        let root = std::env::temp_dir().join(format!(
            "devmanager-browser-resource-authority-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let store = BrowserResourceStore::open(&root, BrowserResourceLimits::default()).unwrap();
        let owner = BrowserWorkspaceKey::new("authority", "exact").unwrap();
        let authority = BrowserReplayRepairRetentionAuthority::issue_for_test(owner.clone(), 41, 7);
        let mut lease = store.begin_repair_retention(&authority).unwrap();
        assert!(matches!(
            store.begin_repair_retention(&authority),
            Err(BrowserError::BlockedPermission { .. })
        ));
        let handle = store
            .put_repair_retained(
                &mut lease,
                BrowserResourceKind::ReplayRepairSnapshot,
                "application/json",
                b"{}",
            )
            .unwrap();
        assert_eq!(
            store.read(&owner, &handle.id).unwrap().metadata.owner,
            owner
        );
        drop(lease);
        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    #[cfg(target_os = "windows")]
    fn retention_is_effectively_pinned_but_persisted_unpinned_until_drop() {
        let root = test_root("effective-pin");
        let first = BrowserResourceStore::open(&root, test_limits(0)).unwrap();
        let second = BrowserResourceStore::open(&root, test_limits(0)).unwrap();
        let owner = test_owner("effective-pin");
        let authority = BrowserReplayRepairRetentionAuthority::issue_for_test(owner.clone(), 1, 1);
        let mut lease = first.begin_repair_retention(&authority).unwrap();
        let handle = second
            .put_repair_retained(
                &mut lease,
                BrowserResourceKind::ReplayRepairSnapshot,
                "application/json",
                b"{}",
            )
            .unwrap();
        assert!(handle.pinned);
        let disk: BrowserResourceMetadata = serde_json::from_slice(
            &std::fs::read(metadata_path(&root, &handle.id).unwrap()).unwrap(),
        )
        .unwrap();
        assert!(!disk.pinned);
        assert!(first.handle(&owner, &handle.id).unwrap().pinned);
        assert!(first.read(&owner, &handle.id).unwrap().metadata.pinned);
        assert!(first.list(&owner).unwrap()[0].pinned);
        drop(lease);
        assert!(matches!(
            first.handle(&owner, &handle.id),
            Err(BrowserError::MissingResource { .. })
        ));
        drop(first);
        drop(second);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    #[cfg(target_os = "windows")]
    fn repair_resources_reject_cross_root_wrong_kind_duplicates_and_manual_pins() {
        let first_root = test_root("exact-first");
        let other_root = test_root("exact-other");
        let first = BrowserResourceStore::open(&first_root, test_limits(0)).unwrap();
        let other = BrowserResourceStore::open(&other_root, test_limits(0)).unwrap();
        let owner = test_owner("exact");
        let authority = BrowserReplayRepairRetentionAuthority::issue_for_test(owner.clone(), 2, 1);
        let mut lease = first.begin_repair_retention(&authority).unwrap();
        assert!(matches!(
            other.put_repair_retained(
                &mut lease,
                BrowserResourceKind::ReplayRepairSnapshot,
                "application/json",
                b"{}",
            ),
            Err(BrowserError::BlockedPermission { .. })
        ));
        assert!(matches!(
            first.put_repair_retained(
                &mut lease,
                BrowserResourceKind::DomSnapshot,
                "application/json",
                b"{}",
            ),
            Err(BrowserError::InvalidInvocation { .. })
        ));
        assert!(matches!(
            first.put(
                &owner,
                BrowserResourceKind::ReplayRepairSnapshot,
                "application/json",
                b"{}",
                false,
            ),
            Err(BrowserError::InvalidInvocation { .. })
        ));
        let retained = first
            .put_repair_retained(
                &mut lease,
                BrowserResourceKind::ReplayRepairScreenshot,
                "image/png",
                b"png",
            )
            .unwrap();
        assert!(matches!(
            first.put_repair_retained(
                &mut lease,
                BrowserResourceKind::ReplayRepairScreenshot,
                "image/png",
                b"png",
            ),
            Err(BrowserError::BlockedPermission { .. })
        ));
        assert!(matches!(
            first.set_pinned(&owner, &retained.id, false),
            Err(BrowserError::BlockedPermission { .. })
        ));
        drop(lease);
        drop(first);
        drop(other);
        std::fs::remove_dir_all(first_root).unwrap();
        std::fs::remove_dir_all(other_root).unwrap();
    }

    #[test]
    #[cfg(target_os = "windows")]
    fn dropping_one_lease_releases_only_its_resources() {
        let root = test_root("lease-isolation");
        let store = BrowserResourceStore::open(&root, test_limits(0)).unwrap();
        let owner = test_owner("lease-isolation");
        let first_authority =
            BrowserReplayRepairRetentionAuthority::issue_for_test(owner.clone(), 3, 1);
        let second_authority =
            BrowserReplayRepairRetentionAuthority::issue_for_test(owner.clone(), 3, 2);
        let mut first_lease = store.begin_repair_retention(&first_authority).unwrap();
        let mut second_lease = store.begin_repair_retention(&second_authority).unwrap();
        let first = store
            .put_repair_retained(
                &mut first_lease,
                BrowserResourceKind::ReplayRepairSnapshot,
                "application/json",
                b"one",
            )
            .unwrap();
        let second = store
            .put_repair_retained(
                &mut second_lease,
                BrowserResourceKind::ReplayRepairSnapshot,
                "application/json",
                b"two",
            )
            .unwrap();
        drop(first_lease);
        assert!(matches!(
            store.handle(&owner, &first.id),
            Err(BrowserError::MissingResource { .. })
        ));
        assert!(store.handle(&owner, &second.id).unwrap().pinned);
        drop(second_lease);
        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    #[cfg(target_os = "windows")]
    fn cleanup_failure_is_unpinned_immediately_and_retried_later() {
        use std::os::windows::fs::OpenOptionsExt;

        let root = test_root("cleanup-retry");
        let store = BrowserResourceStore::open(&root, test_limits(0)).unwrap();
        let owner = test_owner("cleanup-retry");
        let authority = BrowserReplayRepairRetentionAuthority::issue_for_test(owner.clone(), 4, 1);
        let mut lease = store.begin_repair_retention(&authority).unwrap();
        let retained = store
            .put_repair_retained(
                &mut lease,
                BrowserResourceKind::ReplayRepairScreenshot,
                "image/png",
                b"png",
            )
            .unwrap();
        let data_path = data_path(&root, &retained.id).unwrap();
        let blocker = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(&data_path)
            .unwrap();
        drop(lease);
        let disk: BrowserResourceMetadata = serde_json::from_slice(
            &std::fs::read(metadata_path(&root, &retained.id).unwrap()).unwrap(),
        )
        .unwrap();
        assert!(!disk.pinned);
        assert!(!store.handle(&owner, &retained.id).unwrap().pinned);
        drop(blocker);
        assert!(store.list(&owner).unwrap().is_empty());
        assert!(!data_path.exists());
        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    #[cfg(target_os = "windows")]
    fn resource_runtime_child_helper() {
        let Ok(mode) = std::env::var("DEVMANAGER_REPAIR_RESOURCE_UNIT_CHILD") else {
            return;
        };
        let root = PathBuf::from(std::env::var_os("DEVMANAGER_REPAIR_RESOURCE_UNIT_ROOT").unwrap());
        if mode == "busy" {
            assert!(matches!(
                BrowserResourceStore::open(&root, test_limits(1)),
                Err(BrowserError::ResourceRootBusy)
            ));
            return;
        }
        if mode == "available" {
            drop(BrowserResourceStore::open(&root, test_limits(1)).unwrap());
            return;
        }

        let store = BrowserResourceStore::open(&root, test_limits(1)).unwrap();
        let owner = test_owner("crash");
        let authority = BrowserReplayRepairRetentionAuthority::issue_for_test(owner, 5, 1);
        let mut lease = store.begin_repair_retention(&authority).unwrap();
        let handle = store
            .put_repair_retained(
                &mut lease,
                BrowserResourceKind::ReplayRepairSnapshot,
                "application/json",
                b"crash",
            )
            .unwrap();
        std::fs::write(root.join("child-resource-id"), handle.id.0).unwrap();
        std::process::exit(0);
    }

    #[test]
    #[cfg(target_os = "windows")]
    fn lease_alone_keeps_lock_alive_and_final_drop_releases_it() {
        let root = test_root("lease-lock");
        let store = BrowserResourceStore::open(&root, test_limits(1)).unwrap();
        let authority =
            BrowserReplayRepairRetentionAuthority::issue_for_test(test_owner("lease-lock"), 6, 1);
        let lease = store.begin_repair_retention(&authority).unwrap();
        drop(store);
        let blocked = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "browser::resources::tests::resource_runtime_child_helper",
                "--nocapture",
            ])
            .env("DEVMANAGER_REPAIR_RESOURCE_UNIT_CHILD", "busy")
            .env("DEVMANAGER_REPAIR_RESOURCE_UNIT_ROOT", &root)
            .status()
            .unwrap();
        assert!(blocked.success());
        drop(lease);
        let available = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "browser::resources::tests::resource_runtime_child_helper",
                "--nocapture",
            ])
            .env("DEVMANAGER_REPAIR_RESOURCE_UNIT_CHILD", "available")
            .env("DEVMANAGER_REPAIR_RESOURCE_UNIT_ROOT", &root)
            .status()
            .unwrap();
        assert!(available.success());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    #[cfg(target_os = "windows")]
    fn crash_reopen_observes_no_persistent_repair_pin() {
        let root = test_root("crash");
        std::fs::create_dir_all(&root).unwrap();
        let crashed = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "browser::resources::tests::resource_runtime_child_helper",
                "--nocapture",
            ])
            .env("DEVMANAGER_REPAIR_RESOURCE_UNIT_CHILD", "crash")
            .env("DEVMANAGER_REPAIR_RESOURCE_UNIT_ROOT", &root)
            .status()
            .unwrap();
        assert!(crashed.success());
        let id =
            BrowserResourceId(std::fs::read_to_string(root.join("child-resource-id")).unwrap());
        let store = BrowserResourceStore::open(&root, test_limits(1)).unwrap();
        let owner = test_owner("crash");
        assert!(!store.handle(&owner, &id).unwrap().pinned);
        let _ = store
            .put(
                &owner,
                BrowserResourceKind::DomSnapshot,
                "application/json",
                b"replacement",
                false,
            )
            .unwrap();
        assert!(matches!(
            store.handle(&owner, &id),
            Err(BrowserError::MissingResource { .. })
        ));
        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn metadata_write_failure_removes_the_already_written_resource_body() {
        let root = std::env::temp_dir().join(format!(
            "devmanager-browser-resource-partial-write-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let store = BrowserResourceStore::open(&root, BrowserResourceLimits::default()).unwrap();
        let id = BrowserResourceId(format!("{RESOURCE_PREFIX}{}", "0".repeat(RESOURCE_HEX_LEN)));
        let metadata = BrowserResourceMetadata {
            id: id.clone(),
            owner: BrowserWorkspaceKey::new("partial-write", "unit").unwrap(),
            mime_type: "application/json".to_string(),
            kind: BrowserResourceKind::DomSnapshot,
            byte_size: 2,
            created_at_epoch_ms: 1,
            pinned: false,
        };
        let metadata_path = metadata_path(&root, &id).unwrap();
        let data_path = data_path(&root, &id).unwrap();
        std::fs::create_dir(&metadata_path).unwrap();

        assert!(store.write_resource_locked(&metadata, b"{}").is_err());
        assert!(!data_path.exists());
        assert!(!is_direct_regular_file(&root, &metadata_path));

        std::fs::remove_dir(&metadata_path).unwrap();
        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn partial_regular_metadata_failure_removes_both_new_paths() {
        let root = std::env::temp_dir().join(format!(
            "devmanager-browser-resource-partial-metadata-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let store = BrowserResourceStore::open(&root, BrowserResourceLimits::default()).unwrap();
        let id = BrowserResourceId(format!("{RESOURCE_PREFIX}{}", "1".repeat(RESOURCE_HEX_LEN)));
        let metadata = BrowserResourceMetadata {
            id: id.clone(),
            owner: BrowserWorkspaceKey::new("partial-metadata", "unit").unwrap(),
            mime_type: "application/json".to_string(),
            kind: BrowserResourceKind::DomSnapshot,
            byte_size: 2,
            created_at_epoch_ms: 1,
            pinned: false,
        };
        let metadata_path = metadata_path(&root, &id).unwrap();
        let data_path = data_path(&root, &id).unwrap();

        let error = store
            .write_resource_locked_with(&metadata, b"{}", |path, _| {
                std::fs::write(path, b"{").unwrap();
                Err((
                    BrowserError::Io {
                        operation: "injected metadata failure".to_string(),
                        path: path.to_path_buf(),
                        message: "fixed failure".to_string(),
                    },
                    true,
                ))
            })
            .unwrap_err();
        assert!(matches!(error, BrowserError::Io { .. }));
        assert!(!data_path.exists());
        assert!(!metadata_path.exists());
        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }
}
