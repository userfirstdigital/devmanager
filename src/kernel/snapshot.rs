use std::fmt;

use hmac::{Hmac, Mac};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use zeroize::Zeroizing;

use crate::domain::id::{AgentSessionId, ArtifactId, ResourceId, SnapshotId, TaskId};
use crate::domain::snapshot::{
    PageLimits, PageLimitsError, SnapshotItem, SnapshotItemKey, SnapshotPage, SnapshotSection,
    TaskSnapshotItem,
};
use crate::kernel::command_bus;
use crate::kernel::store::{u64_from_nonnegative_i64, KernelStore, StoreError};

const SNAPSHOT_CURSOR_VERSION: u16 = 1;
const SNAPSHOT_CURSOR_DOMAIN: &[u8] = b"devmanager:snapshot-cursor:v1\0";
const CURSOR_TAG_BYTES: usize = 32;
const MAX_CURSOR_BYTES: usize = 4_096;

type HmacSha256 = Hmac<Sha256>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SnapshotError {
    Store(StoreError),
    InvalidLimits(PageLimitsError),
    EntropyUnavailable,
    InvalidCursor,
    CursorContextMismatch,
    UnsupportedSection,
    PageEnvelopeTooLarge {
        encoded_bytes: u32,
        max_encoded_bytes: u32,
    },
    PageItemTooLarge {
        item: SnapshotItemKey,
        encoded_bytes: u32,
        max_encoded_bytes: u32,
    },
}

impl fmt::Display for SnapshotError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Store(error) => error.fmt(f),
            Self::InvalidLimits(error) => error.fmt(f),
            Self::EntropyUnavailable => write!(f, "snapshot cursor entropy unavailable"),
            Self::InvalidCursor => write!(f, "invalid snapshot cursor"),
            Self::CursorContextMismatch => write!(f, "snapshot cursor context mismatch"),
            Self::UnsupportedSection => write!(f, "snapshot section is not implemented"),
            Self::PageEnvelopeTooLarge {
                encoded_bytes,
                max_encoded_bytes,
            } => write!(
                f,
                "snapshot page envelope is {encoded_bytes} bytes, exceeding {max_encoded_bytes}"
            ),
            Self::PageItemTooLarge {
                item,
                encoded_bytes,
                max_encoded_bytes,
            } => write!(
                f,
                "snapshot item {item:?} requires a {encoded_bytes}-byte page, exceeding {max_encoded_bytes}"
            ),
        }
    }
}

impl std::error::Error for SnapshotError {}

impl From<StoreError> for SnapshotError {
    fn from(error: StoreError) -> Self {
        Self::Store(error)
    }
}

impl From<rusqlite::Error> for SnapshotError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Store(StoreError::from(error))
    }
}

impl From<PageLimitsError> for SnapshotError {
    fn from(error: PageLimitsError) -> Self {
        Self::InvalidLimits(error)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SnapshotCursorDocument {
    version: u16,
    snapshot_id: SnapshotId,
    through_sequence: u64,
    section: SnapshotSection,
    last_item: SnapshotItemKey,
    limits: PageLimits,
}

/// One immutable, read-only SQLite view of the durable kernel projections.
///
/// The owned connection holds a deferred read transaction open. Dropping this
/// value releases the view; no OS process or other runtime resource is owned.
#[allow(dead_code)] // consumed by the bounded host registry in a later phase
pub(crate) struct SnapshotSession {
    snapshot_id: SnapshotId,
    through_sequence: u64,
    limits: PageLimits,
    cursor_hmac_key: Zeroizing<[u8; 32]>,
    conn: Connection,
}

impl fmt::Debug for SnapshotSession {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SnapshotSession")
            .field("snapshot_id", &self.snapshot_id)
            .field("through_sequence", &self.through_sequence)
            .field("limits", &self.limits)
            .finish_non_exhaustive()
    }
}

impl KernelStore {
    /// Pin a read-only snapshot at the current global durable event sequence.
    #[allow(dead_code)] // consumed by the bounded host registry in a later phase
    pub(crate) fn begin_snapshot(
        &self,
        limits: PageLimits,
    ) -> Result<SnapshotSession, SnapshotError> {
        limits.validate()?;
        let mut cursor_hmac_key = Zeroizing::new([0u8; 32]);
        getrandom::fill(cursor_hmac_key.as_mut()).map_err(|_| SnapshotError::EntropyUnavailable)?;
        let conn = self.open_query_connection()?;
        conn.execute_batch("BEGIN DEFERRED;")?;
        // The first read establishes the WAL snapshot before the writer can
        // commit changes that would otherwise leak into later pages.
        let sequence: i64 =
            conn.query_row("SELECT COALESCE(MAX(sequence), 0) FROM events", [], |row| {
                row.get(0)
            })?;
        let through_sequence = u64_from_nonnegative_i64("events.sequence", sequence)?;
        Ok(SnapshotSession {
            snapshot_id: SnapshotId::new(),
            through_sequence,
            limits,
            cursor_hmac_key,
            conn,
        })
    }
}

impl SnapshotSession {
    /// Read one bounded section page from the view pinned by `begin_snapshot`.
    #[allow(dead_code)] // consumed by the bounded host registry in a later phase
    pub(crate) fn page(
        &self,
        section: SnapshotSection,
        resume_cursor: Option<&[u8]>,
    ) -> Result<SnapshotPage, SnapshotError> {
        let after_item = match resume_cursor {
            Some(cursor) => Some(self.decode_cursor(cursor, section)?.last_item),
            None => None,
        };
        match section {
            SnapshotSection::Tasks => self.tasks_page(after_item),
            SnapshotSection::AgentSessions => self.agent_sessions_page(after_item),
            SnapshotSection::Artifacts => self.artifacts_page(after_item),
            SnapshotSection::Resources => self.resources_page(after_item),
            SnapshotSection::Operations => Err(SnapshotError::UnsupportedSection),
        }
    }

    fn tasks_page(
        &self,
        after_item: Option<SnapshotItemKey>,
    ) -> Result<SnapshotPage, SnapshotError> {
        let after_task = match after_item {
            Some(SnapshotItemKey::Task(task_id)) => Some(task_id),
            Some(_) => return Err(SnapshotError::CursorContextMismatch),
            None => None,
        };
        let fetch_limit = i64::from(self.limits.max_items) + 1;
        let task_ids = load_task_ids(&self.conn, after_task, fetch_limit)?;
        self.assemble_page(
            SnapshotSection::Tasks,
            after_item,
            task_ids,
            SnapshotItemKey::Task,
            |task_id| {
                let snapshot =
                    command_bus::load_task_snapshot(&self.conn, task_id)?.ok_or_else(|| {
                        StoreError::Projection("task disappeared from pinned snapshot".into())
                    })?;
                Ok(SnapshotItem::Task(TaskSnapshotItem {
                    task: snapshot.task,
                    connectivity: snapshot.connectivity,
                    attention: snapshot.attention,
                    activity: snapshot.activity,
                    review_readiness: snapshot.review_readiness,
                    primary_agent_id: snapshot.primary_agent_id,
                }))
            },
        )
    }

