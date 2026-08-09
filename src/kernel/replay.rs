use std::fmt;

use hmac::{Hmac, Mac};
use rusqlite::{Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use zeroize::Zeroizing;

use crate::domain::event::DomainEvent;
use crate::domain::id::SubscriptionId;
use crate::domain::snapshot::{
    canonical_event_page_size, CanonicalPageSizeError, EventPage, PageLimits, PageLimitsError,
};
use crate::kernel::store::{
    decode_stored_domain_event, load_event_log_bounds, u64_from_nonnegative_i64, u64_to_sqlite_i64,
    KernelStore, StoreError,
};

const EVENT_CURSOR_VERSION: u16 = 1;
const EVENT_CURSOR_DOMAIN: &[u8] = b"devmanager:event-cursor:v1\0";
const CURSOR_TAG_BYTES: usize = 32;
const MAX_CURSOR_BYTES: usize = 4_096;

type HmacSha256 = Hmac<Sha256>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ReplayError {
    Store(StoreError),
    InvalidLimits(PageLimitsError),
    EntropyUnavailable,
    InvalidRange {
        after_sequence: u64,
        through_sequence: u64,
    },
    ReplayUnavailable {
        oldest_sequence: u64,
        newest_sequence: u64,
    },
    InvalidCursor,
    CursorContextMismatch,
    PageEnvelopeTooLarge {
        encoded_bytes: u32,
        max_encoded_bytes: u32,
    },
    PageItemTooLarge {
        sequence: u64,
        encoded_bytes: u32,
        max_encoded_bytes: u32,
    },
}

impl fmt::Display for ReplayError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Store(error) => error.fmt(f),
            Self::InvalidLimits(error) => error.fmt(f),
            Self::EntropyUnavailable => write!(f, "event replay cursor entropy unavailable"),
            Self::InvalidRange {
                after_sequence,
                through_sequence,
            } => write!(
                f,
                "event replay starts after {after_sequence}, beyond newest sequence {through_sequence}"
            ),
            Self::ReplayUnavailable {
                oldest_sequence,
                newest_sequence,
            } => write!(
                f,
                "event replay is unavailable before sequence {oldest_sequence}; newest sequence is {newest_sequence}"
            ),
            Self::InvalidCursor => write!(f, "invalid event replay cursor"),
            Self::CursorContextMismatch => write!(f, "event replay cursor context mismatch"),
            Self::PageEnvelopeTooLarge {
                encoded_bytes,
                max_encoded_bytes,
            } => write!(
                f,
                "event page envelope is {encoded_bytes} bytes, exceeding {max_encoded_bytes}"
            ),
            Self::PageItemTooLarge {
                sequence,
                encoded_bytes,
                max_encoded_bytes,
            } => write!(
                f,
                "event {sequence} requires a {encoded_bytes}-byte page, exceeding {max_encoded_bytes}"
            ),
        }
    }
}

impl std::error::Error for ReplayError {}

impl From<StoreError> for ReplayError {
    fn from(error: StoreError) -> Self {
        Self::Store(error)
    }
}

impl From<PageLimitsError> for ReplayError {
    fn from(error: PageLimitsError) -> Self {
        Self::InvalidLimits(error)
    }
}

impl From<rusqlite::Error> for ReplayError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Store(StoreError::from(error))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct EventCursorDocument {
    version: u16,
    start_after_sequence: u64,
    through_sequence: u64,
    last_sequence: u64,
    limits: PageLimits,
}

/// One immutable, read-only SQLite view of an ordered durable event range.
///
/// The owned connection holds a deferred read transaction open. Dropping this
/// value releases the view; no OS process or other runtime resource is owned.
pub(crate) struct EventReplaySession {
    subscription_id: SubscriptionId,
    start_after_sequence: u64,
    through_sequence: u64,
    limits: PageLimits,
    cursor_hmac_key: Zeroizing<[u8; 32]>,
    conn: Connection,
}

