use std::collections::BTreeMap;
use std::fmt;

use hmac::{Hmac, Mac};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use zeroize::Zeroizing;

use crate::domain::artifact::ArtifactSummary;
use crate::domain::browser::{BrowserSnapshotRow, BrowserSnapshotSection};
use crate::domain::id::{
    AgentSessionId, ArtifactId, BrowserContextId, BrowserTabId, OperationId, ResourceId,
    SnapshotId, TaskId,
};
use crate::domain::snapshot::{
    canonical_snapshot_page_size, CanonicalPageSizeError, PageLimits, PageLimitsError,
    SnapshotItem, SnapshotItemKey, SnapshotPage, SnapshotSection, TaskSnapshotItem,
    MAX_SNAPSHOT_PAGE_ENCODED_BYTES,
};
use crate::kernel::command_bus;
use crate::kernel::store::{load_event_log_bounds, KernelStore, StoreError};
use crate::kernel::SessionScope;

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
    scope: SessionScope,
}

/// One immutable, read-only SQLite view of the durable kernel projections.
///
/// The owned connection holds a deferred read transaction open. Dropping this
/// value releases the view; no OS process or other runtime resource is owned.
pub(crate) struct SnapshotSession {
    snapshot_id: SnapshotId,
    through_sequence: u64,
    limits: PageLimits,
    scope: SessionScope,
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
    pub(crate) fn begin_snapshot(
        &self,
        limits: PageLimits,
    ) -> Result<SnapshotSession, SnapshotError> {
        self.begin_snapshot_scoped(limits, SessionScope::GLOBAL)
    }

    pub(crate) fn begin_snapshot_scoped(
        &self,
        limits: PageLimits,
        scope: SessionScope,
    ) -> Result<SnapshotSession, SnapshotError> {
        limits.validate()?;
        let mut cursor_hmac_key = Zeroizing::new([0u8; 32]);
        getrandom::fill(cursor_hmac_key.as_mut()).map_err(|_| SnapshotError::EntropyUnavailable)?;
        let conn = self.open_query_connection()?;
        conn.execute_batch("BEGIN DEFERRED;")?;
        // The first read establishes the WAL snapshot before the writer can
        // commit changes that would otherwise leak into later pages.
        let through_sequence = load_event_log_bounds(&conn)?.newest_sequence;
        Ok(SnapshotSession {
            snapshot_id: SnapshotId::new(),
            through_sequence,
            limits,
            scope,
            cursor_hmac_key,
            conn,
        })
    }
}

/// Which rows one pinned snapshot session admits.
///
/// The unscoped session is the STARTUP projection, not "everything": the shell
/// renders a Settled or Archived task's operations, agents, resources,
/// artifacts and browser rows only after the user clicks that task, so paging
/// them for the whole store at startup was pure cost. The task-scoped session
/// is what that click issues.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SnapshotProjection {
    /// Every live task's list row, plus detail for the tasks the shell renders
    /// without a click (`open`/`closing`) and every non-terminal operation.
    Startup,
    /// One task's rows for every section, whatever its lifecycle.
    Task(TaskId),
}

/// Lifecycles whose detail rows the startup projection ships.
///
/// `settled` and `archived` are excluded because nothing renders them until
/// the task is selected; `deleted` is excluded because the task is gone.
const STARTUP_DETAIL_LIFECYCLES: &str = "('open','closing')";
/// Operation states that are still reconcilable, hence still shipped unscoped.
const NON_TERMINAL_OPERATION_STATES: &str = "('accepted','uncertain')";

impl SnapshotProjection {
    fn task_id_param(self) -> Vec<u8> {
        match self {
            Self::Startup => Vec::new(),
            Self::Task(task_id) => task_id.as_bytes().to_vec(),
        }
    }
}

impl SnapshotSession {
    fn projection(&self) -> SnapshotProjection {
        match self.scope.task_id {
            Some(task_id) => SnapshotProjection::Task(task_id),
            None => SnapshotProjection::Startup,
        }
    }

    pub(crate) fn snapshot_id(&self) -> SnapshotId {
        self.snapshot_id
    }

    pub(crate) fn scope(&self) -> SessionScope {
        self.scope
    }