    fn agent_sessions_page(
        &self,
        after_item: Option<SnapshotItemKey>,
    ) -> Result<SnapshotPage, SnapshotError> {
        let after_agent = match after_item {
            Some(SnapshotItemKey::AgentSession(agent_session_id)) => Some(agent_session_id),
            Some(_) => return Err(SnapshotError::CursorContextMismatch),
            None => None,
        };
        let fetch_limit = i64::from(self.limits.max_items) + 1;
        let agent_session_ids = load_agent_session_ids(&self.conn, after_agent, fetch_limit)?;
        self.assemble_page(
            SnapshotSection::AgentSessions,
            after_item,
            agent_session_ids,
            SnapshotItemKey::AgentSession,
            |agent_session_id| {
                let agent = command_bus::load_agent_session(&self.conn, agent_session_id)?
                    .ok_or_else(|| {
                        StoreError::Projection(
                            "agent session disappeared from pinned snapshot".into(),
                        )
                    })?;
                Ok(SnapshotItem::AgentSession(agent))
            },
        )
    }

    fn artifacts_page(
        &self,
        after_item: Option<SnapshotItemKey>,
    ) -> Result<SnapshotPage, SnapshotError> {
        let after_artifact = match after_item {
            Some(SnapshotItemKey::Artifact(artifact_id)) => Some(artifact_id),
            Some(_) => return Err(SnapshotError::CursorContextMismatch),
            None => None,
        };
        let fetch_limit = i64::from(self.limits.max_items) + 1;
        let artifact_ids = load_artifact_ids(&self.conn, after_artifact, fetch_limit)?;
        self.assemble_page(
            SnapshotSection::Artifacts,
            after_item,
            artifact_ids,
            SnapshotItemKey::Artifact,
            |artifact_id| {
                let artifact =
                    command_bus::load_artifact(&self.conn, artifact_id)?.ok_or_else(|| {
                        StoreError::Projection("artifact disappeared from pinned snapshot".into())
                    })?;
                Ok(SnapshotItem::Artifact(artifact))
            },
        )
    }

    fn resources_page(
        &self,
        after_item: Option<SnapshotItemKey>,
    ) -> Result<SnapshotPage, SnapshotError> {
        let after_resource = match after_item {
            Some(SnapshotItemKey::Resource(resource_id)) => Some(resource_id),
            Some(_) => return Err(SnapshotError::CursorContextMismatch),
            None => None,
        };
        let fetch_limit = i64::from(self.limits.max_items) + 1;
        let resource_ids = load_resource_ids(&self.conn, after_resource, fetch_limit)?;
        self.assemble_page(
            SnapshotSection::Resources,
            after_item,
            resource_ids,
            SnapshotItemKey::Resource,
            |resource_id| {
                let resource =
                    command_bus::load_resource(&self.conn, resource_id)?.ok_or_else(|| {
                        StoreError::Projection("resource disappeared from pinned snapshot".into())
                    })?;
                Ok(SnapshotItem::Resource(resource))
            },
        )
    }

    fn assemble_page<Id, KeyFor, LoadItem>(
        &self,
        section: SnapshotSection,
        after_item: Option<SnapshotItemKey>,
        ids: Vec<Id>,
        key_for: KeyFor,
        mut load_item: LoadItem,
    ) -> Result<SnapshotPage, SnapshotError>
    where
        Id: Copy,
        KeyFor: Fn(Id) -> SnapshotItemKey,
        LoadItem: FnMut(Id) -> Result<SnapshotItem, SnapshotError>,
    {
        let max_items = usize::try_from(self.limits.max_items)
            .expect("validated u32 snapshot item limit fits usize");
        let mut items = Vec::with_capacity(ids.len().min(max_items));
        let mut accepted_encoded_bytes = None;
        let mut accepted_next_cursor = None;
        for (index, id) in ids.iter().take(max_items).copied().enumerate() {
            let item_key = key_for(id);
            items.push(load_item(id)?);

            let has_more = index + 1 < ids.len();
            let next_cursor = if has_more {
                Some(self.encode_cursor(section, item_key)?)
            } else {
                None
            };
            let encoded_bytes = canonical_page_encoded_bytes(
                self.snapshot_id,
                self.through_sequence,
                section,
                after_item,
                &items,
                &next_cursor,
            )?;
            if encoded_bytes > self.limits.max_encoded_bytes {
                items.pop();
                if items.is_empty() {
                    return Err(SnapshotError::PageItemTooLarge {
                        item: item_key,
                        encoded_bytes,
                        max_encoded_bytes: self.limits.max_encoded_bytes,
                    });
                }
                break;
            }
            accepted_encoded_bytes = Some(encoded_bytes);
            accepted_next_cursor = next_cursor;
        }

        let encoded_bytes = match accepted_encoded_bytes {
            Some(encoded_bytes) => encoded_bytes,
            None => {
                let encoded_bytes = canonical_page_encoded_bytes(
                    self.snapshot_id,
                    self.through_sequence,
                    section,
                    after_item,
                    &items,
                    &None,
                )?;
                if encoded_bytes > self.limits.max_encoded_bytes {
                    return Err(SnapshotError::PageEnvelopeTooLarge {
                        encoded_bytes,
                        max_encoded_bytes: self.limits.max_encoded_bytes,
                    });
                }
                encoded_bytes
            }
        };

        Ok(SnapshotPage {
            snapshot_id: self.snapshot_id,
            through_sequence: self.through_sequence,
            section,
            after_item,
            items,
            encoded_bytes,
            next_cursor: accepted_next_cursor,
        })
    }