impl fmt::Debug for EventReplaySession {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EventReplaySession")
            .field("subscription_id", &self.subscription_id)
            .field("start_after_sequence", &self.start_after_sequence)
            .field("through_sequence", &self.through_sequence)
            .field("limits", &self.limits)
            .finish_non_exhaustive()
    }
}

impl KernelStore {
    /// Pin a read-only event replay at the current durable high-water mark.
    pub(crate) fn begin_event_replay(
        &self,
        after_sequence: u64,
        limits: PageLimits,
    ) -> Result<EventReplaySession, ReplayError> {
        limits.validate()?;
        let mut cursor_hmac_key = Zeroizing::new([0u8; 32]);
        getrandom::fill(cursor_hmac_key.as_mut()).map_err(|_| ReplayError::EntropyUnavailable)?;
        let conn = self.open_query_connection()?;
        conn.execute_batch("BEGIN DEFERRED;")?;
        let bounds = load_event_log_bounds(&conn)?;
        if after_sequence < bounds.pruned_through_sequence {
            return Err(ReplayError::ReplayUnavailable {
                oldest_sequence: bounds.pruned_through_sequence + 1,
                newest_sequence: bounds.newest_sequence,
            });
        }
        let through_sequence = bounds.newest_sequence;
        if after_sequence > through_sequence {
            return Err(ReplayError::InvalidRange {
                after_sequence,
                through_sequence,
            });
        }
        Ok(EventReplaySession {
            subscription_id: SubscriptionId::new(),
            start_after_sequence: after_sequence,
            through_sequence,
            limits,
            cursor_hmac_key,
            conn,
        })
    }
}

impl EventReplaySession {
    pub(crate) fn subscription_id(&self) -> SubscriptionId {
        self.subscription_id
    }

    /// Read one bounded event page from the view pinned by `begin_event_replay`.
    pub(crate) fn page(&self, resume_cursor: Option<&[u8]>) -> Result<EventPage, ReplayError> {
        let after_sequence = match resume_cursor {
            Some(cursor) => self.decode_cursor(cursor)?.last_sequence,
            None => self.start_after_sequence,
        };
        let fetch_limit = i64::from(self.limits.max_items) + 1;
        let sequences = load_event_sequences_after(
            &self.conn,
            after_sequence,
            self.through_sequence,
            fetch_limit,
        )?;
        self.assemble_page(after_sequence, sequences)
    }

    fn assemble_page(
        &self,
        after_sequence: u64,
        sequences: Vec<u64>,
    ) -> Result<EventPage, ReplayError> {
        let max_items = usize::try_from(self.limits.max_items)
            .expect("validated u32 replay item limit fits usize");
        let mut events = Vec::with_capacity(sequences.len().min(max_items));
        let mut accepted_next_cursor = None;
        for (index, sequence) in sequences.iter().take(max_items).copied().enumerate() {
            let event =
                load_event_at_sequence(&self.conn, sequence)?.ok_or(StoreError::Corruption)?;
            events.push(event);

            let has_more = index + 1 < sequences.len();
            let next_cursor = if has_more {
                Some(self.encode_cursor(sequence)?)
            } else {
                None
            };
            let encoded_bytes = canonical_event_page_encoded_bytes(
                after_sequence,
                self.through_sequence,
                &events,
                &next_cursor,
            )?;
            if encoded_bytes > self.limits.max_encoded_bytes {
                events.pop();
                if events.is_empty() {
                    return Err(ReplayError::PageItemTooLarge {
                        sequence,
                        encoded_bytes,
                        max_encoded_bytes: self.limits.max_encoded_bytes,
                    });
                }
                break;
            }
            accepted_next_cursor = next_cursor;
        }

        if events.is_empty() {
            let encoded_bytes = canonical_event_page_encoded_bytes(
                after_sequence,
                self.through_sequence,
                &events,
                &None,
            )?;
            if encoded_bytes > self.limits.max_encoded_bytes {
                return Err(ReplayError::PageEnvelopeTooLarge {
                    encoded_bytes,
                    max_encoded_bytes: self.limits.max_encoded_bytes,
                });
            }
        }

        Ok(EventPage {
            after_sequence,
            through_sequence: self.through_sequence,
            events,
            next_cursor: accepted_next_cursor,
        })
    }

