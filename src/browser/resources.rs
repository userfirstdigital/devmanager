use super::{BrowserError, BrowserResourceId, BrowserWorkspaceKey};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{SystemTime, UNIX_EPOCH};

const RESOURCE_PREFIX: &str = "res-";
const RESOURCE_HEX_LEN: usize = 32;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum BrowserResourceKind {
    DomSnapshot,
    Screenshot,
    AnnotationScreenshot,
    AnnotationDetails,
    NetworkBody,
    CdpResult,
    PerformanceTrace,
    ConsoleLog,
    NetworkLog,
    WorkflowReview,
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

#[derive(Debug)]
struct BrowserResourceStoreInner {
    root: PathBuf,
    trusted_root: Option<PathBuf>,
    limits: BrowserResourceLimits,
    gate: Mutex<()>,
    last_created_at: AtomicU64,
}

#[derive(Debug, Clone)]
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
                root,
                trusted_root: None,
                limits,
                gate: Mutex::new(()),
                last_created_at: AtomicU64::new(max_created),
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
                root,
                trusted_root: Some(trusted_root),
                limits,
                gate: Mutex::new(()),
                last_created_at: AtomicU64::new(max_created),
            }),
        })
    }

    pub fn root(&self) -> &Path {
        &self.inner.root
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
        if byte_size > self.inner.limits.max_resource_bytes {
            return Err(BrowserError::ResourceTooLarge {
                byte_size,
                limit: self.inner.limits.max_resource_bytes,
            });
        }
        let _gate = lock(&self.inner.gate);
        self.verify_root()?;
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
        let data_path = data_path(&self.inner.root, &id)?;
        let metadata_path = metadata_path(&self.inner.root, &id)?;
        std::fs::write(&data_path, bytes)
            .map_err(|error| io_error("write resource bytes", &data_path, error))?;
        let encoded = serde_json::to_vec_pretty(&metadata).map_err(|error| BrowserError::Io {
            operation: "encode resource metadata".to_string(),
            path: metadata_path.clone(),
            message: error.to_string(),
        })?;
        if let Err(error) = std::fs::write(&metadata_path, encoded) {
            let _ = std::fs::remove_file(&data_path);
            return Err(io_error("write resource metadata", &metadata_path, error));
        }
        self.cleanup_locked()?;
        Ok(BrowserResourceHandle::from(&metadata))
    }

    pub fn list(
        &self,
        owner: &BrowserWorkspaceKey,
    ) -> Result<Vec<BrowserResourceHandle>, BrowserError> {
        let _gate = lock(&self.inner.gate);
        self.verify_root()?;
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
        Ok(resources.iter().map(BrowserResourceHandle::from).collect())
    }

    pub fn read(
        &self,
        owner: &BrowserWorkspaceKey,
        id: &BrowserResourceId,
    ) -> Result<BrowserResource, BrowserError> {
        validate_resource_id(id)?;
        let _gate = lock(&self.inner.gate);
        self.verify_root()?;
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
        Ok(BrowserResource { metadata, bytes })
    }

    pub fn handle(
        &self,
        owner: &BrowserWorkspaceKey,
        id: &BrowserResourceId,
    ) -> Result<BrowserResourceHandle, BrowserError> {
        validate_resource_id(id)?;
        let _gate = lock(&self.inner.gate);
        self.verify_root()?;
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
        Ok(BrowserResourceHandle::from(&metadata))
    }

    pub fn set_pinned(
        &self,
        owner: &BrowserWorkspaceKey,
        id: &BrowserResourceId,
        pinned: bool,
    ) -> Result<BrowserResourceHandle, BrowserError> {
        validate_resource_id(id)?;
        let _gate = lock(&self.inner.gate);
        self.verify_root()?;
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
        metadata.pinned = pinned;
        write_metadata(&metadata_path, &metadata)?;
        Ok(BrowserResourceHandle::from(&metadata))
    }

    pub fn reconcile_annotation_pins(
        &self,
        owner: &BrowserWorkspaceKey,
        pinned_ids: &BTreeSet<BrowserResourceId>,
    ) -> Result<(), BrowserError> {
        let _gate = lock(&self.inner.gate);
        self.verify_root()?;
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
        Ok(())
    }

    fn verify_root(&self) -> Result<(), BrowserError> {
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
        let previous = self.inner.last_created_at.load(Ordering::Acquire);
        let next = now.max(previous.saturating_add(1));
        self.inner.last_created_at.store(next, Ordering::Release);
        next
    }

    fn cleanup_locked(&self) -> Result<(), BrowserError> {
        let mut temporary: Vec<_> = scan_metadata(&self.inner.root)
            .into_iter()
            .filter(|metadata| !metadata.pinned)
            .collect();
        temporary.sort_by(|left, right| {
            (left.created_at_epoch_ms, &left.id.0).cmp(&(right.created_at_epoch_ms, &right.id.0))
        });
        let mut count = temporary.len();
        let mut bytes = temporary
            .iter()
            .fold(0_u64, |total, item| total.saturating_add(item.byte_size));
        for metadata in temporary {
            if count <= self.inner.limits.max_temporary_count
                && bytes <= self.inner.limits.max_temporary_bytes
            {
                break;
            }
            let metadata_path = metadata_path(&self.inner.root, &metadata.id)?;
            let data_path = data_path(&self.inner.root, &metadata.id)?;
            remove_direct_regular_file(&self.inner.root, &metadata_path)?;
            remove_direct_regular_file(&self.inner.root, &data_path)?;
            count = count.saturating_sub(1);
            bytes = bytes.saturating_sub(metadata.byte_size);
        }
        Ok(())
    }
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