    fn encode_cursor(
        &self,
        section: SnapshotSection,
        last_item: SnapshotItemKey,
    ) -> Result<Vec<u8>, SnapshotError> {
        let document = SnapshotCursorDocument {
            version: SNAPSHOT_CURSOR_VERSION,
            snapshot_id: self.snapshot_id,
            through_sequence: self.through_sequence,
            section,
            last_item,
            limits: self.limits,
        };
        let payload =
            rmp_serde::to_vec_named(&document).map_err(|error| StoreError::CodecMismatch {
                detail: format!("encode snapshot cursor: {error}"),
            })?;
        let mut mac = HmacSha256::new_from_slice(self.cursor_hmac_key.as_ref())
            .map_err(|_| SnapshotError::InvalidCursor)?;
        mac.update(SNAPSHOT_CURSOR_DOMAIN);
        mac.update(&payload);
        let tag = mac.finalize().into_bytes();
        let mut cursor = Vec::with_capacity(payload.len() + tag.len());
        cursor.extend_from_slice(&payload);
        cursor.extend_from_slice(&tag);
        Ok(cursor)
    }

    fn decode_cursor(
        &self,
        cursor: &[u8],
        requested_section: SnapshotSection,
    ) -> Result<SnapshotCursorDocument, SnapshotError> {
        if cursor.len() <= CURSOR_TAG_BYTES || cursor.len() > MAX_CURSOR_BYTES {
            return Err(SnapshotError::InvalidCursor);
        }
        let (payload, tag) = cursor.split_at(cursor.len() - CURSOR_TAG_BYTES);
        let mut mac = HmacSha256::new_from_slice(self.cursor_hmac_key.as_ref())
            .map_err(|_| SnapshotError::InvalidCursor)?;
        mac.update(SNAPSHOT_CURSOR_DOMAIN);
        mac.update(payload);
        mac.verify_slice(tag)
            .map_err(|_| SnapshotError::InvalidCursor)?;

        let document: SnapshotCursorDocument =
            rmp_serde::from_slice(payload).map_err(|_| SnapshotError::InvalidCursor)?;
        let canonical =
            rmp_serde::to_vec_named(&document).map_err(|_| SnapshotError::InvalidCursor)?;
        if canonical.as_slice() != payload || document.version != SNAPSHOT_CURSOR_VERSION {
            return Err(SnapshotError::InvalidCursor);
        }
        document.limits.validate()?;
        if document.snapshot_id != self.snapshot_id
            || document.through_sequence != self.through_sequence
            || document.section != requested_section
            || document.limits != self.limits
        {
            return Err(SnapshotError::CursorContextMismatch);
        }
        Ok(document)
    }
}

fn load_task_ids(
    conn: &Connection,
    after_task: Option<TaskId>,
    fetch_limit: i64,
) -> Result<Vec<TaskId>, SnapshotError> {
    let mut task_ids = Vec::new();
    match after_task {
        Some(after_task) => {
            let mut stmt = conn.prepare(
                "SELECT task_id FROM tasks WHERE task_id > ?1 ORDER BY task_id ASC LIMIT ?2",
            )?;
            let rows = stmt.query_map(
                rusqlite::params![after_task.as_bytes().as_slice(), fetch_limit],
                |row| row.get::<_, Vec<u8>>(0),
            )?;
            for row in rows {
                task_ids.push(decode_task_id(&row?)?);
            }
        }
        None => {
            let mut stmt =
                conn.prepare("SELECT task_id FROM tasks ORDER BY task_id ASC LIMIT ?1")?;
            let rows = stmt.query_map([fetch_limit], |row| row.get::<_, Vec<u8>>(0))?;
            for row in rows {
                task_ids.push(decode_task_id(&row?)?);
            }
        }
    }
    Ok(task_ids)
}

fn load_agent_session_ids(
    conn: &Connection,
    after_agent: Option<AgentSessionId>,
    fetch_limit: i64,
) -> Result<Vec<AgentSessionId>, SnapshotError> {
    let mut agent_session_ids = Vec::new();
    match after_agent {
        Some(after_agent) => {
            let mut stmt = conn.prepare(
                "SELECT agent_session_id FROM agent_sessions
                 WHERE agent_session_id > ?1 ORDER BY agent_session_id ASC LIMIT ?2",
            )?;
            let rows = stmt.query_map(
                rusqlite::params![after_agent.as_bytes().as_slice(), fetch_limit],
                |row| row.get::<_, Vec<u8>>(0),
            )?;
            for row in rows {
                agent_session_ids.push(decode_agent_session_id(&row?)?);
            }
        }
        None => {
            let mut stmt = conn.prepare(
                "SELECT agent_session_id FROM agent_sessions
                 ORDER BY agent_session_id ASC LIMIT ?1",
            )?;
            let rows = stmt.query_map([fetch_limit], |row| row.get::<_, Vec<u8>>(0))?;
            for row in rows {
                agent_session_ids.push(decode_agent_session_id(&row?)?);
            }
        }
    }
    Ok(agent_session_ids)
}

fn load_artifact_ids(
    conn: &Connection,
    after_artifact: Option<ArtifactId>,
    fetch_limit: i64,
) -> Result<Vec<ArtifactId>, SnapshotError> {
    let mut artifact_ids = Vec::new();
    match after_artifact {
        Some(after_artifact) => {
            let mut stmt = conn.prepare(
                "SELECT artifact_id FROM artifacts
                 WHERE artifact_id > ?1 ORDER BY artifact_id ASC LIMIT ?2",
            )?;
            let rows = stmt.query_map(
                rusqlite::params![after_artifact.as_bytes().as_slice(), fetch_limit],
                |row| row.get::<_, Vec<u8>>(0),
            )?;
            for row in rows {
                artifact_ids.push(decode_artifact_id(&row?)?);
            }
        }
        None => {
            let mut stmt = conn
                .prepare("SELECT artifact_id FROM artifacts ORDER BY artifact_id ASC LIMIT ?1")?;
            let rows = stmt.query_map([fetch_limit], |row| row.get::<_, Vec<u8>>(0))?;
            for row in rows {
                artifact_ids.push(decode_artifact_id(&row?)?);
            }
        }
    }
    Ok(artifact_ids)
}

fn load_resource_ids(
    conn: &Connection,
    after_resource: Option<ResourceId>,
    fetch_limit: i64,
) -> Result<Vec<ResourceId>, SnapshotError> {
    let mut resource_ids = Vec::new();
    match after_resource {
        Some(after_resource) => {
            let mut stmt = conn.prepare(
                "SELECT resource_id FROM resources
                 WHERE resource_id > ?1 ORDER BY resource_id ASC LIMIT ?2",
            )?;
            let rows = stmt.query_map(
                rusqlite::params![after_resource.as_bytes().as_slice(), fetch_limit],
                |row| row.get::<_, Vec<u8>>(0),
            )?;
            for row in rows {
                resource_ids.push(decode_resource_id(&row?)?);
            }
        }
        None => {
            let mut stmt = conn
                .prepare("SELECT resource_id FROM resources ORDER BY resource_id ASC LIMIT ?1")?;
            let rows = stmt.query_map([fetch_limit], |row| row.get::<_, Vec<u8>>(0))?;
            for row in rows {
                resource_ids.push(decode_resource_id(&row?)?);
            }
        }
    }
    Ok(resource_ids)
}

fn decode_task_id(bytes: &[u8]) -> Result<TaskId, SnapshotError> {
    let bytes: [u8; 16] = bytes.try_into().map_err(|_| StoreError::CodecMismatch {
        detail: "tasks.task_id must be 16 bytes".into(),
    })?;
    TaskId::from_bytes(bytes)
        .map_err(|error| StoreError::CodecMismatch {
            detail: format!("tasks.task_id: {error}"),
        })
        .map_err(Into::into)
}

fn decode_agent_session_id(bytes: &[u8]) -> Result<AgentSessionId, SnapshotError> {
    let bytes: [u8; 16] = bytes.try_into().map_err(|_| StoreError::CodecMismatch {
        detail: "agent_sessions.agent_session_id must be 16 bytes".into(),
    })?;
    AgentSessionId::from_bytes(bytes)
        .map_err(|error| StoreError::CodecMismatch {
            detail: format!("agent_sessions.agent_session_id: {error}"),
        })
        .map_err(Into::into)
}

fn decode_artifact_id(bytes: &[u8]) -> Result<ArtifactId, SnapshotError> {
    let bytes: [u8; 16] = bytes.try_into().map_err(|_| StoreError::CodecMismatch {
        detail: "artifacts.artifact_id must be 16 bytes".into(),
    })?;
    ArtifactId::from_bytes(bytes)
        .map_err(|error| StoreError::CodecMismatch {
            detail: format!("artifacts.artifact_id: {error}"),
        })
        .map_err(Into::into)
}

fn decode_resource_id(bytes: &[u8]) -> Result<ResourceId, SnapshotError> {
    let bytes: [u8; 16] = bytes.try_into().map_err(|_| StoreError::CodecMismatch {
        detail: "resources.resource_id must be 16 bytes".into(),
    })?;
    ResourceId::from_bytes(bytes)
        .map_err(|error| StoreError::CodecMismatch {
            detail: format!("resources.resource_id: {error}"),
        })
        .map_err(Into::into)
}

#[derive(Serialize)]
struct SnapshotPageWire<'a> {
    snapshot_id: SnapshotId,
    through_sequence: u64,
    section: SnapshotSection,
    after_item: Option<SnapshotItemKey>,
    items: &'a [SnapshotItem],
    encoded_bytes: u32,
    next_cursor: &'a Option<Vec<u8>>,
}