    fn encode_cursor(&self, last_sequence: u64) -> Result<Vec<u8>, ReplayError> {
        if last_sequence <= self.start_after_sequence || last_sequence > self.through_sequence {
            return Err(ReplayError::InvalidCursor);
        }
        let document = EventCursorDocument {
            version: EVENT_CURSOR_VERSION,
            start_after_sequence: self.start_after_sequence,
            through_sequence: self.through_sequence,
            last_sequence,
            limits: self.limits,
        };
        let payload =
            rmp_serde::to_vec_named(&document).map_err(|error| StoreError::CodecMismatch {
                detail: format!("encode event replay cursor: {error}"),
            })?;
        let mut mac = HmacSha256::new_from_slice(self.cursor_hmac_key.as_ref())
            .map_err(|_| ReplayError::InvalidCursor)?;
        mac.update(EVENT_CURSOR_DOMAIN);
        mac.update(&payload);
        let tag = mac.finalize().into_bytes();
        let mut cursor = Vec::with_capacity(payload.len() + tag.len());
        cursor.extend_from_slice(&payload);
        cursor.extend_from_slice(&tag);
        Ok(cursor)
    }

    fn decode_cursor(&self, cursor: &[u8]) -> Result<EventCursorDocument, ReplayError> {
        if cursor.len() <= CURSOR_TAG_BYTES || cursor.len() > MAX_CURSOR_BYTES {
            return Err(ReplayError::InvalidCursor);
        }
        let (payload, tag) = cursor.split_at(cursor.len() - CURSOR_TAG_BYTES);
        let mut mac = HmacSha256::new_from_slice(self.cursor_hmac_key.as_ref())
            .map_err(|_| ReplayError::InvalidCursor)?;
        mac.update(EVENT_CURSOR_DOMAIN);
        mac.update(payload);
        mac.verify_slice(tag)
            .map_err(|_| ReplayError::InvalidCursor)?;

        let document: EventCursorDocument =
            rmp_serde::from_slice(payload).map_err(|_| ReplayError::InvalidCursor)?;
        let canonical =
            rmp_serde::to_vec_named(&document).map_err(|_| ReplayError::InvalidCursor)?;
        if canonical.as_slice() != payload || document.version != EVENT_CURSOR_VERSION {
            return Err(ReplayError::InvalidCursor);
        }
        document.limits.validate()?;
        if document.start_after_sequence != self.start_after_sequence
            || document.through_sequence != self.through_sequence
            || document.limits != self.limits
        {
            return Err(ReplayError::CursorContextMismatch);
        }
        if document.last_sequence <= document.start_after_sequence
            || document.last_sequence > document.through_sequence
        {
            return Err(ReplayError::InvalidCursor);
        }
        Ok(document)
    }
}

fn load_event_sequences_after(
    conn: &Connection,
    after_sequence: u64,
    through_sequence: u64,
    fetch_limit: i64,
) -> Result<Vec<u64>, ReplayError> {
    let mut stmt = conn.prepare(
        "SELECT sequence FROM events
         WHERE sequence > ?1 AND sequence <= ?2
         ORDER BY sequence ASC LIMIT ?3",
    )?;
    let rows = stmt.query_map(
        rusqlite::params![
            u64_to_sqlite_i64("event_replay.after_sequence", after_sequence)?,
            u64_to_sqlite_i64("event_replay.through_sequence", through_sequence)?,
            fetch_limit,
        ],
        |row| row.get::<_, i64>(0),
    )?;
    let mut sequences = Vec::new();
    for row in rows {
        let sequence = u64_from_nonnegative_i64("events.sequence", row?)?;
        if sequence <= after_sequence || sequence > through_sequence {
            return Err(StoreError::Corruption.into());
        }
        if sequences.last().is_some_and(|prior| sequence <= *prior) {
            return Err(StoreError::Corruption.into());
        }
        sequences.push(sequence);
    }
    Ok(sequences)
}

