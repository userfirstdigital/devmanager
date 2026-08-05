use std::fmt;

use rusqlite::Connection;

use crate::domain::id::{SnapshotId, TaskId};
use crate::domain::snapshot::{SnapshotItem, SnapshotPage, SnapshotSection, TaskSnapshotItem};
use crate::kernel::command_bus;
use crate::kernel::store::{u64_from_nonnegative_i64, KernelStore, StoreError};

/// One immutable, read-only SQLite view of the durable kernel projections.
///
/// The owned connection holds a deferred read transaction open. Dropping this
/// value releases the view; no OS process or other runtime resource is owned.
#[allow(dead_code)] // consumed by the bounded host registry in a later phase
pub(crate) struct SnapshotSession {
    snapshot_id: SnapshotId,
    through_sequence: u64,
    conn: Connection,
}

impl fmt::Debug for SnapshotSession {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SnapshotSession")
            .field("snapshot_id", &self.snapshot_id)
            .field("through_sequence", &self.through_sequence)
            .finish_non_exhaustive()
    }
}

impl KernelStore {
    /// Pin a read-only snapshot at the current global durable event sequence.
    #[allow(dead_code)] // consumed by the bounded host registry in a later phase
    pub(crate) fn begin_snapshot(&self) -> Result<SnapshotSession, StoreError> {
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
            conn,
        })
    }
}

impl SnapshotSession {
    /// Read the task-row section from the view pinned by `begin_snapshot`.
    #[allow(dead_code)] // consumed by the bounded host registry in a later phase
    pub(crate) fn tasks_page(&self) -> Result<SnapshotPage, StoreError> {
        let task_ids = {
            let mut stmt = self
                .conn
                .prepare("SELECT task_id FROM tasks ORDER BY task_id ASC")?;
            let rows = stmt.query_map([], |row| row.get::<_, Vec<u8>>(0))?;
            let mut task_ids = Vec::new();
            for row in rows {
                let bytes = row?;
                let bytes: [u8; 16] =
                    bytes
                        .as_slice()
                        .try_into()
                        .map_err(|_| StoreError::CodecMismatch {
                            detail: "tasks.task_id must be 16 bytes".into(),
                        })?;
                let task_id =
                    TaskId::from_bytes(bytes).map_err(|err| StoreError::CodecMismatch {
                        detail: format!("tasks.task_id: {err}"),
                    })?;
                task_ids.push(task_id);
            }
            task_ids
        };

        let mut items = Vec::with_capacity(task_ids.len());
        for task_id in task_ids {
            let snapshot =
                command_bus::load_task_snapshot(&self.conn, task_id)?.ok_or_else(|| {
                    StoreError::Projection("task disappeared from pinned snapshot".into())
                })?;
            items.push(SnapshotItem::Task(TaskSnapshotItem {
                task: snapshot.task,
                connectivity: snapshot.connectivity,
                attention: snapshot.attention,
                activity: snapshot.activity,
                review_readiness: snapshot.review_readiness,
                primary_agent_id: snapshot.primary_agent_id,
            }));
        }

        Ok(SnapshotPage {
            snapshot_id: self.snapshot_id,
            through_sequence: self.through_sequence,
            section: SnapshotSection::Tasks,
            items,
        })
    }
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
    use crate::domain::command::{
        Command, CommandEnvelope, CommandReceipt, CreateTaskIntent, RenameTaskIntent,
    };
    use crate::domain::id::{
        AgentSessionId, ClientId, CommandId, EnvironmentId, ProjectId, TaskId,
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
        let intent = CreateTaskIntent {
            id: task_id,
            environment_id: environment_id(0x10),
            title: "Ship kernel".into(),
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
        let snapshot = store.begin_snapshot().expect("begin frozen snapshot");

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

        let first = snapshot.tasks_page().expect("first frozen task page");
        let second = snapshot.tasks_page().expect("repeat frozen task page");
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

        let snapshot = store.begin_snapshot().expect("begin snapshot");
        assert!(matches!(
            snapshot
                .tasks_page()
                .expect_err("dangling primary agent must fail closed"),
            StoreError::Projection(_)
        ));
    }
}