fn canonical_page_encoded_bytes(
    snapshot_id: SnapshotId,
    through_sequence: u64,
    section: SnapshotSection,
    after_item: Option<SnapshotItemKey>,
    items: &[SnapshotItem],
    next_cursor: &Option<Vec<u8>>,
) -> Result<u32, SnapshotError> {
    let mut encoded_bytes = 0u32;
    for _ in 0..8 {
        let wire = SnapshotPageWire {
            snapshot_id,
            through_sequence,
            section,
            after_item,
            items,
            encoded_bytes,
            next_cursor,
        };
        let bytes = rmp_serde::to_vec_named(&wire).map_err(|error| StoreError::CodecMismatch {
            detail: format!("encode snapshot page: {error}"),
        })?;
        let actual = u32::try_from(bytes.len()).map_err(|_| StoreError::IntegerOutOfRange {
            field: "snapshot_page.encoded_bytes",
            value: u64::try_from(bytes.len()).unwrap_or(u64::MAX),
        })?;
        if actual == encoded_bytes {
            return Ok(actual);
        }
        encoded_bytes = actual;
    }
    Err(StoreError::CodecMismatch {
        detail: "snapshot page encoded length did not converge".into(),
    }
    .into())
}

impl Drop for SnapshotSession {
    fn drop(&mut self) {
        // Closing the connection also rolls back, but ending the read
        // transaction explicitly releases retained WAL pages promptly.
        let _ = self.conn.execute_batch("ROLLBACK;");
    }
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;
    use crate::domain::agent::{AgentRole, AgentSessionFacts, AgentSessionLifecycle};
    use crate::domain::artifact::{ArtifactContentRef, ArtifactFacts, ArtifactKind, PrivacyClass};
    use crate::domain::command::{
        Command, CommandEnvelope, CommandReceipt, CreateTaskIntent, RenameTaskIntent,
    };
    use crate::domain::id::{
        AgentSessionId, ArtifactId, ClientId, CommandId, EnvironmentId, ProjectId, ResourceId,
        TaskId,
    };
    use crate::domain::resource::{
        OwnerKind, ResourceFacts, ResourceKind, ResourceLifecycle, ResourceRecipe,
    };
    use crate::domain::task::{
        ReviewReadiness, TaskActivity, TaskAssignment, TaskAttention, TaskConnectivity,
        WorkspaceRef,
    };

    fn fixed_uuid_v7(tail: u8) -> [u8; 16] {
        [
            0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, tail,
        ]
    }

    macro_rules! test_id {
        ($name:ident, $ty:ty) => {
            fn $name(tail: u8) -> $ty {
                <$ty>::from_bytes(fixed_uuid_v7(tail)).expect(stringify!($name))
            }
        };
    }

    test_id!(task_id, TaskId);
    test_id!(environment_id, EnvironmentId);
    test_id!(project_id, ProjectId);
    test_id!(command_id, CommandId);
    test_id!(client_id, ClientId);
    test_id!(agent_id, AgentSessionId);
    test_id!(artifact_id, ArtifactId);
    test_id!(resource_id, ResourceId);

    fn envelope(
        command_id: CommandId,
        task_id: Option<TaskId>,
        expected_task_revision: Option<u64>,
        command: Command,
    ) -> CommandEnvelope {
        CommandEnvelope {
            command_id,
            client_id: client_id(0x20),
            task_id,
            issued_at_ms: 1_725_000_000_100,
            expected_task_revision,
            command,
        }
    }

    fn create_task(store: &mut KernelStore, task_id: TaskId, command_id: CommandId) {
        create_task_with_title(store, task_id, command_id, "Ship kernel");
    }

    fn create_task_with_title(
        store: &mut KernelStore,
        task_id: TaskId,
        command_id: CommandId,
        title: &str,
    ) {
        let intent = CreateTaskIntent {
            id: task_id,
            environment_id: environment_id(0x10),
            title: title.into(),
            description: Some("Phase 1 domain".into()),
            project_id: project_id(0x11),
            workspace: WorkspaceRef::Main,
            assignment: TaskAssignment::LocalOwner,
            created_at_ms: 1_725_000_000_000,
            connectivity: TaskConnectivity::Connected,
            attention: TaskAttention::None,
            activity: TaskActivity::Idle,
            review_readiness: ReviewReadiness::NotReady,
        };
        store
            .execute(envelope(
                command_id,
                None,
                None,
                Command::CreateTask(intent),
            ))
            .expect("create task");
    }