fn load_event_at_sequence(
    conn: &Connection,
    expected_sequence: u64,
) -> Result<Option<DomainEvent>, ReplayError> {
    let row: Option<(
        i64,
        Vec<u8>,
        Option<Vec<u8>>,
        Option<i64>,
        String,
        i64,
        i64,
        Vec<u8>,
    )> = conn
        .query_row(
            "SELECT sequence, event_id, task_id, task_revision, event_type, schema_version,
                    occurred_at_ms, payload
             FROM events WHERE sequence = ?1",
            [u64_to_sqlite_i64(
                "event_replay.expected_sequence",
                expected_sequence,
            )?],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                    row.get(7)?,
                ))
            },
        )
        .optional()?;
    let Some((
        sequence,
        event_id,
        task_id,
        task_revision,
        event_type,
        schema_version,
        occurred_at_ms,
        payload,
    )) = row
    else {
        return Ok(None);
    };
    let event = decode_stored_domain_event(
        sequence,
        &event_id,
        task_id.as_deref(),
        task_revision,
        &event_type,
        schema_version,
        occurred_at_ms,
        &payload,
    )?;
    if event.sequence != expected_sequence {
        return Err(StoreError::Corruption.into());
    }
    Ok(Some(event))
}

fn canonical_event_page_encoded_bytes(
    after_sequence: u64,
    through_sequence: u64,
    events: &[DomainEvent],
    next_cursor: &Option<Vec<u8>>,
) -> Result<u32, ReplayError> {
    let page = EventPage {
        after_sequence,
        through_sequence,
        events: events.to_vec(),
        next_cursor: next_cursor.clone(),
    };
    canonical_event_page_size(&page).map_err(|error| {
        ReplayError::Store(match error {
            CanonicalPageSizeError::Encode { detail } => StoreError::CodecMismatch {
                detail: format!("encode event page: {detail}"),
            },
            CanonicalPageSizeError::TooLarge { encoded_bytes } => StoreError::IntegerOutOfRange {
                field: "event_page.encoded_bytes",
                value: u64::try_from(encoded_bytes).unwrap_or(u64::MAX),
            },
            CanonicalPageSizeError::DidNotConverge => StoreError::CodecMismatch {
                detail: "event page encoded length did not converge".into(),
            },
        })
    })
}

