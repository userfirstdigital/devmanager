//! Removing a deleted task from the store, entirely.
//!
//! # Why this exists
//!
//! Nothing in this product had ever pruned anything. `event_retention`
//! shipped in V3 and its `pruned_through_sequence` was still 0 on a
//! ten-month-old dev profile, with tests as its only writers. Deleting a task
//! set `tasks.lifecycle = 'deleted'` and stopped there, so on that profile 48
//! deleted tasks still owned 1,539 of 2,427 operations and 4,775 of 7,575
//! events -- rows nothing would ever read again, paged and integrity-scanned
//! on every client startup.
//!
//! The user's ruling was that deleting a task removes it, including the
//! `tasks` row. There is no tombstone.
//!
//! # Why the purge is a sweep and not part of applying `task.deleted`
//!
//! [`crate::domain::event::Event::TaskDeleted`] keeps its projector arm, which
//! still requires Archived first and still writes lifecycle Deleted. A
//! projection rebuild from a log that still holds the task's events therefore
//! reproduces that row, and the sweep finishes it on its next tick.
//!
//! That is what makes this crash-safe without a ledger: `lifecycle = 'deleted'`
//! IS the durable "owes a purge" marker, it is written by the projector only
//! after the whole task-close chain has settled, and the purge is the single
//! transaction that clears it. A crash anywhere before the commit converges on
//! the next tick; a crash after it has nothing left to converge. It also gives
//! the one-time cleanup of tasks deleted before this existed for free.
//!
//! # Why events are redacted rather than deleted
//!
//! `events.sequence` is the client's replay cursor. Deleting rows would punch
//! holes that a resuming client cannot distinguish from retention loss, and
//! `event_replay` would have to answer `pruned_through_sequence` for a store
//! that has pruned nothing. So the rows stay at their sequences and lose
//! everything that named the task: payload, `task_id`, `task_revision`, and
//! the V17 `operation_id`/`command_id`. A client replaying across the range
//! receives [`Event::Purged`] no-ops and stays contiguous.
//!
//! Clearing the V17 columns is safe for the self-arming backfill precisely
//! because `event_type` is rewritten in the same statement:
//! [`crate::kernel::command_bus::operation_index_has_gap`] only looks at rows
//! whose `event_type` is one of
//! [`crate::kernel::command_bus::OPERATION_IDENTITY_EVENT_TYPES`], and
//! `event.purged` is not one of them. A purged row is invisible to both the
//! gap probe and the backfill's selection, so the backfill cannot re-arm on
//! rows it can never fill.

use rusqlite::{Connection, OptionalExtension, Transaction};

use crate::domain::event::{Event, EVENT_SCHEMA_VERSION};
use crate::domain::id::TaskId;
use crate::domain::task::TaskLifecycle;
use crate::kernel::store::{encode_event_payload, StoreError};

/// One table the purge empties, and the predicate that reaches this task's
/// rows in it.
///
/// **This list is the one place that knows which tables a task's rows live
/// in.** `purge_table_census_covers_every_task_scoped_table` reads
/// `PRAGMA table_info` for every table in the live schema and fails if one
/// grows a `task_id` column without appearing here, so a new task-scoped
/// table cannot be added and silently left behind by the purge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PurgeTable {
    /// The table emptied of this task's rows.
    pub table: &'static str,
    /// The `WHERE` clause. `?1` is the 16-byte task id and may appear more
    /// than once.
    pub predicate: &'static str,
}

/// Every table the purge deletes from, in delete order.
///
/// **The order is children before parents**, and two of the foreign keys it
/// crosses are `DEFERRABLE INITIALLY DEFERRED`
/// (`operations.command_id -> command_receipts`, `outbox.operation_id ->
/// operations`), which is what lets `command_receipts` and `outbox` -- both
/// reached THROUGH `operations` -- be emptied before `operations` itself. The
/// subqueries below would find nothing if `operations` went first.
///
/// `events` and `tasks` are deliberately absent: `events` is redacted rather
/// than deleted (see the module docs) and `tasks` is deleted last, after this
/// whole list, so nothing can be orphaned by a partial run.
pub(crate) const PURGE_TABLES: &[PurgeTable] = &[
    // The provider journal's facts hang off its sessions by digest, and only
    // the session row carries the task.
    PurgeTable {
        table: "semantic_journal_facts",
        predicate: "authority_digest IN (
             SELECT authority_digest FROM semantic_journal_sessions WHERE task_id = ?1
         )",
    },
    PurgeTable {
        table: "semantic_journal_sessions",
        predicate: "task_id = ?1",
    },
    PurgeTable {
        table: "task_terminal_strip",
        predicate: "task_id = ?1",
    },
    // Before `resources`: `terminal_facts.resource_id` references it.
    PurgeTable {
        table: "terminal_facts",
        predicate: "task_id = ?1",
    },
    PurgeTable {
        table: "provider_input_state",
        predicate: "task_id = ?1",
    },
    PurgeTable {
        table: "artifacts",
        predicate: "task_id = ?1",
    },
    PurgeTable {
        table: "agent_sessions",
        predicate: "task_id = ?1",
    },
    PurgeTable {
        table: "resources",
        predicate: "task_id = ?1",
    },
    // Reached through `operations`, so both must precede it.
    PurgeTable {
        table: "outbox",
        predicate: "operation_id IN (SELECT operation_id FROM operations WHERE task_id = ?1)",
    },
    PurgeTable {
        table: "host_cleanup_branches",
        predicate: "operation_id IN (SELECT operation_id FROM operations WHERE task_id = ?1)",
    },
    // BOTH reaches, because they are not the same set. A receipt carries the
    // task it was issued for, and an operation carries the command that
    // accepted it; a receipt whose own `task_id` is NULL is still this task's
    // if one of its operations names it.
    PurgeTable {
        table: "command_receipts",
        predicate: "task_id = ?1
             OR command_id IN (SELECT command_id FROM operations WHERE task_id = ?1)",
    },
    PurgeTable {
        table: "operations",
        predicate: "task_id = ?1",
    },
    // Prompt history and its two search sidecars. Same three statements
    // `crate::prompts::history::delete_history_batch` runs for a retention
    // eviction, in the same order: dropping the history row without the FTS
    // row leaves a search hit that resolves to nothing.
    PurgeTable {
        table: "prompt_search",
        predicate: "source_kind = 'history'
             AND source_id IN (SELECT prompt_history_id FROM prompt_history WHERE task_id = ?1)",
    },
    PurgeTable {
        table: "prompt_search_pending",
        predicate: "source_kind = 'history'
             AND source_id IN (SELECT prompt_history_id FROM prompt_history WHERE task_id = ?1)",
    },
    PurgeTable {
        table: "prompt_history",
        predicate: "task_id = ?1",
    },
];