    /// Read one bounded section page from the view pinned by `begin_snapshot`.
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
            SnapshotSection::Operations => self.operations_page(after_item),
            SnapshotSection::BrowserContexts => self.browser_contexts_page(after_item),
            SnapshotSection::BrowserTabs => self.browser_tabs_page(after_item),
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
        let task_ids = load_task_ids(&self.conn, self.projection(), after_task, fetch_limit)?;
        self.assemble_page(
            SnapshotSection::Tasks,
            after_item,
            task_ids,
            SnapshotItemKey::Task,
            |task_id| {
                let snapshot = command_bus::load_task_snapshot(&self.conn, task_id)
                    .map_err(|error| {
                        StoreError::Projection(format!(
                            "task {task_id} could not be loaded for the pinned snapshot: {error}"
                        ))
                    })?
                    .ok_or_else(|| {
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
        let agent_session_ids =
            load_agent_session_ids(&self.conn, self.projection(), after_agent, fetch_limit)?;
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
        let artifact_ids =
            load_artifact_ids(&self.conn, self.projection(), after_artifact, fetch_limit)?;
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
                let summary = ArtifactSummary::from_facts(&artifact)
                    .map_err(|err| StoreError::Projection(err.to_string()))?;
                Ok(SnapshotItem::Artifact(summary))
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
        let resource_ids =
            load_resource_ids(&self.conn, self.projection(), after_resource, fetch_limit)?;
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

    fn operations_page(
        &self,
        after_item: Option<SnapshotItemKey>,
    ) -> Result<SnapshotPage, SnapshotError> {
        let after_operation = match after_item {
            Some(SnapshotItemKey::Operation(operation_id)) => Some(operation_id),
            Some(_) => return Err(SnapshotError::CursorContextMismatch),
            None => None,
        };
        let fetch_limit = i64::from(self.limits.max_items) + 1;
        let operation_ids =
            load_operation_ids(&self.conn, self.projection(), after_operation, fetch_limit)?;
        self.assemble_page(
            SnapshotSection::Operations,
            after_item,
            operation_ids,
            SnapshotItemKey::Operation,
            |operation_id| {
                let operation = command_bus::load_operation_facts(&self.conn, operation_id)
                    .map_err(|error| {
                        StoreError::Projection(format!(
                            "operation {operation_id} could not be loaded for the pinned snapshot: {error}"
                        ))
                    })?
                    .ok_or_else(|| {
                        StoreError::Projection("operation disappeared from pinned snapshot".into())
                    })?;
                Ok(SnapshotItem::Operation(operation))
            },
        )
    }

    fn browser_contexts_page(
        &self,
        after_item: Option<SnapshotItemKey>,
    ) -> Result<SnapshotPage, SnapshotError> {
        let after_context = match after_item {
            Some(SnapshotItemKey::BrowserContext(context_id)) => Some(context_id),
            Some(_) => return Err(SnapshotError::CursorContextMismatch),
            None => None,
        };
        let (contexts, _) = load_browser_views(&self.conn, self.projection())?;
        let fetch_limit = usize::try_from(i64::from(self.limits.max_items) + 1)
            .expect("validated snapshot item limit fits usize");
        let ids = contexts
            .keys()
            .copied()
            .filter(|id| after_context.map_or(true, |after| *id > after))
            .take(fetch_limit)
            .collect::<Vec<_>>();
        self.assemble_page(
            SnapshotSection::BrowserContexts,
            after_item,
            ids,
            SnapshotItemKey::BrowserContext,
            |context_id| {
                contexts
                    .get(&context_id)
                    .cloned()
                    .map(SnapshotItem::BrowserContext)
                    .ok_or_else(|| {
                        StoreError::Projection(
                            "browser context disappeared from pinned snapshot".into(),
                        )
                        .into()
                    })
            },
        )
    }

    fn browser_tabs_page(
        &self,
        after_item: Option<SnapshotItemKey>,
    ) -> Result<SnapshotPage, SnapshotError> {
        let after_tab = match after_item {
            Some(SnapshotItemKey::BrowserTab(tab_id)) => Some(tab_id),
            Some(_) => return Err(SnapshotError::CursorContextMismatch),
            None => None,
        };
        let (_, tabs) = load_browser_views(&self.conn, self.projection())?;
        let fetch_limit = usize::try_from(i64::from(self.limits.max_items) + 1)
            .expect("validated snapshot item limit fits usize");
        let ids = tabs
            .keys()
            .copied()
            .filter(|id| after_tab.map_or(true, |after| *id > after))
            .take(fetch_limit)
            .collect::<Vec<_>>();
        self.assemble_page(
            SnapshotSection::BrowserTabs,
            after_item,
            ids,
            SnapshotItemKey::BrowserTab,
            |tab_id| {
                tabs.get(&tab_id)
                    .cloned()
                    .map(SnapshotItem::BrowserTab)
                    .ok_or_else(|| {
                        StoreError::Projection(
                            "browser tab disappeared from pinned snapshot".into(),
                        )
                        .into()
                    })
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
            scope: self.scope,
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
            || document.scope != self.scope
        {
            return Err(SnapshotError::CursorContextMismatch);
        }
        Ok(document)
    }
}

fn load_browser_views(
    conn: &Connection,
    projection: SnapshotProjection,
) -> Result<
    (
        BTreeMap<BrowserContextId, crate::domain::browser::BrowserContextView>,
        BTreeMap<BrowserTabId, crate::domain::browser::BrowserTabView>,
    ),
    SnapshotError,
> {
    // Browser rows follow the same admission as every other detail section:
    // walking every settled task's snapshot to project rows nothing renders
    // was the most expensive way to produce nothing.
    let task_ids = load_browser_owner_task_ids(conn, projection)?;
    let mut contexts = BTreeMap::new();
    let mut tabs = BTreeMap::new();
    for task_id in task_ids {
        let Some(snapshot) = command_bus::load_task_snapshot(conn, task_id)? else {
            return Err(StoreError::Projection(
                "browser task disappeared from pinned snapshot".into(),
            )
            .into());
        };
        let context_page = snapshot
            .browser
            .snapshot_page(
                BrowserSnapshotSection::Contexts,
                None,
                crate::domain::browser::MAX_BROWSER_CONTEXTS as u32,
                MAX_SNAPSHOT_PAGE_ENCODED_BYTES,
            )
            .map_err(|error| StoreError::Projection(error.to_string()))?;
        for row in context_page.items {
            let BrowserSnapshotRow::Context(view) = row else {
                return Err(StoreError::Projection(
                    "browser context snapshot returned a tab".into(),
                )
                .into());
            };
            if contexts.insert(view.context_id, view).is_some() {
                return Err(StoreError::Projection(
                    "duplicate browser context identity in pinned snapshot".into(),
                )
                .into());
            }
        }
        let tab_page = snapshot
            .browser
            .snapshot_page(
                BrowserSnapshotSection::Tabs,
                None,
                crate::domain::browser::MAX_BROWSER_TABS as u32,
                MAX_SNAPSHOT_PAGE_ENCODED_BYTES,
            )
            .map_err(|error| StoreError::Projection(error.to_string()))?;
        for row in tab_page.items {
            let BrowserSnapshotRow::Tab(view) = row else {
                return Err(StoreError::Projection(
                    "browser tab snapshot returned a context".into(),
                )
                .into());
            };
            if tabs.insert(view.tab_id, view).is_some() {
                return Err(StoreError::Projection(
                    "duplicate browser tab identity in pinned snapshot".into(),
                )
                .into());
            }
        }
    }
    Ok((contexts, tabs))
}

/// Ids for one section, ordered by identity so the snapshot cursor is stable.
///
/// `after` is bound as a blob and empty means "from the start": an empty blob
/// sorts below every 16-byte id, so one statement serves both the first page
/// and every continuation, and the primary-key index still drives the scan.
fn load_scoped_ids(
    conn: &Connection,
    sql: &str,
    projection: SnapshotProjection,
    after: Option<&[u8]>,
    fetch_limit: i64,
) -> Result<Vec<Vec<u8>>, SnapshotError> {
    let after: &[u8] = after.unwrap_or(&[]);
    let scoped_task = projection.task_id_param();
    let mut params: Vec<&dyn rusqlite::ToSql> = vec![&after, &fetch_limit];
    if matches!(projection, SnapshotProjection::Task(_)) {
        params.push(&scoped_task);
    }
    let mut stmt = conn.prepare(sql)?;
    let rows = stmt.query_map(rusqlite::params_from_iter(params), |row| {
        row.get::<_, Vec<u8>>(0)
    })?;
    let mut ids = Vec::new();
    for row in rows {
        ids.push(row?);
    }
    Ok(ids)
}

fn load_task_ids(
    conn: &Connection,
    projection: SnapshotProjection,
    after_task: Option<TaskId>,
    fetch_limit: i64,
) -> Result<Vec<TaskId>, SnapshotError> {
    // A task deleted between two purge sweeps still has its row; nothing
    // selectable may be in that state, so the list never ships it.
    let sql = match projection {
        SnapshotProjection::Startup => {
            "SELECT task_id FROM tasks
             WHERE task_id > ?1 AND lifecycle <> 'deleted'
             ORDER BY task_id ASC LIMIT ?2"
        }
        SnapshotProjection::Task(_) => {
            "SELECT task_id FROM tasks
             WHERE task_id > ?1 AND task_id = ?3
             ORDER BY task_id ASC LIMIT ?2"
        }
    };
    let after = after_task.map(|id| id.as_bytes().to_vec());
    let rows = load_scoped_ids(conn, sql, projection, after.as_deref(), fetch_limit)?;
    rows.iter().map(|bytes| decode_task_id(bytes)).collect()
}

/// Tasks whose browser rows this projection admits.
///
/// Unscoped this is every task the startup projection ships detail for; scoped
/// it is the one selected task, whatever its lifecycle.
fn load_browser_owner_task_ids(
    conn: &Connection,
    projection: SnapshotProjection,
) -> Result<Vec<TaskId>, SnapshotError> {
    let sql = match projection {
        SnapshotProjection::Startup => startup_browser_owner_task_sql(),
        SnapshotProjection::Task(_) => "SELECT task_id FROM tasks
             WHERE task_id > ?1 AND task_id = ?3
             ORDER BY task_id ASC LIMIT ?2"
            .to_string(),
    };
    let rows = load_scoped_ids(conn, &sql, projection, None, i64::MAX)?;
    rows.iter().map(|bytes| decode_task_id(bytes)).collect()
}

fn startup_browser_owner_task_sql() -> String {
    format!(
        "SELECT task_id FROM tasks
         WHERE task_id > ?1 AND lifecycle IN {STARTUP_DETAIL_LIFECYCLES}
         ORDER BY task_id ASC LIMIT ?2"
    )
}

fn load_agent_session_ids(
    conn: &Connection,
    projection: SnapshotProjection,
    after_agent: Option<AgentSessionId>,
    fetch_limit: i64,
) -> Result<Vec<AgentSessionId>, SnapshotError> {
    // The lifecycle join is LEFT, not INNER, throughout this module: a row
    // naming a task that does not exist is a corrupt projection, and an inner
    // join would delete it from the page instead, turning corruption into
    // absence. Admitting it keeps the item loader's fail-closed check reachable.
    let sql = match projection {
        SnapshotProjection::Startup => format!(
            "SELECT a.agent_session_id FROM agent_sessions a
             LEFT JOIN tasks t ON t.task_id = a.task_id
             WHERE a.agent_session_id > ?1
               AND (t.task_id IS NULL OR t.lifecycle IN {STARTUP_DETAIL_LIFECYCLES})
             ORDER BY a.agent_session_id ASC LIMIT ?2"
        ),
        SnapshotProjection::Task(_) => "SELECT agent_session_id FROM agent_sessions
             WHERE agent_session_id > ?1 AND task_id = ?3
             ORDER BY agent_session_id ASC LIMIT ?2"
            .to_string(),
    };
    let after = after_agent.map(|id| id.as_bytes().to_vec());
    let rows = load_scoped_ids(conn, &sql, projection, after.as_deref(), fetch_limit)?;
    rows.iter()
        .map(|bytes| decode_agent_session_id(bytes))
        .collect()
}

fn load_artifact_ids(
    conn: &Connection,
    projection: SnapshotProjection,
    after_artifact: Option<ArtifactId>,
    fetch_limit: i64,
) -> Result<Vec<ArtifactId>, SnapshotError> {
    let sql = match projection {
        SnapshotProjection::Startup => format!(
            "SELECT a.artifact_id FROM artifacts a
             LEFT JOIN tasks t ON t.task_id = a.task_id
             WHERE a.artifact_id > ?1
               AND (t.task_id IS NULL OR t.lifecycle IN {STARTUP_DETAIL_LIFECYCLES})
             ORDER BY a.artifact_id ASC LIMIT ?2"
        ),
        SnapshotProjection::Task(_) => "SELECT artifact_id FROM artifacts
             WHERE artifact_id > ?1 AND task_id = ?3
             ORDER BY artifact_id ASC LIMIT ?2"
            .to_string(),
    };
    let after = after_artifact.map(|id| id.as_bytes().to_vec());
    let rows = load_scoped_ids(conn, &sql, projection, after.as_deref(), fetch_limit)?;
    rows.iter().map(|bytes| decode_artifact_id(bytes)).collect()
}

fn load_resource_ids(
    conn: &Connection,
    projection: SnapshotProjection,
    after_resource: Option<ResourceId>,
    fetch_limit: i64,
) -> Result<Vec<ResourceId>, SnapshotError> {
    // A host-owned resource has no task and is never withheld: the shell shows
    // it with no task selected at all.
    let sql = match projection {
        SnapshotProjection::Startup => format!(
            "SELECT r.resource_id FROM resources r
             LEFT JOIN tasks t ON t.task_id = r.task_id
             WHERE r.resource_id > ?1
               AND (r.task_id IS NULL
                    OR t.task_id IS NULL
                    OR t.lifecycle IN {STARTUP_DETAIL_LIFECYCLES})
             ORDER BY r.resource_id ASC LIMIT ?2"
        ),
        SnapshotProjection::Task(_) => "SELECT resource_id FROM resources
             WHERE resource_id > ?1 AND task_id = ?3
             ORDER BY resource_id ASC LIMIT ?2"
            .to_string(),
    };
    let after = after_resource.map(|id| id.as_bytes().to_vec());
    let rows = load_scoped_ids(conn, &sql, projection, after.as_deref(), fetch_limit)?;
    rows.iter().map(|bytes| decode_resource_id(bytes)).collect()
}

fn load_operation_ids(
    conn: &Connection,
    projection: SnapshotProjection,
    after_operation: Option<OperationId>,
    fetch_limit: i64,
) -> Result<Vec<OperationId>, SnapshotError> {
    // Unscoped: only operations that can still change, for any task, so
    // pending-action reconciliation keeps working. A settled operation is
    // history no surface reads.
    //
    // The task join is required in BOTH directions, and unlike every other
    // section it is not fail-open: the client refuses an operation whose parent
    // task it was not given, so shipping one would reject the whole startup
    // snapshot rather than surface one bad row. `purge` removes a task's
    // operations with the task, so a surviving orphan is already a purge fault;
    // and a task deleted between two sweeps still owns rows nothing may render.
    let sql = match projection {
        SnapshotProjection::Startup => format!(
            "SELECT o.operation_id FROM operations o
             LEFT JOIN tasks t ON t.task_id = o.task_id
             WHERE o.operation_id > ?1
               AND o.state IN {NON_TERMINAL_OPERATION_STATES}
               AND (o.task_id IS NULL
                    OR (t.task_id IS NOT NULL AND t.lifecycle <> 'deleted'))
             ORDER BY o.operation_id ASC LIMIT ?2"
        ),
        SnapshotProjection::Task(_) => "SELECT operation_id FROM operations
             WHERE operation_id > ?1 AND task_id = ?3
             ORDER BY operation_id ASC LIMIT ?2"
            .to_string(),
    };
    let after = after_operation.map(|id| id.as_bytes().to_vec());
    let rows = load_scoped_ids(conn, &sql, projection, after.as_deref(), fetch_limit)?;
    rows.iter()
        .map(|bytes| decode_operation_id(bytes))
        .collect()
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

fn decode_operation_id(bytes: &[u8]) -> Result<OperationId, SnapshotError> {
    let bytes: [u8; 16] = bytes.try_into().map_err(|_| StoreError::CodecMismatch {
        detail: "operations.operation_id must be 16 bytes".into(),
    })?;
    OperationId::from_bytes(bytes)
        .map_err(|error| StoreError::CodecMismatch {
            detail: format!("operations.operation_id: {error}"),
        })
        .map_err(Into::into)
}

fn canonical_page_encoded_bytes(
    snapshot_id: SnapshotId,
    through_sequence: u64,
    section: SnapshotSection,
    after_item: Option<SnapshotItemKey>,
    items: &[SnapshotItem],
    next_cursor: &Option<Vec<u8>>,
) -> Result<u32, SnapshotError> {
    let page = SnapshotPage {
        snapshot_id,
        through_sequence,
        section,
        after_item,
        items: items.to_vec(),
        encoded_bytes: 0,
        next_cursor: next_cursor.clone(),
    };
    canonical_snapshot_page_size(&page).map_err(snapshot_page_size_error)
}

fn snapshot_page_size_error(error: CanonicalPageSizeError) -> SnapshotError {
    SnapshotError::Store(match error {
        CanonicalPageSizeError::Encode { detail } => StoreError::CodecMismatch {
            detail: format!("encode snapshot page: {detail}"),
        },
        CanonicalPageSizeError::TooLarge { encoded_bytes } => StoreError::IntegerOutOfRange {
            field: "snapshot_page.encoded_bytes",
            value: u64::try_from(encoded_bytes).unwrap_or(u64::MAX),
        },
        CanonicalPageSizeError::DidNotConverge => StoreError::CodecMismatch {
            detail: "snapshot page encoded length did not converge".into(),
        },
    })
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
    /// Timing probe against a real store copy. Not a gate: run by hand with
    /// `DEVMANAGER_PROBE_STORE=<copy of kernel.sqlite3> cargo test --lib
    /// kernel::snapshot::tests::probe_snapshot_page_costs -- --ignored --nocapture`.
    /// Prints one PROBE line per page the way the client pages at startup, then
    /// splits the Tasks section into per-item load time and cumulative
    /// re-encode time so the two candidate costs are measured, not argued.
    #[test]
    #[ignore = "timing probe; needs DEVMANAGER_PROBE_STORE"]
    fn probe_snapshot_page_costs() {
        use std::time::Instant;
        let Ok(path) = std::env::var("DEVMANAGER_PROBE_STORE") else {
            eprintln!("SKIPPED probe_snapshot_page_costs: DEVMANAGER_PROBE_STORE not set");
            return;
        };
        let bus =
            command_bus::CommandBus::open(std::path::Path::new(&path)).expect("open store copy");
        let limits = PageLimits::new(128, 512 * 1024).expect("limits");
        let scope = crate::kernel::SessionScope {
            client_id: Some(crate::domain::id::ClientId::new()),
            task_id: None,
            connection_id: None,
            action_epoch: None,
            runtime_generation: None,
        };
        let session = bus.begin_snapshot_scoped(limits, scope).expect("session");
        let sections = [
            SnapshotSection::Tasks,
            SnapshotSection::AgentSessions,
            SnapshotSection::Artifacts,
            SnapshotSection::Resources,
            SnapshotSection::Operations,
            SnapshotSection::BrowserContexts,
            SnapshotSection::BrowserTabs,
        ];
        let whole = Instant::now();
        for section in sections {
            let mut cursor: Option<Vec<u8>> = None;
            let mut page_no = 0;
            loop {
                let started = Instant::now();
                let page = session.page(section, cursor.as_deref()).expect("page");
                let ms = started.elapsed().as_millis();
                page_no += 1;
                eprintln!(
                    "PROBE section={section:?} page={page_no} items={} encoded_bytes={} ms={ms}",
                    page.items.len(),
                    page.encoded_bytes
                );
                cursor = page.next_cursor.clone();
                if cursor.is_none() {
                    break;
                }
            }
        }
        eprintln!("PROBE all sections: {} ms", whole.elapsed().as_millis());

        // Cost split for Tasks.
        let ids = load_task_ids(&session.conn, SnapshotProjection::Startup, None, 1_000)
            .expect("task ids");
        let started = Instant::now();
        let mut items = Vec::new();
        for task_id in &ids {
            let snapshot = command_bus::load_task_snapshot(&session.conn, *task_id)
                .expect("load")
                .expect("present");
            items.push(SnapshotItem::Task(TaskSnapshotItem {
                task: snapshot.task,
                connectivity: snapshot.connectivity,
                attention: snapshot.attention,
                activity: snapshot.activity,
                review_readiness: snapshot.review_readiness,
                primary_agent_id: snapshot.primary_agent_id,
            }));
        }
        let load_ms = started.elapsed().as_millis();
        let started = Instant::now();
        let once = canonical_page_encoded_bytes(
            session.snapshot_id,
            session.through_sequence,
            SnapshotSection::Tasks,
            None,
            &items,
            &None,
        )
        .expect("encode");
        let encode_once_ms = started.elapsed().as_millis();
        let started = Instant::now();
        for n in 1..=items.len() {
            let _ = canonical_page_encoded_bytes(
                session.snapshot_id,
                session.through_sequence,
                SnapshotSection::Tasks,
                None,
                &items[..n],
                &None,
            )
            .expect("encode prefix");
        }
        let encode_cumulative_ms = started.elapsed().as_millis();
        eprintln!(
            "PROBE tasks={} load_all_ms={load_ms} encode_once_ms={encode_once_ms} ({once} bytes) encode_cumulative_ms={encode_cumulative_ms}",
            items.len()
        );
    }

    use std::time::Duration;

    use sha2::{Digest, Sha256};
    use tempfile::TempDir;

    use super::*;
    use crate::domain::agent::{AgentRole, AgentSessionFacts, AgentSessionLifecycle};
    use crate::domain::artifact::{ArtifactContentRef, ArtifactFacts, ArtifactKind, PrivacyClass};
    use crate::domain::command::{
        Command, CommandEnvelope, CommandReceipt, CreateTaskIntent, RenameTaskIntent,
    };
    use crate::domain::id::{
        AgentSessionId, ArtifactId, ClientId, CommandId, EnvironmentId, OperationId, ProjectId,
        ResourceId, TaskId,
    };
    use crate::domain::operation::OperationState;
    use crate::domain::resource::{
        OwnerKind, ResourceFacts, ResourceKind, ResourceLifecycle, ResourceRecipe,
    };
    use crate::domain::task::{
        ReviewReadiness, TaskActivity, TaskAssignment, TaskAttention, TaskConnectivity,
        WorkspaceRef,
    };
    use crate::kernel::dispatch::DispatchCompletion;
    use crate::providers::ProviderKind;
    use uuid::Uuid;

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

    fn create_task(store: &mut KernelStore, task_id: TaskId, command_id: CommandId) -> OperationId {
        create_task_with_title(store, task_id, command_id, "Ship kernel")
    }

    fn create_task_with_title(
        store: &mut KernelStore,
        task_id: TaskId,
        command_id: CommandId,
        title: &str,
    ) -> OperationId {
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
        match store
            .execute_for_test(envelope(
                command_id,
                None,
                None,
                Command::CreateTask(intent),
            ))
            .expect("create task")
        {
            CommandReceipt::Accepted { operation_id, .. } => operation_id,
            other => panic!("expected accepted create, got {other:?}"),
        }
    }

    fn agent_facts(
        task_id: TaskId,
        agent_session_id: AgentSessionId,
        role: AgentRole,
    ) -> AgentSessionFacts {
        AgentSessionFacts {
            id: agent_session_id,
            task_id,
            role,
            provider_kind: ProviderKind::Codex,
            provider_session_id: Some(
                format!("session-{}", agent_session_id)
                    .parse()
                    .expect("provider session"),
            ),
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
        register_agent_with_role(
            store,
            task_id,
            agent_session_id,
            command_id,
            expected_revision,
            AgentRole::Primary,
        );
    }

    fn register_agent_with_role(
        store: &mut KernelStore,
        task_id: TaskId,
        agent_session_id: AgentSessionId,
        command_id: CommandId,
        expected_revision: u64,
        role: AgentRole,
    ) {
        if matches!(role, AgentRole::Specialist { .. }) {
            let agent = agent_facts(task_id, agent_session_id, role);
            let connection = Connection::open(store.path()).expect("open agent projection");
            connection
                .execute(
                    "INSERT INTO agent_sessions(
                        agent_session_id, task_id, role, provider_kind,
                        provider_session_id, lifecycle, runtime_generation, revision
                    ) VALUES (?1, ?2, ?3, 'codex', ?4, 'open', 0, 0)",
                    rusqlite::params![
                        agent.id.as_bytes(),
                        agent.task_id.as_bytes(),
                        rmp_serde::to_vec(&agent.role).expect("encode agent role"),
                        agent.provider_session_id.as_ref().map(ToString::to_string),
                    ],
                )
                .expect("insert agent projection");
            return;
        }

        store
            .execute(envelope(
                command_id,
                Some(task_id),
                Some(expected_revision),
                Command::RegisterAgentSession {
                    agent: agent_facts(task_id, agent_session_id, role),
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
        let sha256 = {
            let mut hasher = Sha256::new();
            hasher.update(body.as_bytes());
            hasher.finalize().into()
        };
        ArtifactFacts {
            id: artifact_id,
            task_id,
            kind: ArtifactKind::Finding,
            label: label.into(),
            content_ref: ArtifactContentRef::inline_utf8(body).expect("artifact content"),
            sha256,
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
            recipe: ResourceRecipe::terminal(120, 40),
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

    /// Session scope naming one task, which is what a click on a Done or
    /// Archived task opens.
    fn task_scope(task: TaskId) -> SessionScope {
        SessionScope {
            client_id: Some(client_id(0x21)),
            task_id: Some(task),
            connection_id: None,
            action_epoch: None,
            runtime_generation: None,
        }
    }

    fn begin_close(store: &mut KernelStore, task: TaskId, command: CommandId) -> OperationId {
        let revision = task_revision(store.path(), task);
        match store
            .execute(envelope(
                command,
                Some(task),
                Some(revision),
                Command::BeginCloseTask,
            ))
            .expect("begin close")
        {
            CommandReceipt::Accepted { operation_id, .. } => operation_id,
            other => panic!("expected accepted close, got {other:?}"),
        }
    }

    fn task_revision(path: &std::path::Path, task: TaskId) -> u64 {
        let conn = Connection::open(path).expect("open revision reader");
        let revision: i64 = conn
            .query_row(
                "SELECT revision FROM tasks WHERE task_id = ?1",
                [task.as_bytes().as_slice()],
                |row| row.get(0),
            )
            .expect("task revision");
        u64::try_from(revision).expect("nonnegative revision")
    }

    fn set_task_lifecycle(path: &std::path::Path, task: TaskId, lifecycle: &str) {
        let conn = Connection::open(path).expect("open lifecycle writer");
        let changed = conn
            .execute(
                "UPDATE tasks SET lifecycle = ?2 WHERE task_id = ?1",
                rusqlite::params![task.as_bytes().as_slice(), lifecycle],
            )
            .expect("update lifecycle");
        assert_eq!(changed, 1, "lifecycle fixture must name a live task");
    }

    /// Remove a task row while leaving its operations behind.
    fn delete_task_row(path: &std::path::Path, task: TaskId) {
        let conn = Connection::open(path).expect("open task writer");
        conn.execute_batch("PRAGMA foreign_keys = OFF;")
            .expect("allow orphan fixture");
        let changed = conn
            .execute(
                "DELETE FROM tasks WHERE task_id = ?1",
                [task.as_bytes().as_slice()],
            )
            .expect("delete task row");
        assert_eq!(changed, 1, "the fixture must name a live task");
    }

    fn release_task_resources(path: &std::path::Path, task: TaskId) {
        let conn = Connection::open(path).expect("open resource writer");
        conn.execute(
            "UPDATE resources SET lifecycle = 'released' WHERE task_id = ?1",
            [task.as_bytes().as_slice()],
        )
        .expect("release resources");
    }

    fn section_item_count(session: &SnapshotSession, section: SnapshotSection) -> usize {
        let mut cursor: Option<Vec<u8>> = None;
        let mut count = 0;
        loop {
            let page = session.page(section, cursor.as_deref()).expect("page");
            count += page.items.len();
            cursor = page.next_cursor.clone();
            if cursor.is_none() {
                return count;
            }
        }
    }

    fn section_items(session: &SnapshotSession, section: SnapshotSection) -> Vec<SnapshotItem> {
        let mut cursor: Option<Vec<u8>> = None;
        let mut items = Vec::new();
        loop {
            let page = session.page(section, cursor.as_deref()).expect("page");
            items.extend(page.items.clone());
            cursor = page.next_cursor.clone();
            if cursor.is_none() {
                return items;
            }
        }
    }

    /// One store covering every task lifecycle twice over.
    ///
    /// The first four tasks carry an agent and a resource and run
    /// open / settled / archived / deleted; they are the subject of the DETAIL
    /// sections, and every operation they own has settled.
    ///
    /// The last two carry no children and are closed, so each owns one
    /// still-`accepted` operation; they are the subject of the OPERATIONS
    /// section. Their lifecycle is NOT rewritten: a live side-effect operation
    /// is validated against the durable task mutation chain, so a projection
    /// poke would make the operation itself unreadable. The second one's task
    /// row is deleted outright, which is what a purge leaves behind if it ever
    /// removed a task without its operations.
    fn lifecycle_fixture_store(path: &std::path::Path) -> (KernelStore, [TaskId; 4], [TaskId; 2]) {
        let mut store = KernelStore::open(path).expect("open");
        let detail_tasks = [task_id(0x41), task_id(0x42), task_id(0x43), task_id(0x44)];
        let operation_tasks = [task_id(0x45), task_id(0x46)];
        for (index, task) in detail_tasks.iter().copied().enumerate() {
            let index = u8::try_from(index).expect("fixture index fits");
            create_task(&mut store, task, command_id(0x50 + index));
            let revision = task_revision(path, task);
            register_agent(
                &mut store,
                task,
                agent_id(0x60 + index),
                command_id(0x70 + index),
                revision,
            );
            let revision = task_revision(path, task);
            register_resource(
                &mut store,
                task,
                resource_id(0x80 + index),
                command_id(0x90 + index),
                revision,
            );
        }
        for (index, task) in operation_tasks.iter().copied().enumerate() {
            let index = u8::try_from(index).expect("fixture index fits");
            create_task(&mut store, task, command_id(0x54 + index));
            begin_close(&mut store, task, command_id(0xA0 + index));
        }

        // An archived or deleted task may not own a live resource, so release
        // the fixture's before flipping the lifecycle.
        for task in [detail_tasks[2], detail_tasks[3]] {
            release_task_resources(path, task);
        }
        set_task_lifecycle(path, detail_tasks[0], "open");
        set_task_lifecycle(path, detail_tasks[1], "settled");
        set_task_lifecycle(path, detail_tasks[2], "archived");
        set_task_lifecycle(path, detail_tasks[3], "deleted");
        delete_task_row(path, operation_tasks[1]);
        (store, detail_tasks, operation_tasks)
    }

    #[test]
    fn startup_projection_ships_open_task_detail_only() {
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("kernel.sqlite3");
        let (store, detail_tasks, operation_tasks) = lifecycle_fixture_store(&path);
        let [open_task, settled_task, archived_task, deleted_task] = detail_tasks;

        let session = store
            .begin_snapshot(PageLimits::new(1_000, 512 * 1024).expect("limits"))
            .expect("begin startup snapshot");

        // The list still shows settled and archived; the deleted rows are gone.
        let listed: Vec<TaskId> = section_items(&session, SnapshotSection::Tasks)
            .into_iter()
            .map(|item| match item {
                SnapshotItem::Task(task) => task.task.id,
                other => panic!("expected task item, got {other:?}"),
            })
            .collect();
        assert!(listed.contains(&open_task));
        assert!(listed.contains(&settled_task));
        assert!(listed.contains(&archived_task));
        assert!(!listed.contains(&deleted_task));
        assert!(!listed.contains(&operation_tasks[1]));
        assert!(listed.contains(&operation_tasks[0]));
        assert_eq!(
            listed.len(),
            4,
            "one of six fixture tasks is deleted and one is purged: {listed:?}"
        );

        // Detail sections carry only the open task's rows.
        let agents: Vec<TaskId> = section_items(&session, SnapshotSection::AgentSessions)
            .into_iter()
            .map(|item| match item {
                SnapshotItem::AgentSession(agent) => agent.task_id,
                other => panic!("expected agent item, got {other:?}"),
            })
            .collect();
        assert_eq!(agents, vec![open_task]);

        let resources: Vec<Option<TaskId>> = section_items(&session, SnapshotSection::Resources)
            .into_iter()
            .map(|item| match item {
                SnapshotItem::Resource(resource) => resource.task_id,
                other => panic!("expected resource item, got {other:?}"),
            })
            .collect();
        assert_eq!(resources, vec![Some(open_task)]);

        // Operations: every non-terminal one, for every live task, and no
        // settled history at all.
        let operations: Vec<(Option<TaskId>, bool)> =
            section_items(&session, SnapshotSection::Operations)
                .into_iter()
                .map(|item| match item {
                    SnapshotItem::Operation(operation) => (
                        operation.task_id,
                        matches!(
                            operation.state,
                            OperationState::Accepted | OperationState::Uncertain { .. }
                        ),
                    ),
                    other => panic!("expected operation item, got {other:?}"),
                })
                .collect();
        assert!(
            operations.iter().all(|(_, non_terminal)| *non_terminal),
            "startup must never page a terminal operation: {operations:?}"
        );
        let operation_owners: Vec<Option<TaskId>> =
            operations.iter().map(|(task, _)| *task).collect();
        assert!(
            operation_owners.contains(&Some(operation_tasks[0])),
            "a live operation is still paged at startup: {operations:?}"
        );
        assert!(
            !operation_owners.contains(&Some(operation_tasks[1])),
            "an operation whose task row is gone has no parent to admit it"
        );
        for task in detail_tasks {
            assert!(
                !operation_owners.contains(&Some(task)),
                "every operation those tasks own has settled"
            );
        }
    }

    #[test]
    fn task_scoped_snapshot_ships_an_archived_task_detail() {
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("kernel.sqlite3");
        let (store, detail_tasks, _operation_tasks) = lifecycle_fixture_store(&path);

        let [open_task, _settled_task, archived_task, _deleted_task] = detail_tasks;

        let session = store
            .begin_snapshot_scoped(
                PageLimits::new(1_000, 512 * 1024).expect("limits"),
                task_scope(archived_task),
            )
            .expect("begin task-scoped snapshot");

        let listed: Vec<TaskId> = section_items(&session, SnapshotSection::Tasks)
            .into_iter()
            .map(|item| match item {
                SnapshotItem::Task(task) => task.task.id,
                other => panic!("expected task item, got {other:?}"),
            })
            .collect();
        assert_eq!(listed, vec![archived_task]);

        let agents: Vec<TaskId> = section_items(&session, SnapshotSection::AgentSessions)
            .into_iter()
            .map(|item| match item {
                SnapshotItem::AgentSession(agent) => agent.task_id,
                other => panic!("expected agent item, got {other:?}"),
            })
            .collect();
        assert_eq!(agents, vec![archived_task]);

        let resources: Vec<Option<TaskId>> = section_items(&session, SnapshotSection::Resources)
            .into_iter()
            .map(|item| match item {
                SnapshotItem::Resource(resource) => resource.task_id,
                other => panic!("expected resource item, got {other:?}"),
            })
            .collect();
        assert_eq!(resources, vec![Some(archived_task)]);

        // The scope is the whole filter: no other task's rows leak in, and the
        // scoped view does carry that task's settled operations, which the
        // startup projection withholds.
        let operations = section_items(&session, SnapshotSection::Operations);
        assert!(!operations.is_empty());
        assert!(operations.iter().any(|item| matches!(
            item,
            SnapshotItem::Operation(operation)
                if matches!(operation.state, OperationState::Settled { .. })
        )));
        for item in &operations {
            match item {
                SnapshotItem::Operation(operation) => {
                    assert_eq!(operation.task_id, Some(archived_task));
                }
                other => panic!("expected operation item, got {other:?}"),
            }
        }
        assert_ne!(archived_task, open_task);
        assert_eq!(
            section_item_count(&session, SnapshotSection::Artifacts),
            0,
            "the fixture registers no artifact"
        );
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
    fn scoped_snapshot_cursor_rejects_cross_scope_replay() {
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("kernel.sqlite3");
        let mut store = KernelStore::open(&path).expect("open");
        let scoped_task = task_id(0xE1);
        create_task(&mut store, scoped_task, command_id(0xE2));
        create_task(&mut store, task_id(0xE3), command_id(0xE4));
        // The scope is now a row filter, so the cursor's section must hold at
        // least two rows FOR THAT TASK or there is no continuation to fence.
        register_resource(
            &mut store,
            scoped_task,
            resource_id(0xE7),
            command_id(0xE8),
            task_revision(&path, scoped_task),
        );
        register_resource(
            &mut store,
            scoped_task,
            resource_id(0xE9),
            command_id(0xEA),
            task_revision(&path, scoped_task),
        );

        let limits = PageLimits::new(1, 512 * 1024).expect("limits");
        let scope = SessionScope {
            client_id: Some(client_id(0xE5)),
            task_id: Some(scoped_task),
            connection_id: Some(Uuid::now_v7()),
            action_epoch: Some(7),
            runtime_generation: Some(11),
        };
        let first = store
            .begin_snapshot_scoped(limits, scope)
            .expect("scoped snapshot");
        let cursor = first
            .page(SnapshotSection::Resources, None)
            .expect("first page")
            .next_cursor
            .expect("resume cursor");

        let other_scopes = [
            SessionScope {
                client_id: Some(ClientId::new()),
                ..scope
            },
            SessionScope {
                task_id: Some(TaskId::new()),
                ..scope
            },
            SessionScope {
                connection_id: Some(Uuid::now_v7()),
                ..scope
            },
            SessionScope {
                action_epoch: Some(scope.action_epoch.unwrap() + 1),
                ..scope
            },
            SessionScope {
                runtime_generation: Some(scope.runtime_generation.unwrap() + 1),
                ..scope
            },
        ];
        for other_scope in other_scopes {
            let mut other = store
                .begin_snapshot_scoped(limits, other_scope)
                .expect("other scoped snapshot");
            // Keep the other session's snapshot metadata and HMAC key equal to
            // the original. The rejection below therefore proves the exact
            // scope, rather than merely a different random session identity,
            // fences this cursor.
            other.snapshot_id = first.snapshot_id;
            other.through_sequence = first.through_sequence;
            other.cursor_hmac_key = first.cursor_hmac_key.clone();
            assert_eq!(
                other.page(SnapshotSection::Resources, Some(&cursor)),
                Err(SnapshotError::CursorContextMismatch)
            );
        }
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
            let role = if expected_revision == 1 {
                AgentRole::Primary
            } else {
                AgentRole::specialist(format!("snapshot-{agent_tail:x}")).expect("specialist role")
            };
            register_agent_with_role(
                &mut store,
                task,
                agent_id(agent_tail),
                command_id(command_tail),
                expected_revision,
                role,
            );
        }

        let snapshot = store
            .begin_snapshot(PageLimits::new(2, 512 * 1024).expect("limits"))
            .expect("begin snapshot");
        register_agent_with_role(
            &mut store,
            task,
            agent_id(0xE8),
            command_id(0xE9),
            4,
            AgentRole::specialist("snapshot-new").expect("specialist role"),
        );

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
        let huge_body = "x".repeat(8_192);
        register_artifact(
            &mut store,
            task,
            artifact_facts(task, huge_artifact, "Huge", huge_body.clone()),
            command_id(0xA5),
            2,
        );
        let snapshot = store
            .begin_snapshot(PageLimits::new(10, 2_048).expect("limits"))
            .expect("begin snapshot");
        let page = snapshot
            .page(SnapshotSection::Artifacts, None)
            .expect("metadata-only summaries fit both artifacts");
        assert_eq!(page.items.len(), 2);
        assert_eq!(page.next_cursor, None);
        assert_eq!(
            usize::try_from(page.encoded_bytes).expect("page length fits"),
            rmp_serde::to_vec_named(&page)
                .expect("encode final page")
                .len(),
        );
        let encoded = rmp_serde::to_vec_named(&page).expect("encode page");
        assert!(
            !encoded
                .windows(huge_body.len())
                .any(|window| window == huge_body.as_bytes()),
            "snapshot encoding must omit huge inline body"
        );
        let conn = store.open_query_connection().expect("query conn");
        let durable = command_bus::load_artifact(&conn, huge_artifact)
            .expect("load")
            .expect("huge artifact");
        assert!(matches!(
            durable.content_ref,
            ArtifactContentRef::InlineUtf8(body) if body == huge_body
        ));
    }

    #[test]
    fn artifact_snapshot_omits_inline_body_until_requested() {
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("kernel.sqlite3");
        let mut store = KernelStore::open(&path).expect("open");
        let task = task_id(0xC0);
        let artifact = artifact_id(0xC1);
        create_task(&mut store, task, command_id(0xC2));
        const BODY: &str = "DISTINCTIVE_ARTIFACT_BODY_TOKEN_2_5E";
        const LABEL: &str = "DistinctiveLabelToken";
        register_artifact(
            &mut store,
            task,
            artifact_facts(task, artifact, LABEL, BODY.into()),
            command_id(0xC3),
            1,
        );

        let snapshot = store
            .begin_snapshot(PageLimits::new(10, 512 * 1024).expect("limits"))
            .expect("begin snapshot");
        let page = snapshot
            .page(SnapshotSection::Artifacts, None)
            .expect("artifacts page");
        let encoded = rmp_serde::to_vec_named(&page).expect("encode artifacts page");
        assert!(
            !encoded
                .windows(BODY.len())
                .any(|window| window == BODY.as_bytes()),
            "snapshot must not inline artifact body bytes"
        );
        assert!(
            encoded
                .windows(LABEL.len())
                .any(|window| window == LABEL.as_bytes()),
            "snapshot must retain artifact label"
        );
        let summary = match &page.items[..] {
            [SnapshotItem::Artifact(summary)] => summary,
            other => panic!("expected one artifact summary, got {other:?}"),
        };
        assert_eq!(summary.id, artifact);
        assert_eq!(summary.label, LABEL);
        let mut expected_digest = Sha256::new();
        expected_digest.update(BODY.as_bytes());
        let expected_digest: [u8; 32] = expected_digest.finalize().into();
        assert_eq!(
            summary.sha256, expected_digest,
            "decoded summary hash must equal the real content SHA-256"
        );

        let conn = store.open_query_connection().expect("query conn");
        let durable = command_bus::load_artifact(&conn, artifact)
            .expect("load")
            .expect("artifact");
        assert!(matches!(
            durable.content_ref,
            ArtifactContentRef::InlineUtf8(body) if body == BODY
        ));
        assert_eq!(durable.sha256, expected_digest);
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
                rmp_serde::to_vec(&ResourceRecipe::terminal(120, 40))
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
    fn snapshot_operations_pages_are_frozen_and_resume_without_duplicates() {
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("kernel.sqlite3");
        let mut store = KernelStore::open(&path).expect("open");
        // Three tasks, each with one still-accepted close operation. Create
        // operations settle inside the test fixture, and the startup projection
        // never pages a terminal operation, so the closes are the subjects.
        let first_task = task_id(0xD0);
        let second_task = task_id(0xD1);
        let third_task = task_id(0xD8);
        create_task(&mut store, first_task, command_id(0xD2));
        create_task(&mut store, second_task, command_id(0xD3));
        create_task(&mut store, third_task, command_id(0xD9));
        let close_first_command = command_id(0xD4);
        let close_second_command = command_id(0xDC);
        let close_third_command = command_id(0xDD);
        let close_first = begin_close(&mut store, first_task, close_first_command);
        let close_second = begin_close(&mut store, second_task, close_second_command);
        let close_third = begin_close(&mut store, third_task, close_third_command);
        let mut expected_ids = vec![close_first, close_second, close_third];
        expected_ids.sort();

        let snapshot = store
            .begin_snapshot(PageLimits::new(2, 512 * 1024).expect("limits"))
            .expect("begin snapshot");

        let claim = store
            .claim_next_dispatch(Duration::from_secs(30))
            .expect("claim dispatch")
            .expect("dispatch ready");
        let permit = store.begin_dispatch(&claim).expect("begin dispatch");
        assert!(matches!(
            store
                .record_dispatch_completion(&permit, DispatchCompletion::Settled)
                .expect("settle close"),
            OperationState::Settled { .. }
        ));
        create_task(&mut store, task_id(0xD5), command_id(0xD6));
        let post_snapshot = begin_close(&mut store, task_id(0xD5), command_id(0xDE));

        let first = snapshot
            .page(SnapshotSection::Operations, None)
            .expect("first operation page");
        let cursor = first.next_cursor.clone().expect("operation resume cursor");
        let second = snapshot
            .page(SnapshotSection::Operations, Some(&cursor))
            .expect("second operation page");

        assert_eq!(first.items.len(), 2);
        assert_eq!(second.items.len(), 1);
        assert_eq!(first.snapshot_id, second.snapshot_id);
        assert_eq!(first.through_sequence, second.through_sequence);
        assert_eq!(
            second.after_item,
            Some(SnapshotItemKey::Operation(expected_ids[1]))
        );
        assert_eq!(second.next_cursor, None);
        for page in [&first, &second] {
            assert_eq!(
                usize::try_from(page.encoded_bytes).expect("page length fits"),
                rmp_serde::to_vec_named(page)
                    .expect("encode operation page")
                    .len(),
            );
        }

        let operations = first
            .items
            .iter()
            .chain(second.items.iter())
            .map(|item| match item {
                SnapshotItem::Operation(operation) => operation,
                other => panic!("expected operation item, got {other:?}"),
            })
            .collect::<Vec<_>>();
        assert_eq!(
            operations
                .iter()
                .map(|operation| operation.id)
                .collect::<Vec<_>>(),
            expected_ids
        );
        assert!(!operations
            .iter()
            .any(|operation| operation.id == post_snapshot));

        for (id, command_id, task_id) in [
            (close_first, close_first_command, first_task),
            (close_second, close_second_command, second_task),
            (close_third, close_third_command, third_task),
        ] {
            let facts = operations
                .iter()
                .find(|operation| operation.id == id)
                .expect("close operation");
            assert_eq!(facts.command_id, command_id);
            assert_eq!(facts.task_id, Some(task_id));
            // Pinned before the dispatch settle below, so the frozen view still
            // reports the state it had when the session opened.
            assert_eq!(facts.state, OperationState::Accepted);
        }
    }

    #[test]
    fn snapshot_operations_reject_invalid_durable_lineage() {
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("kernel.sqlite3");
        let mut store = KernelStore::open(&path).expect("open");
        let command = command_id(0xD8);
        create_task(&mut store, task_id(0xD7), command);

        let conn = Connection::open(&path).expect("open tamper connection");
        conn.execute(
            "UPDATE command_receipts SET receipt = X'00' WHERE command_id = ?1",
            [command.as_bytes().as_slice()],
        )
        .expect("tamper receipt lineage");
        drop(conn);

        // The subject is a settled create operation, which only the
        // task-scoped projection pages.
        let snapshot = store
            .begin_snapshot_scoped(
                PageLimits::new(10, 512 * 1024).expect("limits"),
                task_scope(task_id(0xD7)),
            )
            .expect("begin snapshot");
        assert!(matches!(
            snapshot
                .page(SnapshotSection::Operations, None)
                .expect_err("invalid operation lineage must fail closed"),
            SnapshotError::Store(StoreError::Projection(_))
        ));
    }

    #[test]
    fn snapshot_operations_report_oversized_item_identity() {
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("kernel.sqlite3");
        let mut store = KernelStore::open(&path).expect("open");
        let operation = create_task(&mut store, task_id(0xDA), command_id(0xDB));

        // The subject is a settled create operation, which only the task-scoped
        // projection pages.
        let snapshot = store
            .begin_snapshot_scoped(
                PageLimits::new(10, 1).expect("limits"),
                task_scope(task_id(0xDA)),
            )
            .expect("begin snapshot");
        assert!(matches!(
            snapshot.page(SnapshotSection::Operations, None),
            Err(SnapshotError::PageItemTooLarge {
                item: SnapshotItemKey::Operation(id),
                ..
            }) if id == operation
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