impl Drop for EventReplaySession {
    fn drop(&mut self) {
        let _ = self.conn.execute_batch("ROLLBACK;");
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use rusqlite::Connection;
    use tempfile::TempDir;

    use super::*;
    use crate::domain::artifact::{ArtifactContentRef, ArtifactFacts, ArtifactKind, PrivacyClass};
    use crate::domain::command::{Command, CommandEnvelope, CommandReceipt, CreateTaskIntent};
    use crate::domain::id::{ArtifactId, ClientId, CommandId, EnvironmentId, ProjectId, TaskId};
    use crate::domain::snapshot::SnapshotSection;
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
    test_id!(artifact_id, ArtifactId);

    fn envelope(
        command_id: CommandId,
        task_id: Option<TaskId>,
        expected_task_revision: Option<u64>,
        command: Command,
    ) -> CommandEnvelope {
        CommandEnvelope {
            command_id,
            client_id: client_id(0x01),
            task_id,
            issued_at_ms: 1_725_000_000_100,
            expected_task_revision,
            command,
        }
    }

    fn create_task(store: &mut KernelStore, tail: u8) {
        let task = task_id(tail);
        let intent = CreateTaskIntent {
            id: task,
            environment_id: environment_id(0x02),
            title: format!("Replay task {tail}"),
            description: Some("ordered replay proof".into()),
            project_id: project_id(0x03),
            workspace: WorkspaceRef::Main,
            assignment: TaskAssignment::LocalOwner,
            created_at_ms: 1_725_000_000_000,
            connectivity: TaskConnectivity::Connected,
            attention: TaskAttention::None,
            activity: TaskActivity::Idle,
            review_readiness: ReviewReadiness::NotReady,
        };
        assert!(matches!(
            store
                .execute(envelope(
                    command_id(tail.wrapping_add(0x40)),
                    None,
                    None,
                    Command::CreateTask(intent),
                ))
                .expect("create task"),
            CommandReceipt::Accepted { .. }
        ));
    }

    fn register_large_artifact(store: &mut KernelStore, task: TaskId) {
        let artifact = ArtifactFacts {
            id: artifact_id(0xF0),
            task_id: task,
            kind: ArtifactKind::Evidence,
            label: "Large replay event".into(),
            content_ref: ArtifactContentRef::inline_utf8("x".repeat(8_192))
                .expect("large inline artifact"),
            sha256: [0xA5; 32],
            privacy_class: PrivacyClass::LocalOnly,
            created_at_ms: 1_725_000_000_200,
        };
        assert!(matches!(
            store
                .execute(envelope(
                    command_id(0xF1),
                    Some(task),
                    Some(1),
                    Command::RegisterArtifact { artifact },
                ))
                .expect("register artifact"),
            CommandReceipt::Accepted { .. }
        ));
    }

    fn max_sequence(path: &Path) -> u64 {
        let conn = Connection::open(path).expect("open observer");
        let sequence: i64 = conn
            .query_row("SELECT COALESCE(MAX(sequence), 0) FROM events", [], |row| {
                row.get(0)
            })
            .expect("maximum sequence");
        u64::try_from(sequence).expect("nonnegative sequence")
    }

    #[test]
    fn events_after_cursor_are_strictly_ordered_frozen_and_resume_without_duplicates() {
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("kernel.sqlite3");
        let mut store = KernelStore::open(&path).expect("open");
        create_task(&mut store, 0x10);
        let start_after_sequence = max_sequence(&path);
        create_task(&mut store, 0x11);
        let through_sequence = max_sequence(&path);

        let replay = store
            .begin_event_replay(
                start_after_sequence,
                PageLimits::new(2, 512 * 1024).expect("limits"),
            )
            .expect("begin replay");
        create_task(&mut store, 0x12);

        let mut resume = None;
        let mut expected_after = start_after_sequence;
        let mut sequences = Vec::new();
        for _ in 0..100 {
            let page = replay.page(resume.as_deref()).expect("ordered replay page");
            assert_eq!(page.after_sequence, expected_after);
            assert_eq!(page.through_sequence, through_sequence);
            assert!(page.events.len() <= 2);
            for event in &page.events {
                assert!(event.sequence > page.after_sequence);
                assert!(event.sequence <= page.through_sequence);
            }
            sequences.extend(page.events.iter().map(|event| event.sequence));
            let Some(cursor) = page.next_cursor else {
                resume = None;
                break;
            };
            expected_after = page.events.last().expect("cursor requires event").sequence;
            resume = Some(cursor);
        }
        assert!(resume.is_none(), "replay did not terminate");
        assert!(sequences.windows(2).all(|pair| pair[0] < pair[1]));
        assert_eq!(
            sequences,
            (start_after_sequence + 1..=through_sequence).collect::<Vec<_>>(),
            "pinned replay must contain each original event exactly once"
        );
        assert!(max_sequence(&path) > through_sequence);
    }

    #[test]
    fn event_page_honors_item_and_exact_encoded_byte_limits() {
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("kernel.sqlite3");
        let mut store = KernelStore::open(&path).expect("open");
        for tail in 0x20..0x2A {
            create_task(&mut store, tail);
        }
        let limits = PageLimits::new(100, 2_048).expect("limits");
        let replay = store.begin_event_replay(0, limits).expect("begin replay");

        let mut resume = None;
        let mut sequences = Vec::new();
        let mut page_count = 0;
        for _ in 0..100 {
            let page = replay.page(resume.as_deref()).expect("bounded page");
            page_count += 1;
            let encoded = rmp_serde::to_vec_named(&page).expect("encode event page");
            assert_eq!(
                u32::try_from(encoded.len()).expect("page length fits"),
                canonical_event_page_encoded_bytes(
                    page.after_sequence,
                    page.through_sequence,
                    &page.events,
                    &page.next_cursor,
                )
                .expect("measure canonical page"),
            );
            assert!(encoded.len() <= 2_048);
            assert!(page.events.len() <= 100);
            sequences.extend(page.events.iter().map(|event| event.sequence));
            resume = page.next_cursor;
            if resume.is_none() {
                break;
            }
        }
        assert!(resume.is_none(), "bounded replay did not terminate");
        assert!(page_count > 1, "byte limit should split this replay");
        assert!(sequences.windows(2).all(|pair| pair[0] < pair[1]));
        assert_eq!(sequences.last().copied(), Some(max_sequence(&path)));
    }

    #[test]
    fn event_page_reports_oversized_single_event() {
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("kernel.sqlite3");
        let mut store = KernelStore::open(&path).expect("open");
        let task = task_id(0x30);
        create_task(&mut store, 0x30);
        let after_sequence = max_sequence(&path);
        register_large_artifact(&mut store, task);

        let replay = store
            .begin_event_replay(after_sequence, PageLimits::new(10, 2_048).expect("limits"))
            .expect("begin replay");
        assert!(matches!(
            replay.page(None),
            Err(ReplayError::PageItemTooLarge { sequence, .. })
                if sequence == after_sequence + 1
        ));
    }

    #[test]
    fn event_cursor_rejects_tampering_and_wrong_context() {
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("kernel.sqlite3");
        let mut store = KernelStore::open(&path).expect("open");
        create_task(&mut store, 0x40);
        create_task(&mut store, 0x41);
        let limits = PageLimits::new(1, 512 * 1024).expect("limits");
        let replay = store.begin_event_replay(0, limits).expect("begin replay");
        let first = replay.page(None).expect("first page");
        let cursor = first.next_cursor.expect("resume cursor");

        let mut tampered = cursor.clone();
        tampered[0] ^= 0x01;
        assert!(matches!(
            replay.page(Some(&tampered)),
            Err(ReplayError::InvalidCursor)
        ));

        let other = store.begin_event_replay(0, limits).expect("other replay");
        assert!(matches!(
            other.page(Some(&cursor)),
            Err(ReplayError::InvalidCursor)
        ));

        let snapshot = store.begin_snapshot(limits).expect("begin snapshot");
        let snapshot_cursor = snapshot
            .page(SnapshotSection::Tasks, None)
            .expect("snapshot page")
            .next_cursor
            .expect("snapshot cursor");
        assert!(matches!(
            replay.page(Some(&snapshot_cursor)),
            Err(ReplayError::InvalidCursor)
        ));

        assert!(matches!(
            store.begin_event_replay(first.through_sequence + 1, limits),
            Err(ReplayError::InvalidRange { .. })
        ));
    }

    #[test]
    fn expired_replay_returns_retention_bounds_and_exact_edge_resumes() {
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("kernel.sqlite3");
        let mut store = KernelStore::open(&path).expect("open");
        create_task(&mut store, 0x45);
        create_task(&mut store, 0x46);
        create_task(&mut store, 0x47);
        let newest_sequence = max_sequence(&path);
        let pruned_through_sequence = newest_sequence - 2;

        let conn = Connection::open(&path).expect("open retention fixture");
        conn.execute(
            "UPDATE event_retention SET pruned_through_sequence = ?1 WHERE singleton_key = 1",
            [i64::try_from(pruned_through_sequence).expect("prune boundary fits")],
        )
        .expect("advance explicit retention boundary");
        conn.execute(
            "DELETE FROM events WHERE sequence = ?1",
            [i64::try_from(pruned_through_sequence + 1).expect("gap sequence fits")],
        )
        .expect("create a retained sequence gap");
        drop(conn);

        let limits = PageLimits::new(100, 512 * 1024).expect("limits");
        assert!(matches!(
            store.begin_event_replay(pruned_through_sequence - 1, limits),
            Err(ReplayError::ReplayUnavailable {
                oldest_sequence,
                newest_sequence: reported_newest,
            }) if oldest_sequence == pruned_through_sequence + 1
                && reported_newest == newest_sequence
        ));

        let replay = store
            .begin_event_replay(pruned_through_sequence, limits)
            .expect("exact retention edge remains replayable");
        let page = replay.page(None).expect("replay retained suffix");
        assert_eq!(page.after_sequence, pruned_through_sequence);
        assert_eq!(page.through_sequence, newest_sequence);
        assert_eq!(
            page.events
                .iter()
                .map(|event| event.sequence)
                .collect::<Vec<_>>(),
            vec![newest_sequence],
            "ordinary gaps after the boundary must not be mistaken for pruning"
        );
    }

    #[test]
    fn expired_cursor_requires_snapshot() {
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("kernel.sqlite3");
        let mut store = KernelStore::open(&path).expect("open");
        create_task(&mut store, 0x4B);
        create_task(&mut store, 0x4C);
        create_task(&mut store, 0x4D);
        let newest_sequence = max_sequence(&path);
        let pruned_through_sequence = newest_sequence - 1;

        let conn = Connection::open(&path).expect("open retention fixture");
        conn.execute(
            "UPDATE event_retention SET pruned_through_sequence = ?1 WHERE singleton_key = 1",
            [i64::try_from(pruned_through_sequence).expect("prune boundary fits")],
        )
        .expect("advance explicit retention boundary");
        drop(conn);

        let limits = PageLimits::new(100, 512 * 1024).expect("limits");
        let reported_newest = match store
            .begin_event_replay(pruned_through_sequence - 1, limits)
            .expect_err("expired replay must require a fresh snapshot")
        {
            ReplayError::ReplayUnavailable {
                oldest_sequence,
                newest_sequence: reported_newest,
            } => {
                assert_eq!(oldest_sequence, pruned_through_sequence + 1);
                reported_newest
            }
            other => panic!("expected replay unavailable, got {other:?}"),
        };
        assert_eq!(reported_newest, newest_sequence);

        let snapshot = store
            .begin_snapshot(limits)
            .expect("begin required replacement snapshot");
        let first_page = snapshot
            .page(SnapshotSection::Tasks, None)
            .expect("load replacement snapshot");
        assert_eq!(
            first_page.through_sequence, reported_newest,
            "replacement snapshot must start from the replay error's durable high-water"
        );
        assert_eq!(first_page.items.len(), 3);
    }

    #[test]
    fn retained_sequence_gaps_do_not_expire_replay() {
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("kernel.sqlite3");
        let mut store = KernelStore::open(&path).expect("open");
        create_task(&mut store, 0x49);
        create_task(&mut store, 0x4A);
        let newest_sequence = max_sequence(&path);

        let conn = Connection::open(&path).expect("open gap fixture");
        conn.execute("DELETE FROM events WHERE sequence = 1", [])
            .expect("create leading retained gap");
        drop(conn);

        let replay = store
            .begin_event_replay(0, PageLimits::new(100, 512 * 1024).expect("limits"))
            .expect("metadata, not the first stored row, defines expiration");
        let page = replay.page(None).expect("replay across retained gap");
        assert_eq!(page.after_sequence, 0);
        assert_eq!(page.through_sequence, newest_sequence);
        assert!(!page.events.is_empty());
        assert!(page.events.iter().all(|event| event.sequence > 1));
    }

    #[test]
    fn fully_pruned_history_preserves_high_water_for_snapshot_and_replay() {
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("kernel.sqlite3");
        let mut store = KernelStore::open(&path).expect("open");
        create_task(&mut store, 0x48);
        let newest_sequence = max_sequence(&path);

        let conn = Connection::open(&path).expect("open retention fixture");
        conn.execute(
            "UPDATE event_retention SET pruned_through_sequence = ?1 WHERE singleton_key = 1",
            [i64::try_from(newest_sequence).expect("high-water fits")],
        )
        .expect("advance explicit retention boundary");
        conn.execute("DELETE FROM events", [])
            .expect("simulate later bounded pruning");
        drop(conn);

        let limits = PageLimits::new(100, 512 * 1024).expect("limits");
        let snapshot = store.begin_snapshot(limits).expect("begin snapshot");
        let snapshot_page = snapshot
            .page(SnapshotSection::Tasks, None)
            .expect("load frozen task snapshot");
        assert_eq!(snapshot_page.through_sequence, newest_sequence);
        assert_eq!(snapshot_page.items.len(), 1);

        let replay = store
            .begin_event_replay(newest_sequence, limits)
            .expect("resume at fully pruned high-water");
        let replay_page = replay.page(None).expect("empty retained suffix");
        assert_eq!(replay_page.through_sequence, newest_sequence);
        assert!(replay_page.events.is_empty());
        assert!(replay_page.next_cursor.is_none());
    }

    #[test]
    fn missing_retention_singleton_fails_closed() {
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("kernel.sqlite3");
        let store = KernelStore::open(&path).expect("open");

        let conn = Connection::open(&path).expect("open retention fixture");
        conn.execute("DELETE FROM event_retention WHERE singleton_key = 1", [])
            .expect("remove retention singleton");
        drop(conn);

        assert!(matches!(
            store.begin_event_replay(0, PageLimits::new(10, 512 * 1024).expect("limits")),
            Err(ReplayError::Store(StoreError::Corruption))
        ));
        assert!(matches!(
            store.begin_snapshot(PageLimits::new(10, 512 * 1024).expect("limits")),
            Err(crate::kernel::snapshot::SnapshotError::Store(
                StoreError::Corruption
            ))
        ));
    }

    #[test]
    fn invalid_retention_metadata_is_corruption_for_replay_and_snapshot() {
        for tamper_sql in [
            "UPDATE event_retention SET pruned_through_sequence = -1 WHERE singleton_key = 1",
            "UPDATE event_retention SET pruned_through_sequence = 'not-an-integer' WHERE singleton_key = 1",
            "INSERT INTO event_retention(singleton_key, pruned_through_sequence) VALUES (2, 0)",
        ] {
            let dir = TempDir::new().expect("tempdir");
            let path = dir.path().join("kernel.sqlite3");
            let store = KernelStore::open(&path).expect("open");

            let conn = Connection::open(&path).expect("open retention fixture");
            conn.execute_batch("PRAGMA ignore_check_constraints = ON;")
                .expect("allow malformed metadata fixture");
            conn.execute_batch(tamper_sql)
                .expect("tamper retention metadata");
            drop(conn);

            assert!(
                matches!(
                    store.begin_event_replay(
                        0,
                        PageLimits::new(10, 512 * 1024).expect("limits")
                    ),
                    Err(ReplayError::Store(StoreError::Corruption))
                ),
                "replay must classify invalid metadata as corruption: {tamper_sql}"
            );
            assert!(
                matches!(
                    store.begin_snapshot(PageLimits::new(10, 512 * 1024).expect("limits")),
                    Err(crate::kernel::snapshot::SnapshotError::Store(
                        StoreError::Corruption
                    ))
                ),
                "snapshot must classify invalid metadata as corruption: {tamper_sql}"
            );
        }
    }

    #[test]
    fn event_replay_rejects_corrupt_durable_event_columns() {
        for tamper_sql in [
            "UPDATE events SET event_type = 'unknown.event' WHERE sequence = 1",
            "UPDATE events SET schema_version = 99 WHERE sequence = 1",
            "UPDATE events SET payload = X'00' WHERE sequence = 1",
            "UPDATE events SET task_revision = -1 WHERE sequence = 1",
        ] {
            let dir = TempDir::new().expect("tempdir");
            let path = dir.path().join("kernel.sqlite3");
            let mut store = KernelStore::open(&path).expect("open");
            create_task(&mut store, 0x50);

            let conn = Connection::open(&path).expect("open tamper connection");
            conn.execute_batch(tamper_sql)
                .expect("tamper durable event");
            drop(conn);

            let replay = store
                .begin_event_replay(0, PageLimits::new(10, 512 * 1024).expect("limits"))
                .expect("begin replay");
            assert!(
                matches!(replay.page(None), Err(ReplayError::Store(_))),
                "tamper must fail closed: {tamper_sql}"
            );
        }
    }
}