    fn agent_facts(task_id: TaskId, agent_session_id: AgentSessionId) -> AgentSessionFacts {
        AgentSessionFacts {
            id: agent_session_id,
            task_id,
            role: AgentRole::Primary,
            provider_kind: "codex".into(),
            provider_session_id: Some(format!("session-{agent_session_id}")),
            lifecycle: AgentSessionLifecycle::Open,
            runtime_generation: 0,
            revision: 0,
        }
    }

    fn register_agent(
        store: &mut KernelStore,
        task_id: TaskId,
        agent_session_id: AgentSessionId,
        command_id: CommandId,
        expected_revision: u64,
    ) {
        store
            .execute(envelope(
                command_id,
                Some(task_id),
                Some(expected_revision),
                Command::RegisterAgentSession {
                    agent: agent_facts(task_id, agent_session_id),
                },
            ))
            .expect("register agent");
    }

    fn artifact_facts(
        task_id: TaskId,
        artifact_id: ArtifactId,
        label: &str,
        body: String,
    ) -> ArtifactFacts {
        ArtifactFacts {
            id: artifact_id,
            task_id,
            kind: ArtifactKind::Finding,
            label: label.into(),
            content_ref: ArtifactContentRef::inline_utf8(body).expect("artifact content"),
            sha256: [artifact_id.as_bytes()[15]; 32],
            privacy_class: PrivacyClass::LocalOnly,
            created_at_ms: 1_725_000_000_200,
        }
    }

    fn register_artifact(
        store: &mut KernelStore,
        task_id: TaskId,
        artifact: ArtifactFacts,
        command_id: CommandId,
        expected_revision: u64,
    ) {
        store
            .execute(envelope(
                command_id,
                Some(task_id),
                Some(expected_revision),
                Command::RegisterArtifact { artifact },
            ))
            .expect("register artifact");
    }

    fn resource_facts(task_id: TaskId, resource_id: ResourceId) -> ResourceFacts {
        ResourceFacts {
            id: resource_id,
            task_id: Some(task_id),
            owner_kind: OwnerKind::Task,
            resource_kind: ResourceKind::Terminal,
            recipe: ResourceRecipe::Terminal {
                cols: 120,
                rows: 40,
            },
            lifecycle: ResourceLifecycle::Active,
            runtime_generation: 0,
            updated_at_ms: 1_725_000_000_300,
        }
    }

    fn register_resource(
        store: &mut KernelStore,
        task_id: TaskId,
        resource_id: ResourceId,
        command_id: CommandId,
        expected_revision: u64,
    ) {
        store
            .execute(envelope(
                command_id,
                Some(task_id),
                Some(expected_revision),
                Command::RegisterResource {
                    resource: resource_facts(task_id, resource_id),
                },
            ))
            .expect("register resource");
    }

    #[test]
    fn snapshot_has_global_cursor() {
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("kernel.sqlite3");
        let mut store = KernelStore::open(&path).expect("open");
        let task = task_id(0xB1);
        create_task(&mut store, task, command_id(0xB2));

        let before_sequence = {
            let conn = Connection::open(&path).expect("open observer");
            let sequence: i64 = conn
                .query_row("SELECT COALESCE(MAX(sequence), 0) FROM events", [], |row| {
                    row.get(0)
                })
                .expect("maximum sequence");
            u64::try_from(sequence).expect("nonnegative event sequence")
        };
        let snapshot = store
            .begin_snapshot(PageLimits::new(1_000, 512 * 1024).expect("limits"))
            .expect("begin frozen snapshot");

        let rename = envelope(
            command_id(0xB3),
            Some(task),
            Some(1),
            Command::RenameTask(RenameTaskIntent {
                title: "Renamed after snapshot".into(),
            }),
        );
        assert!(matches!(
            store.execute(rename).expect("rename after snapshot"),
            CommandReceipt::Accepted {
                task_revision: Some(2),
                ..
            }
        ));

        let first = snapshot
            .page(SnapshotSection::Tasks, None)
            .expect("first frozen task page");
        let second = snapshot
            .page(SnapshotSection::Tasks, None)
            .expect("repeat frozen task page");
        assert_eq!(first, second);
        assert_eq!(first.through_sequence, before_sequence);
        assert_eq!(first.section, SnapshotSection::Tasks);
        assert_eq!(first.items.len(), 1);
        match &first.items[0] {
            SnapshotItem::Task(item) => {
                assert_eq!(item.task.id, task);
                assert_eq!(item.task.title, "Ship kernel");
                assert_eq!(item.task.revision, 1);
            }
            other => panic!("expected task item, got {other:?}"),
        }
    }

    #[test]
    fn snapshot_rejects_invalid_primary_agent_projection() {
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("kernel.sqlite3");
        let mut store = KernelStore::open(&path).expect("open");
        let task = task_id(0xB4);
        create_task(&mut store, task, command_id(0xB5));

        let conn = Connection::open(&path).expect("open tamper connection");
        conn.execute(
            "UPDATE tasks SET primary_agent_session_id = ?1 WHERE task_id = ?2",
            rusqlite::params![
                agent_id(0xB6).as_bytes().as_slice(),
                task.as_bytes().as_slice()
            ],
        )
        .expect("tamper primary agent projection");
        drop(conn);

        let snapshot = store
            .begin_snapshot(PageLimits::new(1_000, 512 * 1024).expect("limits"))
            .expect("begin snapshot");
        assert!(matches!(
            snapshot
                .page(SnapshotSection::Tasks, None)
                .expect_err("dangling primary agent must fail closed"),
            SnapshotError::Store(StoreError::Projection(_))
        ));
    }