/// Tables that carry a `task_id` column and are deliberately NOT in
/// [`PURGE_TABLES`], with the reason. The census test reads this beside the
/// purge list, so exempting a table is a decision someone had to write down.
///
/// Test-only because the census is its only consumer: it exists to make an
/// exemption a written decision rather than a silent omission, not to be read
/// at runtime.
#[cfg(test)]
pub(crate) const PURGE_EXEMPT_TASK_SCOPED_TABLES: &[(&str, &str)] = &[
    (
        "events",
        "redacted in place -- the sequences are the client's replay cursor",
    ),
    ("tasks", "deleted last, after every table above"),
];

/// What one purge removed, per table, plus the events it redacted.
///
/// Per table rather than a total, because a total cannot say WHICH table a
/// zero came from -- and the interesting zero is a table the purge failed to
/// reach, not a table the task never used.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskPurgeReport {
    pub task_id: TaskId,
    /// `(table, rows deleted)`, in [`PURGE_TABLES`] order.
    pub rows_deleted: Vec<(&'static str, u64)>,
    /// `events` rows rewritten to `event.purged`.
    pub events_redacted: u64,
}

impl TaskPurgeReport {
    pub fn total_rows_deleted(&self) -> u64 {
        self.rows_deleted
            .iter()
            .map(|(_, rows)| *rows)
            .fold(0_u64, u64::saturating_add)
    }

    /// One line for a host log: only the tables that actually had rows.
    pub fn summary(&self) -> String {
        let mut parts = Vec::new();
        for (table, rows) in &self.rows_deleted {
            if *rows > 0 {
                parts.push(format!("{table}={rows}"));
            }
        }
        if self.events_redacted > 0 {
            parts.push(format!("events_redacted={}", self.events_redacted));
        }
        if parts.is_empty() {
            "no rows".to_string()
        } else {
            parts.join(" ")
        }
    }
}

/// Why a purge did nothing.
///
/// A named reason rather than `Ok(None)`: the sweep logs `NotDeleted` loudly
/// because it means something asked to purge a live task, and treats
/// `NotFound` as the ordinary already-done answer. A bare `None` would make
/// those one fact.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskPurgeRefusal {
    /// The task exists and is not `deleted`. **The only guard that licenses a
    /// purge.** `lifecycle = 'deleted'` is written by the projector only after
    /// the whole task-close/archive chain has settled, so a client close still
    /// in flight cannot be racing this: it has not reached Deleted yet.
    NotDeleted { lifecycle: TaskLifecycle },
    /// No `tasks` row. Already purged, or never existed.
    NotFound,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaskPurge {
    Purged(TaskPurgeReport),
    Refused(TaskPurgeRefusal),
}

/// Purge one deleted task inside an already-open writer transaction.
///
/// The caller owns the transaction so the whole purge -- every table, the
/// event redaction and the `tasks` row -- commits or does not, and a reader
/// never observes a half-purged task.
pub(crate) fn purge_deleted_task_in_tx(
    tx: &Transaction<'_>,
    task_id: TaskId,
) -> Result<TaskPurge, StoreError> {
    let lifecycle: Option<String> = tx
        .query_row(
            "SELECT lifecycle FROM tasks WHERE task_id = ?1",
            [task_id.as_bytes().as_slice()],
            |row| row.get(0),
        )
        .optional()?;
    let Some(lifecycle) = lifecycle else {
        return Ok(TaskPurge::Refused(TaskPurgeRefusal::NotFound));
    };
    let lifecycle = crate::kernel::command_bus::parse_lifecycle(&lifecycle)?;
    if lifecycle != TaskLifecycle::Deleted {
        return Ok(TaskPurge::Refused(TaskPurgeRefusal::NotDeleted {
            lifecycle,
        }));
    }

    let task_bytes = task_id.as_bytes();
    let mut rows_deleted = Vec::with_capacity(PURGE_TABLES.len());
    for entry in PURGE_TABLES {
        let sql = format!("DELETE FROM {} WHERE {}", entry.table, entry.predicate);
        let changed = tx.execute(&sql, [task_bytes.as_slice()])?;
        rows_deleted.push((entry.table, u64::try_from(changed).unwrap_or(0)));
    }

    let events_redacted = redact_task_events(tx, task_id)?;

    let removed = tx.execute(
        "DELETE FROM tasks WHERE task_id = ?1",
        [task_bytes.as_slice()],
    )?;
    if removed != 1 {
        // The row was read under this same writer transaction, so exactly one
        // row must go. Anything else means the table changed underneath a
        // transaction that holds the write lock.
        return Err(StoreError::Corruption);
    }

    Ok(TaskPurge::Purged(TaskPurgeReport {
        task_id,
        rows_deleted,
        events_redacted,
    }))
}

/// Rewrite this task's `events` rows to `event.purged` in place.
///
/// Sequences, `event_id`s and `occurred_at_ms` are untouched: they are what
/// keep a replay contiguous. Everything that named the task goes, including
/// the V17 identity columns -- an operation whose `operations` row this purge
/// just deleted must not stay reachable by an indexed lookup.
fn redact_task_events(tx: &Transaction<'_>, task_id: TaskId) -> Result<u64, StoreError> {
    let payload = encode_event_payload(&Event::Purged)?;
    let changed = tx.execute(
        "UPDATE events
         SET event_type = ?1,
             schema_version = ?2,
             payload = ?3,
             task_id = NULL,
             task_revision = NULL,
             operation_id = NULL,
             command_id = NULL
         WHERE task_id = ?4",
        rusqlite::params![
            Event::Purged.event_type(),
            i64::from(EVENT_SCHEMA_VERSION),
            payload,
            task_id.as_bytes().as_slice(),
        ],
    )?;
    Ok(u64::try_from(changed).unwrap_or(0))
}