    #[test]
    fn snapshot_pages_resume_without_duplicates() {
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("kernel.sqlite3");
        let mut store = KernelStore::open(&path).expect("open");
        for (task_tail, command_tail) in [(0xC1, 0xD1), (0xC2, 0xD2), (0xC3, 0xD3)] {
            create_task(&mut store, task_id(task_tail), command_id(command_tail));
        }

        let limits = PageLimits::new(2, 512 * 1024).expect("limits");
        let snapshot = store.begin_snapshot(limits).expect("begin snapshot");
        let first = snapshot
            .page(SnapshotSection::Tasks, None)
            .expect("first page");
        let cursor = first.next_cursor.clone().expect("resume cursor");
        let second = snapshot
            .page(SnapshotSection::Tasks, Some(&cursor))
            .expect("second page");

        assert_eq!(first.items.len(), 2);
        assert_eq!(second.items.len(), 1);
        assert_eq!(second.next_cursor, None);
        assert_eq!(first.snapshot_id, second.snapshot_id);
        assert_eq!(first.through_sequence, second.through_sequence);

        let mut ids = first
            .items
            .iter()
            .chain(second.items.iter())
            .map(|item| match item {
                SnapshotItem::Task(item) => item.task.id,
                other => panic!("expected task item, got {other:?}"),
            })
            .collect::<Vec<_>>();
        let unique = ids
            .iter()
            .copied()
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(unique.len(), 3);
        ids.sort();
        assert_eq!(ids, vec![task_id(0xC1), task_id(0xC2), task_id(0xC3)]);
        assert_eq!(
            second.after_item,
            Some(SnapshotItemKey::Task(task_id(0xC2)))
        );
    }

    #[test]
    fn snapshot_cursor_rejects_tampering_and_wrong_section() {
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("kernel.sqlite3");
        let mut store = KernelStore::open(&path).expect("open");
        create_task(&mut store, task_id(0xC4), command_id(0xD4));
        create_task(&mut store, task_id(0xC5), command_id(0xD5));

        let snapshot = store
            .begin_snapshot(PageLimits::new(1, 512 * 1024).expect("limits"))
            .expect("begin snapshot");
        let first = snapshot
            .page(SnapshotSection::Tasks, None)
            .expect("first page");
        let cursor = first.next_cursor.expect("resume cursor");

        let mut tampered = cursor.clone();
        tampered[0] ^= 0x01;
        assert_eq!(
            snapshot.page(SnapshotSection::Tasks, Some(&tampered)),
            Err(SnapshotError::InvalidCursor)
        );
        assert_eq!(
            snapshot.page(SnapshotSection::AgentSessions, Some(&cursor)),
            Err(SnapshotError::CursorContextMismatch)
        );
    }

    #[test]
    fn snapshot_page_honors_item_and_encoded_byte_limits() {
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("kernel.sqlite3");
        let mut store = KernelStore::open(&path).expect("open");
        create_task(&mut store, task_id(0xC6), command_id(0xD6));
        create_task(&mut store, task_id(0xC7), command_id(0xD7));

        let limits = PageLimits::new(1, 2_048).expect("limits");
        let snapshot = store.begin_snapshot(limits).expect("begin snapshot");
        let page = snapshot
            .page(SnapshotSection::Tasks, None)
            .expect("bounded page");
        assert_eq!(page.items.len(), 1);
        assert!(page.encoded_bytes <= limits.max_encoded_bytes);
        assert_eq!(
            usize::try_from(page.encoded_bytes).expect("page length fits"),
            rmp_serde::to_vec_named(&page)
                .expect("encode final page")
                .len(),
            "encoded_bytes must describe the complete canonical page body"
        );

        let huge_dir = TempDir::new().expect("huge tempdir");
        let huge_path = huge_dir.path().join("kernel.sqlite3");
        let mut huge_store = KernelStore::open(&huge_path).expect("open huge store");
        create_task_with_title(
            &mut huge_store,
            task_id(0xC8),
            command_id(0xD8),
            &"x".repeat(4_096),
        );
        let huge_snapshot = huge_store
            .begin_snapshot(PageLimits::new(1, 1_024).expect("small byte limit"))
            .expect("begin huge snapshot");
        assert!(matches!(
            huge_snapshot.page(SnapshotSection::Tasks, None),
            Err(SnapshotError::PageItemTooLarge {
                item: SnapshotItemKey::Task(id),
                ..
            }) if id == task_id(0xC8)
        ));
    }

    #[test]
    fn snapshot_agent_sessions_pages_are_frozen_and_resume_without_duplicates() {
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("kernel.sqlite3");
        let mut store = KernelStore::open(&path).expect("open");
        let task = task_id(0xE0);
        create_task(&mut store, task, command_id(0xE1));
        for (agent_tail, command_tail, expected_revision) in
            [(0xE2, 0xE5, 1), (0xE3, 0xE6, 2), (0xE4, 0xE7, 3)]
        {
            register_agent(
                &mut store,
                task,
                agent_id(agent_tail),
                command_id(command_tail),
                expected_revision,
            );
        }

        let snapshot = store
            .begin_snapshot(PageLimits::new(2, 512 * 1024).expect("limits"))
            .expect("begin snapshot");
        register_agent(&mut store, task, agent_id(0xE8), command_id(0xE9), 4);

        let first = snapshot
            .page(SnapshotSection::AgentSessions, None)
            .expect("first agent page");
        let cursor = first.next_cursor.clone().expect("agent resume cursor");
        let second = snapshot
            .page(SnapshotSection::AgentSessions, Some(&cursor))
            .expect("second agent page");

        assert_eq!(first.items.len(), 2);
        assert_eq!(second.items.len(), 1);
        assert_eq!(first.snapshot_id, second.snapshot_id);
        assert_eq!(first.through_sequence, second.through_sequence);
        assert_eq!(
            second.after_item,
            Some(SnapshotItemKey::AgentSession(agent_id(0xE3)))
        );
        assert_eq!(second.next_cursor, None);

        let ids = first
            .items
            .iter()
            .chain(second.items.iter())
            .map(|item| match item {
                SnapshotItem::AgentSession(agent) => agent.id,
                other => panic!("expected agent item, got {other:?}"),
            })
            .collect::<Vec<_>>();
        assert_eq!(ids, vec![agent_id(0xE2), agent_id(0xE3), agent_id(0xE4)]);
        assert!(!ids.contains(&agent_id(0xE8)));
    }

    #[test]
    fn snapshot_agent_sessions_rejects_invalid_projection() {
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("kernel.sqlite3");
        let mut store = KernelStore::open(&path).expect("open");
        let task = task_id(0xEA);
        let agent = agent_id(0xEB);
        create_task(&mut store, task, command_id(0xEC));
        register_agent(&mut store, task, agent, command_id(0xED), 1);

        let conn = Connection::open(&path).expect("open tamper connection");
        conn.execute(
            "UPDATE agent_sessions SET runtime_generation = -1 WHERE agent_session_id = ?1",
            [agent.as_bytes().as_slice()],
        )
        .expect("tamper agent generation");
        drop(conn);

        let snapshot = store
            .begin_snapshot(PageLimits::new(1_000, 512 * 1024).expect("limits"))
            .expect("begin snapshot");
        assert_eq!(
            snapshot
                .page(SnapshotSection::AgentSessions, None)
                .expect_err("invalid agent projection must fail closed"),
            SnapshotError::Store(StoreError::IntegerOutOfRange {
                field: "agent_sessions.runtime_generation",
                value: 1,
            })
        );
    }

    #[test]
    fn snapshot_artifacts_pages_are_frozen_and_resume_without_duplicates() {
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("kernel.sqlite3");
        let mut store = KernelStore::open(&path).expect("open");
        let task = task_id(0xF0);
        create_task(&mut store, task, command_id(0xF1));
        for (artifact_tail, command_tail, expected_revision) in
            [(0xF2, 0xF5, 1), (0xF3, 0xF6, 2), (0xF4, 0xF7, 3)]
        {
            register_artifact(
                &mut store,
                task,
                artifact_facts(
                    task,
                    artifact_id(artifact_tail),
                    &format!("Artifact {artifact_tail}"),
                    format!("body-{artifact_tail}"),
                ),
                command_id(command_tail),
                expected_revision,
            );
        }

        let snapshot = store
            .begin_snapshot(PageLimits::new(2, 512 * 1024).expect("limits"))
            .expect("begin snapshot");
        register_artifact(
            &mut store,
            task,
            artifact_facts(
                task,
                artifact_id(0xF8),
                "Post-snapshot artifact",
                "post-snapshot".into(),
            ),
            command_id(0xF9),
            4,
        );

        let first = snapshot
            .page(SnapshotSection::Artifacts, None)
            .expect("first artifact page");
        let cursor = first.next_cursor.clone().expect("artifact resume cursor");
        let second = snapshot
            .page(SnapshotSection::Artifacts, Some(&cursor))
            .expect("second artifact page");

        assert_eq!(first.items.len(), 2);
        assert_eq!(second.items.len(), 1);
        assert_eq!(first.snapshot_id, second.snapshot_id);
        assert_eq!(first.through_sequence, second.through_sequence);
        assert_eq!(
            second.after_item,
            Some(SnapshotItemKey::Artifact(artifact_id(0xF3)))
        );
        assert_eq!(second.next_cursor, None);

        let ids = first
            .items
            .iter()
            .chain(second.items.iter())
            .map(|item| match item {
                SnapshotItem::Artifact(artifact) => artifact.id,
                other => panic!("expected artifact item, got {other:?}"),
            })
            .collect::<Vec<_>>();
        assert_eq!(
            ids,
            vec![artifact_id(0xF2), artifact_id(0xF3), artifact_id(0xF4)]
        );
        assert!(!ids.contains(&artifact_id(0xF8)));
    }

    #[test]
    fn snapshot_artifacts_preserve_exact_bytes_and_oversize_identity() {
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("kernel.sqlite3");
        let mut store = KernelStore::open(&path).expect("open");
        let task = task_id(0xA0);
        let artifact = artifact_id(0xA1);
        create_task(&mut store, task, command_id(0xA2));
        register_artifact(
            &mut store,
            task,
            artifact_facts(task, artifact, "Small", "small body".into()),
            command_id(0xA3),
            1,
        );
        let huge_artifact = artifact_id(0xA4);
        register_artifact(
            &mut store,
            task,
            artifact_facts(task, huge_artifact, "Huge", "x".repeat(8_192)),
            command_id(0xA5),
            2,
        );
        let snapshot = store
            .begin_snapshot(PageLimits::new(10, 2_048).expect("limits"))
            .expect("begin snapshot");
        let page = snapshot
            .page(SnapshotSection::Artifacts, None)
            .expect("artifact page");
        assert_eq!(page.items.len(), 1);
        assert_eq!(
            usize::try_from(page.encoded_bytes).expect("page length fits"),
            rmp_serde::to_vec_named(&page)
                .expect("encode final page")
                .len(),
        );
        let cursor = page
            .next_cursor
            .expect("byte cutoff must preserve a resume cursor");
        assert!(matches!(
            snapshot.page(SnapshotSection::Artifacts, Some(&cursor)),
            Err(SnapshotError::PageItemTooLarge {
                item: SnapshotItemKey::Artifact(id),
                ..
            }) if id == huge_artifact
        ));
    }

    #[test]
    fn snapshot_artifacts_reject_invalid_projection() {
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("kernel.sqlite3");
        let mut store = KernelStore::open(&path).expect("open");
        let task = task_id(0xA8);
        let artifact = artifact_id(0xA9);
        create_task(&mut store, task, command_id(0xAA));
        register_artifact(
            &mut store,
            task,
            artifact_facts(task, artifact, "Valid", "body".into()),
            command_id(0xAB),
            1,
        );

        let conn = Connection::open(&path).expect("open tamper connection");
        conn.execute(
            "UPDATE artifacts SET label = '  ' WHERE artifact_id = ?1",
            [artifact.as_bytes().as_slice()],
        )
        .expect("tamper artifact label");
        drop(conn);

        let snapshot = store
            .begin_snapshot(PageLimits::new(1_000, 512 * 1024).expect("limits"))
            .expect("begin snapshot");
        assert!(matches!(
            snapshot
                .page(SnapshotSection::Artifacts, None)
                .expect_err("invalid artifact projection must fail closed"),
            SnapshotError::Store(StoreError::Projection(_))
        ));
    }

    #[test]
    fn snapshot_resources_pages_are_frozen_and_resume_without_duplicates() {
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("kernel.sqlite3");
        let mut store = KernelStore::open(&path).expect("open");
        let task = task_id(0xB0);
        create_task(&mut store, task, command_id(0xB1));
        for (resource_tail, command_tail, expected_revision) in
            [(0xB2, 0xB5, 1), (0xB3, 0xB6, 2), (0xB4, 0xB7, 3)]
        {
            register_resource(
                &mut store,
                task,
                resource_id(resource_tail),
                command_id(command_tail),
                expected_revision,
            );
        }

        let snapshot = store
            .begin_snapshot(PageLimits::new(2, 512 * 1024).expect("limits"))
            .expect("begin snapshot");
        register_resource(&mut store, task, resource_id(0xB8), command_id(0xB9), 4);

        let first = snapshot
            .page(SnapshotSection::Resources, None)
            .expect("first resource page");
        let cursor = first.next_cursor.clone().expect("resource resume cursor");
        let second = snapshot
            .page(SnapshotSection::Resources, Some(&cursor))
            .expect("second resource page");

        assert_eq!(first.items.len(), 2);
        assert_eq!(second.items.len(), 1);
        assert_eq!(first.snapshot_id, second.snapshot_id);
        assert_eq!(first.through_sequence, second.through_sequence);
        assert_eq!(
            second.after_item,
            Some(SnapshotItemKey::Resource(resource_id(0xB3)))
        );
        assert_eq!(second.next_cursor, None);

        let ids = first
            .items
            .iter()
            .chain(second.items.iter())
            .map(|item| match item {
                SnapshotItem::Resource(resource) => resource.id,
                other => panic!("expected resource item, got {other:?}"),
            })
            .collect::<Vec<_>>();
        assert_eq!(
            ids,
            vec![resource_id(0xB2), resource_id(0xB3), resource_id(0xB4)]
        );
        assert!(!ids.contains(&resource_id(0xB8)));
    }