/// The deleted tasks awaiting a purge, oldest first, at most `limit`.
///
/// Oldest first so a backlog drains in a stable order rather than starving one
/// task behind another.
pub(crate) fn deleted_task_ids(conn: &Connection, limit: u32) -> Result<Vec<TaskId>, StoreError> {
    let mut stmt = conn.prepare(
        "SELECT task_id FROM tasks
         WHERE lifecycle = 'deleted'
         ORDER BY updated_at_ms ASC, task_id ASC
         LIMIT ?1",
    )?;
    let rows = stmt.query_map([i64::from(limit)], |row| row.get::<_, Vec<u8>>(0))?;
    let mut task_ids = Vec::new();
    for row in rows {
        task_ids.push(crate::kernel::store::task_id_from_bytes(&row?)?);
    }
    Ok(task_ids)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::agent::{AgentRole, AgentSessionFacts, AgentSessionLifecycle};
    use crate::domain::artifact::{ArtifactContentRef, ArtifactFacts, ArtifactKind, PrivacyClass};
    use crate::domain::command::{Command, CommandEnvelope, CommandReceipt, CreateTaskIntent};
    use crate::domain::id::{
        AgentSessionId, ArtifactId, ClientId, CommandId, EnvironmentId, ProjectId, ResourceId,
    };
    use crate::domain::task::{
        ReviewReadiness, TaskActivity, TaskAssignment, TaskAttention, TaskConnectivity,
        WorkspaceRef,
    };
    use crate::kernel::store::KernelStore;
    use crate::providers::ProviderKind;
    use std::time::Duration;
    use tempfile::TempDir;

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
    test_id!(agent_session_id, AgentSessionId);
    test_id!(resource_id, ResourceId);

    fn envelope(
        command: CommandId,
        task: Option<TaskId>,
        expected_task_revision: Option<u64>,
        payload: Command,
    ) -> CommandEnvelope {
        CommandEnvelope {
            command_id: command,
            client_id: client_id(0x01),
            task_id: task,
            issued_at_ms: 1_725_000_000_100,
            expected_task_revision,
            command: payload,
        }
    }

    fn accepted(receipt: CommandReceipt, what: &str) {
        assert!(
            matches!(receipt, CommandReceipt::Accepted { .. }),
            "{what} must be accepted, got {receipt:?}"
        );
    }

    /// Create a task, give it an agent session and an artifact, then close,
    /// archive and delete it through the same command path the host uses.
    ///
    /// Every command also writes a `command_receipts` row and an `operations`
    /// row, and `BeginCloseTask` writes an `outbox` row, so the tables reached
    /// THROUGH `operations` are populated by real lineage rather than by hand.
    fn seed_deleted_task(store: &mut KernelStore) -> TaskId {
        let task = task_id(0x40);
        accepted(
            store
                .execute_for_test(envelope(
                    command_id(0x41),
                    None,
                    None,
                    Command::CreateTask(CreateTaskIntent {
                        id: task,
                        environment_id: environment_id(0x02),
                        title: "purge fixture".into(),
                        description: Some("everything this task owns must go".into()),
                        project_id: project_id(0x03),
                        workspace: WorkspaceRef::Main,
                        assignment: TaskAssignment::LocalOwner,
                        created_at_ms: 1_725_000_000_000,
                        connectivity: TaskConnectivity::Connected,
                        attention: TaskAttention::None,
                        activity: TaskActivity::Idle,
                        review_readiness: ReviewReadiness::NotReady,
                    }),
                ))
                .expect("create task"),
            "create task",
        );
        accepted(
            store
                .execute(envelope(
                    command_id(0x42),
                    Some(task),
                    Some(1),
                    Command::RegisterAgentSession {
                        agent: AgentSessionFacts {
                            id: agent_session_id(0x50),
                            task_id: task,
                            role: AgentRole::Primary,
                            provider_kind: ProviderKind::ClaudeCode,
                            provider_session_id: None,
                            lifecycle: AgentSessionLifecycle::Open,
                            runtime_generation: 0,
                            revision: 0,
                        },
                    },
                ))
                .expect("register agent session"),
            "register agent session",
        );
        accepted(
            store
                .execute(envelope(
                    command_id(0x43),
                    Some(task),
                    Some(2),
                    Command::RegisterArtifact {
                        artifact: ArtifactFacts {
                            id: artifact_id(0x60),
                            task_id: task,
                            kind: ArtifactKind::Evidence,
                            label: "purge fixture artifact".into(),
                            content_ref: ArtifactContentRef::inline_utf8("evidence".to_string())
                                .expect("inline artifact"),
                            sha256: [0xA5; 32],
                            privacy_class: PrivacyClass::LocalOnly,
                            created_at_ms: 1_725_000_000_200,
                        },
                    },
                ))
                .expect("register artifact"),
            "register artifact",
        );
        accepted(
            store
                .execute(envelope(
                    command_id(0x44),
                    Some(task),
                    Some(3),
                    Command::BeginCloseTask,
                ))
                .expect("begin close"),
            "begin close",
        );
        store
            .settle_next_process_empty_task_teardown(Duration::from_secs(30))
            .expect("settle process-empty teardown");
        accepted(
            store
                .execute(envelope(
                    command_id(0x45),
                    Some(task),
                    Some(5),
                    Command::DeleteTask,
                ))
                .expect("delete task"),
            "delete task",
        );
        task
    }

    /// Fill the task-scoped tables the fixture's command path does not reach.
    ///
    /// Hand-written rows, deliberately: the point of these tests is that the
    /// purge REACHES every table in [`PURGE_TABLES`], and a table the fixture
    /// happened to leave empty would let a missing predicate pass. Each row is
    /// the minimum the table's own CHECK constraints accept.
    fn seed_remaining_task_tables(path: &std::path::Path, task: TaskId) {
        let conn = Connection::open(path).expect("open seeding connection");
        conn.execute_batch("PRAGMA foreign_keys = ON;")
            .expect("enable foreign keys");
        let task_bytes = task.as_bytes().to_vec();
        let resource = resource_id(0x70).as_bytes().to_vec();
        let agent = agent_session_id(0x50).as_bytes().to_vec();
        let recipe = rmp_serde::to_vec(&crate::domain::resource::ResourceRecipe::terminal(80, 24))
            .expect("encode terminal recipe");
        conn.execute(
            "INSERT INTO resources(
                 resource_id, task_id, owner_kind, resource_kind, recipe,
                 lifecycle, runtime_generation, updated_at_ms
             ) VALUES (?1, ?2, 'task', 'terminal', ?3, 'released', 0, 1)",
            rusqlite::params![resource, task_bytes, recipe],
        )
        .expect("seed resources");
        conn.execute(
            "INSERT INTO terminal_facts(
                 resource_id, task_id, title, live_cwd, exit_code, exit_summary,
                 exited_at_ms, created_at_ms, last_activity_at_ms
             ) VALUES (?1, ?2, 'purge fixture terminal', NULL, NULL, NULL, NULL, 1, 1)",
            rusqlite::params![resource, task_bytes],
        )
        .expect("seed terminal_facts");
        conn.execute(
            "INSERT INTO task_terminal_strip(task_id, order_msgpack, focused_resource_id)
             VALUES (?1, ?2, ?3)",
            rusqlite::params![
                task_bytes,
                rmp_serde::to_vec(&vec![resource_id(0x70)]).expect("encode strip"),
                resource
            ],
        )
        .expect("seed task_terminal_strip");
        // provider_input_state is NOT seeded here: the projector already
        // writes one row per registered agent session, and the fixture
        // registers one. The pre-purge "> 0 rows" assertion below is what
        // keeps that true -- if the projector stops writing it, the test goes
        // red rather than passing vacuously.
        conn.execute(
            "INSERT INTO semantic_journal_sessions(
                 authority_digest, provider_kind, task_id, agent_session_id, resource_id,
                 runtime_generation, action_epoch, managed_root, opened_at_ms
             ) VALUES (?1, 'claude', ?2, ?3, ?4, 0, 0, ?5, 1)",
            rusqlite::params![
                vec![0x11_u8; 32],
                task_bytes,
                agent,
                resource,
                vec![0x22_u8; 32]
            ],
        )
        .expect("seed semantic_journal_sessions");
        conn.execute(
            "INSERT INTO semantic_journal_facts(
                 authority_digest, sequence, event_id, delivery_id, provider_event_id,
                 content_hash, kind, visibility, privacy_class, redaction_class,
                 occurred_at_ms, ingested_at_ms, schema_version, payload
             ) VALUES (?1, 1, ?2, 'delivery-1', 'provider-1', ?3,
                       'message', 'visible', 'local_only', 'none', 1, 1, 1, X'90')",
            rusqlite::params![vec![0x11_u8; 32], fixed_uuid_v7(0x80), vec![0x33_u8; 32]],
        )
        .expect("seed semantic_journal_facts");
        let history = fixed_uuid_v7(0x90).to_vec();
        conn.execute(
            "INSERT INTO prompt_history(
                 prompt_history_id, request_id, submitted_event_id, task_id, agent_session_id,
                 provider_kind, body, body_sha256, submitted_at_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, 'claude', 'purge fixture prompt', ?6, 1)",
            rusqlite::params![
                history,
                fixed_uuid_v7(0x91),
                fixed_uuid_v7(0x92),
                task_bytes,
                agent,
                vec![0x44_u8; 32]
            ],
        )
        .expect("seed prompt_history");
        conn.execute(
            "INSERT INTO prompt_search(source_kind, source_id, title, body, tags)
             VALUES ('history', ?1, '', 'purge fixture prompt', '')",
            rusqlite::params![history],
        )
        .expect("seed prompt_search");
        conn.execute(
            "INSERT INTO prompt_search_pending(source_kind, source_id, enqueue_seq)
             VALUES ('history', ?1, 1)",
            rusqlite::params![history],
        )
        .expect("seed prompt_search_pending");
        let operation: Vec<u8> = conn
            .query_row(
                "SELECT operation_id FROM operations WHERE task_id = ?1 LIMIT 1",
                [task_bytes.as_slice()],
                |row| row.get(0),
            )
            .expect("fixture task owns at least one operation");
        conn.execute(
            "INSERT INTO host_cleanup_branches(
                 operation_id, branch, result, remaining_count, completed_at_ms
             ) VALUES (?1, 'agent_sessions', 'succeeded', 0, 1)",
            rusqlite::params![operation],
        )
        .expect("seed host_cleanup_branches");
    }

    fn rows_for_task(conn: &Connection, entry: &PurgeTable, task: TaskId) -> i64 {
        conn.query_row(
            &format!(
                "SELECT COUNT(*) FROM {} WHERE {}",
                entry.table, entry.predicate
            ),
            [task.as_bytes().as_slice()],
            |row| row.get(0),
        )
        .unwrap_or_else(|error| panic!("count {}: {error}", entry.table))
    }

    #[test]
    fn purge_empties_every_task_scoped_table_and_redacts_the_events() {
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("kernel.sqlite3");
        let mut store = KernelStore::open(&path).expect("open");
        let task = seed_deleted_task(&mut store);
        seed_remaining_task_tables(&path, task);

        // Every listed table must actually hold rows for this task before the
        // purge, or its assertion below would pass without proving anything.
        let observer = Connection::open(&path).expect("observer");
        for entry in PURGE_TABLES {
            assert!(
                rows_for_task(&observer, entry, task) > 0,
                "fixture must seed {} or the purge assertion is vacuous",
                entry.table
            );
        }
        let events_before: i64 = observer
            .query_row(
                "SELECT COUNT(*) FROM events WHERE task_id = ?1",
                [task.as_bytes().as_slice()],
                |row| row.get(0),
            )
            .expect("count events");
        assert!(events_before > 0, "fixture must write events");
        let total_events_before: i64 = observer
            .query_row("SELECT COUNT(*) FROM events", [], |row| row.get(0))
            .expect("count all events");
        let max_sequence_before: i64 = observer
            .query_row("SELECT MAX(sequence) FROM events", [], |row| row.get(0))
            .expect("max sequence");
        drop(observer);

        let report = match store
            .with_immediate_transaction(|tx| purge_deleted_task_in_tx(tx, task))
            .expect("purge")
        {
            TaskPurge::Purged(report) => report,
            other => panic!("expected a purge, got {other:?}"),
        };
        assert_eq!(report.task_id, task);
        assert_eq!(
            report.events_redacted,
            u64::try_from(events_before).unwrap()
        );

        let observer = Connection::open(&path).expect("observer");
        for entry in PURGE_TABLES {
            assert_eq!(
                rows_for_task(&observer, entry, task),
                0,
                "{} still holds rows for the purged task",
                entry.table
            );
        }
        let remaining_tasks: i64 = observer
            .query_row(
                "SELECT COUNT(*) FROM tasks WHERE task_id = ?1",
                [task.as_bytes().as_slice()],
                |row| row.get(0),
            )
            .expect("count tasks");
        assert_eq!(remaining_tasks, 0, "the tasks row must be gone");

        // Redacted, not deleted: the row count and the high-water sequence are
        // unchanged, and every one of this task's rows now carries the purged
        // type with NULL identity columns.
        let total_events_after: i64 = observer
            .query_row("SELECT COUNT(*) FROM events", [], |row| row.get(0))
            .expect("count all events");
        assert_eq!(total_events_after, total_events_before);
        let max_sequence_after: i64 = observer
            .query_row("SELECT MAX(sequence) FROM events", [], |row| row.get(0))
            .expect("max sequence");
        assert_eq!(max_sequence_after, max_sequence_before);
        let purged: i64 = observer
            .query_row(
                "SELECT COUNT(*) FROM events WHERE event_type = 'event.purged'",
                [],
                |row| row.get(0),
            )
            .expect("count purged");
        assert_eq!(purged, events_before);
        let leaked: i64 = observer
            .query_row(
                "SELECT COUNT(*) FROM events
                 WHERE event_type = 'event.purged'
                   AND (task_id IS NOT NULL OR task_revision IS NOT NULL
                        OR operation_id IS NOT NULL OR command_id IS NOT NULL)",
                [],
                |row| row.get(0),
            )
            .expect("count leaked identity");
        assert_eq!(leaked, 0, "a purged row must name nothing");

        // A purged row still decodes, so a replay across the range yields
        // no-ops rather than poisoning the session.
        let (event_type, schema_version, payload): (String, i64, Vec<u8>) = observer
            .query_row(
                "SELECT event_type, schema_version, payload FROM events
                 WHERE event_type = 'event.purged' LIMIT 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .expect("read a purged row");
        assert_eq!(
            crate::kernel::store::decode_stored_event(&event_type, schema_version, &payload)
                .expect("purged rows decode"),
            Event::Purged
        );
    }

    #[test]
    fn purge_refuses_a_task_that_is_not_deleted() {
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("kernel.sqlite3");
        let mut store = KernelStore::open(&path).expect("open");
        let task = task_id(0x40);
        accepted(
            store
                .execute_for_test(envelope(
                    command_id(0x41),
                    None,
                    None,
                    Command::CreateTask(CreateTaskIntent {
                        id: task,
                        environment_id: environment_id(0x02),
                        title: "still open".into(),
                        description: None,
                        project_id: project_id(0x03),
                        workspace: WorkspaceRef::Main,
                        assignment: TaskAssignment::LocalOwner,
                        created_at_ms: 1_725_000_000_000,
                        connectivity: TaskConnectivity::Connected,
                        attention: TaskAttention::None,
                        activity: TaskActivity::Idle,
                        review_readiness: ReviewReadiness::NotReady,
                    }),
                ))
                .expect("create task"),
            "create task",
        );

        assert_eq!(
            store
                .with_immediate_transaction(|tx| purge_deleted_task_in_tx(tx, task))
                .expect("purge call"),
            TaskPurge::Refused(TaskPurgeRefusal::NotDeleted {
                lifecycle: TaskLifecycle::Open
            })
        );
        let observer = Connection::open(&path).expect("observer");
        let remaining: i64 = observer
            .query_row(
                "SELECT COUNT(*) FROM tasks WHERE task_id = ?1",
                [task.as_bytes().as_slice()],
                |row| row.get(0),
            )
            .expect("count tasks");
        assert_eq!(remaining, 1, "a refused purge must change nothing");
    }

    #[test]
    fn purge_of_an_absent_task_is_not_found() {
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("kernel.sqlite3");
        let mut store = KernelStore::open(&path).expect("open");
        assert_eq!(
            store
                .with_immediate_transaction(|tx| purge_deleted_task_in_tx(tx, task_id(0xEE)))
                .expect("purge call"),
            TaskPurge::Refused(TaskPurgeRefusal::NotFound)
        );
    }

    #[test]
    fn purge_is_idempotent_and_the_worklist_drains() {
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("kernel.sqlite3");
        let mut store = KernelStore::open(&path).expect("open");
        let task = seed_deleted_task(&mut store);

        let observer = Connection::open(&path).expect("observer");
        assert_eq!(
            deleted_task_ids(&observer, 8).expect("worklist"),
            vec![task]
        );
        drop(observer);

        assert!(matches!(
            store
                .with_immediate_transaction(|tx| purge_deleted_task_in_tx(tx, task))
                .expect("first purge"),
            TaskPurge::Purged(_)
        ));
        assert_eq!(
            store
                .with_immediate_transaction(|tx| purge_deleted_task_in_tx(tx, task))
                .expect("second purge"),
            TaskPurge::Refused(TaskPurgeRefusal::NotFound)
        );
        let observer = Connection::open(&path).expect("observer");
        assert!(deleted_task_ids(&observer, 8).expect("worklist").is_empty());
    }

    /// The V17 self-arming backfill must not re-arm forever on purged rows.
    ///
    /// It would if redaction cleared `operation_id` while leaving
    /// `event_type = 'operation.settled'`: the gap probe would keep reporting
    /// a gap, the backfill would select the row, fail to decode an operation
    /// out of an empty payload, and every open would pay a scan it can never
    /// finish. Rewriting the type in the same statement is what prevents it.
    #[test]
    fn purged_rows_do_not_re_arm_the_v17_identity_backfill() {
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("kernel.sqlite3");
        let mut store = KernelStore::open(&path).expect("open");
        let task = seed_deleted_task(&mut store);
        let operation_events: i64 = {
            let observer = Connection::open(&path).expect("observer");
            observer
                .query_row(
                    "SELECT COUNT(*) FROM events
                     WHERE task_id = ?1 AND event_type LIKE 'operation.%'",
                    [task.as_bytes().as_slice()],
                    |row| row.get(0),
                )
                .expect("count operation events")
        };
        assert!(
            operation_events > 0,
            "fixture must write operation facts or this proves nothing"
        );
        assert!(matches!(
            store
                .with_immediate_transaction(|tx| purge_deleted_task_in_tx(tx, task))
                .expect("purge"),
            TaskPurge::Purged(_)
        ));
        drop(store);

        let observer = Connection::open(&path).expect("observer");
        assert!(
            !crate::kernel::command_bus::operation_index_has_gap(
                &observer,
                crate::kernel::command_bus::OPERATION_IDENTITY_EVENT_TYPES,
            )
            .expect("gap probe"),
            "purged rows must be invisible to the V17 gap probe"
        );
        drop(observer);

        let reopened = KernelStore::open(&path).expect("reopen");
        assert_eq!(
            reopened.startup_identity_backfill(),
            crate::kernel::store::OperationIdentityBackfill::NotNeeded
        );
    }

    /// The sweep's worklist offers deleted tasks and nothing else.
    ///
    /// The selection IS the guard: `tasks.lifecycle = 'deleted'` is written by
    /// the projector only after the whole close/archive chain has settled, so
    /// an open task -- or one still closing -- is never a candidate, and
    /// `purge_deleted_task` re-checks the same column inside its own writer
    /// transaction.
    #[test]
    fn the_worklist_offers_deleted_tasks_and_nothing_else() {
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("kernel.sqlite3");
        let mut store = KernelStore::open(&path).expect("open");
        let deleted = seed_deleted_task(&mut store);

        let open = task_id(0xA0);
        accepted(
            store
                .execute_for_test(envelope(
                    command_id(0xA1),
                    None,
                    None,
                    Command::CreateTask(CreateTaskIntent {
                        id: open,
                        environment_id: environment_id(0x02),
                        title: "still open".into(),
                        description: None,
                        project_id: project_id(0x03),
                        workspace: WorkspaceRef::Main,
                        assignment: TaskAssignment::LocalOwner,
                        created_at_ms: 1_725_000_000_000,
                        connectivity: TaskConnectivity::Connected,
                        attention: TaskAttention::None,
                        activity: TaskActivity::Idle,
                        review_readiness: ReviewReadiness::NotReady,
                    }),
                ))
                .expect("create open task"),
            "create open task",
        );
        let closing = task_id(0xB0);
        accepted(
            store
                .execute_for_test(envelope(
                    command_id(0xB1),
                    None,
                    None,
                    Command::CreateTask(CreateTaskIntent {
                        id: closing,
                        environment_id: environment_id(0x02),
                        title: "closing".into(),
                        description: None,
                        project_id: project_id(0x03),
                        workspace: WorkspaceRef::Main,
                        assignment: TaskAssignment::LocalOwner,
                        created_at_ms: 1_725_000_000_000,
                        connectivity: TaskConnectivity::Connected,
                        attention: TaskAttention::None,
                        activity: TaskActivity::Idle,
                        review_readiness: ReviewReadiness::NotReady,
                    }),
                ))
                .expect("create closing task"),
            "create closing task",
        );
        accepted(
            store
                .execute(envelope(
                    command_id(0xB2),
                    Some(closing),
                    Some(1),
                    Command::BeginCloseTask,
                ))
                .expect("begin close"),
            "begin close",
        );
        store
            .settle_next_process_empty_task_teardown(Duration::from_secs(30))
            .expect("archive the closing task");

        let observer = Connection::open(&path).expect("observer");
        assert_eq!(
            deleted_task_ids(&observer, 8).expect("worklist"),
            vec![deleted],
            "only the deleted task may be swept"
        );

        // And the purge refuses each of the others by name, so the guard holds
        // even if something else ever hands it a task id.
        drop(observer);
        for (task, expected) in [
            (open, TaskLifecycle::Open),
            (closing, TaskLifecycle::Archived),
        ] {
            assert_eq!(
                store
                    .with_immediate_transaction(|tx| purge_deleted_task_in_tx(tx, task))
                    .expect("purge call"),
                TaskPurge::Refused(TaskPurgeRefusal::NotDeleted {
                    lifecycle: expected
                })
            );
        }
    }

    /// A client resuming from a cursor BEFORE the purged range must get a
    /// contiguous page of no-ops, not a history gap.
    ///
    /// This is the whole reason the rows are redacted rather than deleted, and
    /// it is checked end to end: the replay session pages every sequence, each
    /// purged row decodes to [`Event::Purged`] with no task, and the sequences
    /// are unbroken across the range.
    #[test]
    fn replay_across_a_purged_range_yields_contiguous_no_ops() {
        use crate::domain::snapshot::PageLimits;

        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("kernel.sqlite3");
        let mut store = KernelStore::open(&path).expect("open");
        let task = seed_deleted_task(&mut store);
        let highest = {
            let observer = Connection::open(&path).expect("observer");
            let highest: i64 = observer
                .query_row("SELECT MAX(sequence) FROM events", [], |row| row.get(0))
                .expect("max sequence");
            highest
        };
        assert!(matches!(
            store
                .with_immediate_transaction(|tx| purge_deleted_task_in_tx(tx, task))
                .expect("purge"),
            TaskPurge::Purged(_)
        ));

        let session = store
            .begin_event_replay(0, PageLimits::new(500, 512 * 1024).expect("limits"))
            .expect("replay from the very beginning");
        let mut sequences = Vec::new();
        let mut purged = 0_usize;
        let mut cursor = None;
        loop {
            let page = session
                .page(cursor.as_deref())
                .expect("a page across the purged range");
            for event in &page.events {
                sequences.push(event.sequence);
                if event.payload == Event::Purged {
                    purged += 1;
                    assert_eq!(event.task_id, None, "a purged event must not name a task");
                    assert_eq!(event.task_revision, None);
                }
            }
            match page.next_cursor {
                Some(next) => cursor = Some(next),
                None => break,
            }
        }
        assert!(purged > 0, "the fixture must have purged some events");
        assert_eq!(
            sequences,
            (1..=u64::try_from(highest).unwrap()).collect::<Vec<_>>(),
            "the replay must stay contiguous across the purged range"
        );

        // The retention boundary is untouched: nothing was pruned, so a
        // resuming client is never told it lost history.
        let observer = Connection::open(&path).expect("observer");
        let pruned_through: i64 = observer
            .query_row(
                "SELECT pruned_through_sequence FROM event_retention WHERE singleton_key = 1",
                [],
                |row| row.get(0),
            )
            .expect("retention boundary");
        assert_eq!(
            pruned_through, 0,
            "a purge must never move the prune boundary"
        );
    }

    /// A rebuild after a purge must reproduce exactly the tables the purge
    /// left, with no drift.
    ///
    /// This is the load-bearing consequence of redacting instead of deleting.
    /// A rebuild replays the whole log into shadow tables and diffs them
    /// against the live projections; if a purged row still decoded to
    /// `TaskDeleted` the rebuild would resurrect the `tasks` row and report
    /// drift, and the sweep and the rebuild would fight forever.
    #[test]
    fn projection_rebuild_after_a_purge_reproduces_the_same_tables() {
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("kernel.sqlite3");
        let mut store = KernelStore::open(&path).expect("open");
        let survivor = {
            let task = task_id(0x40);
            let purged = seed_deleted_task(&mut store);
            assert_eq!(purged, task);
            task
        };
        assert!(matches!(
            store
                .with_immediate_transaction(|tx| purge_deleted_task_in_tx(tx, survivor))
                .expect("purge"),
            TaskPurge::Purged(_)
        ));

        let rebuild = store.rebuild_projections().expect("rebuild after purge");
        assert!(
            !rebuild.drift_detected,
            "a rebuild after a purge must not drift: {rebuild:?}"
        );
        let observer = Connection::open(&path).expect("observer");
        let resurrected: i64 = observer
            .query_row("SELECT COUNT(*) FROM tasks", [], |row| row.get(0))
            .expect("count tasks");
        assert_eq!(
            resurrected, 0,
            "the rebuild must not resurrect the purged task"
        );
        for entry in PURGE_TABLES {
            assert_eq!(
                rows_for_task(&observer, entry, survivor),
                0,
                "{} came back after the rebuild",
                entry.table
            );
        }
    }

    /// A snapshot session pinned BEFORE a purge keeps answering cleanly.
    ///
    /// `begin_snapshot` holds a DEFERRED read transaction and takes its WAL
    /// snapshot on the first read, so a purge committed afterwards is
    /// invisible to it: `load_task_ids` and the per-task load both read the
    /// frozen view. That is why `snapshot.rs`'s "task disappeared from pinned
    /// snapshot" fail-closed guard stays a corruption guard rather than
    /// becoming a NotFound the purge can trigger.
    #[test]
    fn a_snapshot_pinned_before_a_purge_still_pages_the_task() {
        use crate::domain::snapshot::{PageLimits, SnapshotSection};

        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("kernel.sqlite3");
        let mut store = KernelStore::open(&path).expect("open");
        let task = seed_deleted_task(&mut store);

        let session = store
            .begin_snapshot(PageLimits::new(100, 512 * 1024).expect("limits"))
            .expect("pin snapshot");
        // Force the WAL snapshot before the purge commits.
        let first = session
            .page(SnapshotSection::Tasks, None)
            .expect("first page");
        assert!(
            first.items.iter().any(|item| matches!(
                item,
                crate::domain::snapshot::SnapshotItem::Task(task_item)
                    if task_item.task.id == task
            )),
            "the pinned snapshot must contain the task it was pinned over"
        );

        assert!(matches!(
            store
                .with_immediate_transaction(|tx| purge_deleted_task_in_tx(tx, task))
                .expect("purge"),
            TaskPurge::Purged(_)
        ));

        let after = session
            .page(SnapshotSection::Tasks, None)
            .expect("the pinned view must survive a purge committed underneath it");
        assert_eq!(after.items.len(), first.items.len());
    }

    /// **This is the test that keeps [`PURGE_TABLES`] honest.**
    ///
    /// It reads the live schema rather than a list someone maintained by hand,
    /// so a table that grows a `task_id` column and is not added to the purge
    /// (or written down as an exemption) fails here rather than silently
    /// leaking that table's rows forever.
    #[test]
    fn purge_covers_every_table_with_a_task_id_column() {
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("kernel.sqlite3");
        let store = KernelStore::open(&path).expect("open");
        drop(store);
        let conn = Connection::open(&path).expect("observer");

        let task_scoped = task_scoped_tables(&conn);
        assert!(
            task_scoped.contains(&"tasks".to_string()),
            "the census itself must find at least the tasks table; found {task_scoped:?}"
        );
        assert_eq!(
            uncovered_task_scoped_tables(&task_scoped),
            Vec::<String>::new(),
            "every table with a task_id column must be in PURGE_TABLES or in \
             PURGE_EXEMPT_TASK_SCOPED_TABLES with a reason"
        );
        for table in &task_scoped {
            let purged = PURGE_TABLES.iter().any(|entry| entry.table == table);
            let exempt = PURGE_EXEMPT_TASK_SCOPED_TABLES
                .iter()
                .any(|(name, _)| name == table);
            assert!(
                !(purged && exempt),
                "{table} is both purged and exempt; the two lists must not overlap"
            );
        }
        for (name, reason) in PURGE_EXEMPT_TASK_SCOPED_TABLES {
            assert!(
                task_scoped.contains(&(*name).to_string()),
                "{name} is listed as exempt but has no task_id column"
            );
            assert!(!reason.is_empty(), "{name} exemption needs a reason");
        }

        // Sabotage: add a task-scoped table nobody listed, and prove the SAME
        // check that just passed now names it. Without this the census could
        // be vacuous -- a coverage rule that cannot go red is not a rule.
        conn.execute_batch(
            "CREATE TABLE census_sabotage_notes (
                 note_id BLOB PRIMARY KEY,
                 task_id BLOB NOT NULL
             );",
        )
        .expect("create sabotage table");
        assert_eq!(
            uncovered_task_scoped_tables(&task_scoped_tables(&conn)),
            vec!["census_sabotage_notes".to_string()],
            "an unlisted task-scoped table must fail the census"
        );
    }

    /// The census rule itself, so the passing case and the sabotage case ask
    /// exactly the same question. Returns the tables that carry a `task_id`
    /// and appear in neither list.
    fn uncovered_task_scoped_tables(task_scoped: &[String]) -> Vec<String> {
        task_scoped
            .iter()
            .filter(|table| {
                !PURGE_TABLES.iter().any(|entry| entry.table == *table)
                    && !PURGE_EXEMPT_TASK_SCOPED_TABLES
                        .iter()
                        .any(|(name, _)| name == *table)
            })
            .cloned()
            .collect()
    }

    /// Every table in the live schema that carries a `task_id` column.
    fn task_scoped_tables(conn: &Connection) -> Vec<String> {
        let tables: Vec<String> = {
            let mut stmt = conn
                .prepare(
                    "SELECT name FROM sqlite_schema
                     WHERE type = 'table' AND name NOT LIKE 'sqlite_%'
                     ORDER BY name",
                )
                .expect("prepare table list");
            let rows = stmt
                .query_map([], |row| row.get::<_, String>(0))
                .expect("query table list");
            rows.map(|row| row.expect("table name")).collect()
        };
        let mut task_scoped = Vec::new();
        for table in tables {
            let mut stmt = conn
                .prepare(&format!("PRAGMA table_info('{table}')"))
                .expect("prepare table_info");
            let mut rows = stmt.query([]).expect("query table_info");
            while let Some(row) = rows.next().expect("table_info row") {
                let column: String = row.get(1).expect("column name");
                if column == "task_id" {
                    task_scoped.push(table.clone());
                    break;
                }
            }
        }
        task_scoped
    }

    /// Purge a copy of a REAL store and print the before/after counts.
    ///
    /// Ignored by default: it needs a store copy, which only a developer can
    /// supply. `DEVMANAGER_PURGE_PROBE_STORE` must point at a COPY -- this
    /// writes to it.
    ///
    /// ```text
    /// $env:DEVMANAGER_PURGE_PROBE_STORE = "<copy>/kernel.sqlite3"
    /// cargo test --lib kernel::purge::tests::probe -- --ignored --nocapture
    /// ```
    #[test]
    #[ignore]
    fn probe_purge_against_a_store_copy() {
        let path = std::env::var("DEVMANAGER_PURGE_PROBE_STORE")
            .expect("DEVMANAGER_PURGE_PROBE_STORE must point at a COPY of a kernel store");
        let path = std::path::PathBuf::from(path);
        let mut store = KernelStore::open(&path).expect("open store copy");

        let census = |conn: &Connection| -> Vec<(String, i64)> {
            let mut counts = Vec::new();
            for table in [
                "tasks",
                "events",
                "operations",
                "command_receipts",
                "outbox",
            ] {
                let total: i64 = conn
                    .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                        row.get(0)
                    })
                    .expect("count");
                counts.push((table.to_string(), total));
            }
            for (label, sql) in [
                (
                    "tasks(deleted)",
                    "SELECT COUNT(*) FROM tasks WHERE lifecycle = 'deleted'",
                ),
                (
                    "operations(deleted tasks)",
                    "SELECT COUNT(*) FROM operations WHERE task_id IN
                     (SELECT task_id FROM tasks WHERE lifecycle = 'deleted')",
                ),
                (
                    "events(deleted tasks)",
                    "SELECT COUNT(*) FROM events WHERE task_id IN
                     (SELECT task_id FROM tasks WHERE lifecycle = 'deleted')",
                ),
                (
                    "events(purged)",
                    "SELECT COUNT(*) FROM events WHERE event_type = 'event.purged'",
                ),
            ] {
                let total: i64 = conn.query_row(sql, [], |row| row.get(0)).expect("count");
                counts.push((label.to_string(), total));
            }
            counts
        };

        let before = census(&Connection::open(&path).expect("observer"));
        println!("--- before ---");
        for (label, total) in &before {
            println!("{label:32} {total}");
        }

        let started = std::time::Instant::now();
        let mut purged = 0_u32;
        loop {
            let batch = {
                let conn = Connection::open(&path).expect("observer");
                deleted_task_ids(&conn, 8).expect("worklist")
            };
            if batch.is_empty() {
                break;
            }
            for task in batch {
                match store
                    .with_immediate_transaction(|tx| purge_deleted_task_in_tx(tx, task))
                    .expect("purge")
                {
                    TaskPurge::Purged(report) => {
                        purged += 1;
                        println!("purged {task}: {}", report.summary());
                    }
                    other => panic!("worklist offered {task} but purge said {other:?}"),
                }
            }
        }
        let elapsed = started.elapsed();

        // What the reaper actually pays every tick once the backlog is gone,
        // and WHERE that cost is -- because the obvious answer is wrong.
        //
        // Measured against this store copy: the empty-worklist probe on a
        // fresh query connection costs ~2.3 ms, of which the connection open
        // is ~76 us and the worklist query on a REUSED connection is ~37 us.
        // `SELECT 1` on a fresh connection costs the same ~2.2 ms, so the
        // whole cost is the FIRST STATEMENT on a new query connection (schema
        // load and WAL index attach), not the query and not the missing index
        // on `tasks.lifecycle` that a plan would have tempted you to add.
        //
        // The sweep therefore pays what any per-tick read on a fresh query
        // connection pays here -- the same price `pending_resource_releases`
        // already pays one step earlier on the same tick. Reducing it means
        // changing how the store hands out query connections, which is a
        // separate change with its own snapshot-freshness argument.
        let idle_started = std::time::Instant::now();
        for _ in 0..100 {
            let conn = store.open_query_connection().expect("query connection");
            assert!(deleted_task_ids(&conn, 8).expect("worklist").is_empty());
        }
        let idle_us = idle_started.elapsed().as_micros() / 100;
        // How much of that is the connection, not the scan.
        let open_started = std::time::Instant::now();
        for _ in 0..100 {
            let _ = store.open_query_connection().expect("query connection");
        }
        let open_us = open_started.elapsed().as_micros() / 100;
        // A trivial statement on a fresh connection: whatever this costs is
        // the price of asking ANYTHING on a new connection, not of the
        // worklist query.
        let trivial_started = std::time::Instant::now();
        for _ in 0..100 {
            let conn = store.open_query_connection().expect("query connection");
            let _: i64 = conn
                .query_row("SELECT 1", [], |row| row.get(0))
                .expect("select 1");
        }
        let trivial_us = trivial_started.elapsed().as_micros() / 100;
        // And the worklist query on ONE reused connection.
        let reused = store.open_query_connection().expect("query connection");
        let reused_started = std::time::Instant::now();
        for _ in 0..100 {
            assert!(deleted_task_ids(&reused, 8).expect("worklist").is_empty());
        }
        let reused_us = reused_started.elapsed().as_micros() / 100;

        let after = census(&Connection::open(&path).expect("observer"));
        println!(
            "--- after ({purged} tasks in {} ms) ---",
            elapsed.as_millis()
        );
        println!("steady-state per-tick probe: {idle_us} us (open alone {open_us} us, open + SELECT 1 {trivial_us} us, worklist on a reused connection {reused_us} us)");
        for (label, total) in &after {
            println!("{label:32} {total}");
        }
    }
}