    #[test]
    fn snapshot_resources_preserve_host_ownership() {
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("kernel.sqlite3");
        let store = KernelStore::open(&path).expect("open");
        let resource = resource_id(0xBA);

        let conn = Connection::open(&path).expect("open seed connection");
        conn.execute(
            "INSERT INTO resources(
                resource_id, task_id, owner_kind, resource_kind, recipe, lifecycle,
                runtime_generation, updated_at_ms
             ) VALUES (?1, NULL, 'host', 'service', ?2, 'released', 7, 100)",
            rusqlite::params![
                resource.as_bytes().as_slice(),
                rmp_serde::to_vec(&ResourceRecipe::Service {
                    command: "host-service".into(),
                })
                .expect("encode host recipe"),
            ],
        )
        .expect("seed host resource projection");
        drop(conn);

        let snapshot = store
            .begin_snapshot(PageLimits::new(10, 512 * 1024).expect("limits"))
            .expect("begin snapshot");
        let page = snapshot
            .page(SnapshotSection::Resources, None)
            .expect("host resource page");
        assert_eq!(page.items.len(), 1);
        match &page.items[0] {
            SnapshotItem::Resource(item) => {
                assert_eq!(item.id, resource);
                assert_eq!(item.owner_kind, OwnerKind::Host);
                assert_eq!(item.task_id, None);
                assert_eq!(item.lifecycle, ResourceLifecycle::Released);
                assert_eq!(item.runtime_generation, 7);
            }
            other => panic!("expected resource item, got {other:?}"),
        }
    }

    #[test]
    fn snapshot_resources_reject_invalid_owner_binding() {
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("kernel.sqlite3");
        let mut store = KernelStore::open(&path).expect("open");
        let task = task_id(0xBB);
        let resource = resource_id(0xBC);
        create_task(&mut store, task, command_id(0xBD));
        register_resource(&mut store, task, resource, command_id(0xBE), 1);

        let conn = Connection::open(&path).expect("open tamper connection");
        conn.execute(
            "UPDATE resources SET owner_kind = 'host' WHERE resource_id = ?1",
            [resource.as_bytes().as_slice()],
        )
        .expect("tamper resource owner binding");
        drop(conn);

        let snapshot = store
            .begin_snapshot(PageLimits::new(1_000, 512 * 1024).expect("limits"))
            .expect("begin snapshot");
        assert!(matches!(
            snapshot
                .page(SnapshotSection::Resources, None)
                .expect_err("invalid owner binding must fail closed"),
            SnapshotError::Store(StoreError::Projection(_))
        ));
    }

    #[test]
    fn snapshot_resources_reject_dangling_task_owner() {
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("kernel.sqlite3");
        let store = KernelStore::open(&path).expect("open");
        let resource = resource_id(0xC0);
        let missing_task = task_id(0xC1);

        let conn = Connection::open(&path).expect("open seed connection");
        conn.execute_batch("PRAGMA foreign_keys = OFF;")
            .expect("allow corruption fixture");
        conn.execute(
            "INSERT INTO resources(
                resource_id, task_id, owner_kind, resource_kind, recipe, lifecycle,
                runtime_generation, updated_at_ms
             ) VALUES (?1, ?2, 'task', 'terminal', ?3, 'active', 0, 100)",
            rusqlite::params![
                resource.as_bytes().as_slice(),
                missing_task.as_bytes().as_slice(),
                rmp_serde::to_vec(&ResourceRecipe::Terminal {
                    cols: 120,
                    rows: 40
                })
                .expect("encode terminal recipe"),
            ],
        )
        .expect("seed dangling resource projection");
        drop(conn);

        let snapshot = store
            .begin_snapshot(PageLimits::new(10, 512 * 1024).expect("limits"))
            .expect("begin snapshot");
        assert!(matches!(
            snapshot
                .page(SnapshotSection::Resources, None)
                .expect_err("dangling task owner must fail closed"),
            SnapshotError::Store(StoreError::Corruption)
        ));
    }

    #[test]
    fn snapshot_resources_report_oversized_item_identity() {
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("kernel.sqlite3");
        let store = KernelStore::open(&path).expect("open");
        let resource = resource_id(0xC2);

        let conn = Connection::open(&path).expect("open seed connection");
        conn.execute(
            "INSERT INTO resources(
                resource_id, task_id, owner_kind, resource_kind, recipe, lifecycle,
                runtime_generation, updated_at_ms
             ) VALUES (?1, NULL, 'host', 'service', ?2, 'active', 0, 100)",
            rusqlite::params![
                resource.as_bytes().as_slice(),
                rmp_serde::to_vec(&ResourceRecipe::Service {
                    command: "x".repeat(8_192),
                })
                .expect("encode oversized service recipe"),
            ],
        )
        .expect("seed oversized resource projection");
        drop(conn);

        let snapshot = store
            .begin_snapshot(PageLimits::new(10, 2_048).expect("limits"))
            .expect("begin snapshot");
        assert!(matches!(
            snapshot.page(SnapshotSection::Resources, None),
            Err(SnapshotError::PageItemTooLarge {
                item: SnapshotItemKey::Resource(id),
                ..
            }) if id == resource
        ));
    }

    #[test]
    fn page_limits_reject_forged_wire_values() {
        let forged = PageLimits {
            max_items: 0,
            max_encoded_bytes: 1,
        };
        assert!(
            rmp_serde::to_vec_named(&forged).is_err(),
            "invalid public value must not serialize"
        );

        #[derive(Serialize)]
        struct ForgedPageLimits {
            max_items: u32,
            max_encoded_bytes: u32,
        }
        let bytes = rmp_serde::to_vec_named(&ForgedPageLimits {
            max_items: 0,
            max_encoded_bytes: 1,
        })
        .expect("encode tampered limits");
        assert!(rmp_serde::from_slice::<PageLimits>(&bytes).is_err());
    }
}
