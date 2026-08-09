use devmanager::client::{ClientModel, ClientModelBuilder};
use devmanager::domain::id::{EnvironmentId, ProjectId, SnapshotId, TaskId};
use devmanager::domain::snapshot::{SnapshotItem, SnapshotItemKey, SnapshotPage, SnapshotSection};
use devmanager::domain::task::{
    ReviewReadiness, TaskActivity, TaskAssignment, TaskAttention, TaskConnectivity, TaskFacts,
    TaskLifecycle, WorkspaceRef,
};
use devmanager::ui::shell::{
    NavigationRejection, NavigationResult, PointerButton, ReleaseRejection, Shell, TerminalRelease,
};
use devmanager::ui::task_cockpit::{TaskList, VirtualWindow, MAX_TASK_LIST_ITEMS};
use serde::Deserialize;
use std::fs;

#[derive(Debug, Deserialize)]
struct TaskListFixture {
    schema: String,
    task_ids: Vec<TaskId>,
    expected_count: usize,
    expected_overscan: usize,
}

fn fixture(name: &str) -> TaskListFixture {
    let path = format!("{}/tests/fixtures/ui/{name}", env!("CARGO_MANIFEST_DIR"));
    serde_json::from_str(&fs::read_to_string(path).expect("task-list fixture"))
        .expect("valid task-list fixture")
}

fn fixed_uuid_v7(tail: u8) -> [u8; 16] {
    [
        0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        tail,
    ]
}

fn task_id_from_index(index: u32) -> TaskId {
    let mut bytes = [
        0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00,
    ];
    bytes[12..].copy_from_slice(&index.to_be_bytes());
    TaskId::from_bytes(bytes).expect("fixed UUIDv7 task id")
}

fn task_item(id: TaskId, ordinal: usize) -> SnapshotItem {
    task_item_with_lifecycle(id, ordinal, TaskLifecycle::Open)
}

fn task_item_with_lifecycle(id: TaskId, ordinal: usize, lifecycle: TaskLifecycle) -> SnapshotItem {
    SnapshotItem::Task(devmanager::domain::snapshot::TaskSnapshotItem {
        task: TaskFacts {
            id,
            environment_id: EnvironmentId::from_bytes(fixed_uuid_v7(0x10)).expect("environment"),
            title: format!("Task {ordinal}"),
            description: None,
            project_id: ProjectId::from_bytes(fixed_uuid_v7(0x11)).expect("project"),
            workspace: WorkspaceRef::Main,
            assignment: TaskAssignment::LocalOwner,
            lifecycle,
            action_epoch: 0,
            revision: 1,
            created_at_ms: 1_725_000_000_000 + ordinal as i64,
        },
        connectivity: TaskConnectivity::Connected,
        attention: TaskAttention::None,
        activity: TaskActivity::Idle,
        review_readiness: ReviewReadiness::NotReady,
        primary_agent_id: None,
    })
}

fn model_from_ids(ids: &[TaskId]) -> ClientModel {
    let snapshot_id = SnapshotId::from_bytes(fixed_uuid_v7(0x20)).expect("snapshot");
    let mut builder = ClientModelBuilder::new();
    let chunks: Vec<&[TaskId]> = ids.chunks(1_000).collect();
    for (chunk_index, chunk) in chunks.iter().enumerate() {
        let after_item = if chunk_index == 0 {
            None
        } else {
            Some(SnapshotItemKey::Task(ids[chunk_index * 1_000 - 1]))
        };
        let next_cursor = (chunk_index + 1 < chunks.len()).then(|| vec![chunk_index as u8 + 1]);
        builder
            .ingest_page(SnapshotPage {
                snapshot_id,
                through_sequence: 1,
                section: SnapshotSection::Tasks,
                after_item,
                items: chunk
                    .iter()
                    .enumerate()
                    .map(|(offset, id)| task_item(*id, chunk_index * 1_000 + offset))
                    .collect(),
                encoded_bytes: 1,
                next_cursor,
            })
            .expect("task page");
    }
    if ids.is_empty() {
        builder
            .ingest_page(SnapshotPage {
                snapshot_id,
                through_sequence: 1,
                section: SnapshotSection::Tasks,
                after_item: None,
                items: Vec::new(),
                encoded_bytes: 1,
                next_cursor: None,
            })
            .expect("empty task page");
    }
    for section in [
        SnapshotSection::AgentSessions,
        SnapshotSection::Artifacts,
        SnapshotSection::Resources,
        SnapshotSection::Operations,
    ] {
        builder
            .ingest_page(SnapshotPage {
                snapshot_id,
                through_sequence: 1,
                section,
                after_item: None,
                items: Vec::new(),
                encoded_bytes: 1,
                next_cursor: None,
            })
            .expect("empty related section");
    }
    builder.finish().expect("complete client model")
}

fn model_from_task_items(items: Vec<SnapshotItem>) -> ClientModel {
    let snapshot_id = SnapshotId::from_bytes(fixed_uuid_v7(0x20)).expect("snapshot");
    let mut builder = ClientModelBuilder::new();
    builder
        .ingest_page(SnapshotPage {
            snapshot_id,
            through_sequence: 1,
            section: SnapshotSection::Tasks,
            after_item: None,
            items,
            encoded_bytes: 1,
            next_cursor: None,
        })
        .expect("task page");
    for section in [
        SnapshotSection::AgentSessions,
        SnapshotSection::Artifacts,
        SnapshotSection::Resources,
        SnapshotSection::Operations,
    ] {
        builder
            .ingest_page(SnapshotPage {
                snapshot_id,
                through_sequence: 1,
                section,
                after_item: None,
                items: Vec::new(),
                encoded_bytes: 1,
                next_cursor: None,
            })
            .expect("empty related section");
    }
    builder.finish().expect("complete client model")
}

#[test]
fn navigation_mouse_down_commits_only_tasks_in_the_current_bounded_inbox() {
    let first = task_id_from_index(1);
    let second = task_id_from_index(2);
    let foreign = task_id_from_index(3);
    let model = model_from_ids(&[first, second]);
    let inbox = TaskList::from_model(&model);
    let mut shell = Shell::new(Some(first));
    let epoch = shell.navigation_epoch();
    let owner = shell
        .terminal_mouse_down(7, first, PointerButton::Primary, epoch, Some(first))
        .expect("selected task owns the pointer before navigation");

    let navigation = shell.navigation_mouse_down(second, epoch, &inbox);
    assert!(navigation.consumed());
    assert_eq!(
        navigation,
        NavigationResult::Committed {
            task_id: second,
            navigation_epoch: epoch + 1,
        }
    );
    assert_eq!(shell.selected_task(), Some(second));
    assert!(shell.navigation_mouse_up());
    assert_eq!(
        shell.terminal_mouse_up(Some(owner)),
        TerminalRelease::Rejected(ReleaseRejection::NoOwner)
    );

    let epoch = shell.navigation_epoch();
    assert_eq!(
        shell.navigation_mouse_down(foreign, epoch, &inbox),
        NavigationResult::Rejected {
            reason: NavigationRejection::TaskNotInInbox,
        }
    );
    assert_eq!(shell.selected_task(), Some(second));
    assert_eq!(shell.navigation_epoch(), epoch);
    assert_eq!(
        shell.navigation_mouse_down(first, epoch - 1, &inbox),
        NavigationResult::Rejected {
            reason: NavigationRejection::StaleEpoch,
        }
    );
}

#[test]
fn inbox_excludes_archived_tasks_and_rejects_their_navigation() {
    let open = task_id_from_index(11);
    let archived = task_id_from_index(12);
    let model = model_from_task_items(vec![
        task_item_with_lifecycle(open, 0, TaskLifecycle::Open),
        task_item_with_lifecycle(archived, 1, TaskLifecycle::Archived),
    ]);
    let inbox = TaskList::from_model(&model);

    assert_eq!(inbox.task_ids(), &[open]);
    let mut shell = Shell::new(Some(open));
    let epoch = shell.navigation_epoch();
    assert_eq!(
        shell.navigation_mouse_down(archived, epoch, &inbox),
        NavigationResult::Rejected {
            reason: NavigationRejection::TaskNotInInbox,
        }
    );
    assert_eq!(shell.selected_task(), Some(open));
    assert_eq!(shell.navigation_epoch(), epoch);
}

#[test]
fn task_list_consumes_only_model_ids_and_keeps_deterministic_order() {
    let fixture = fixture("task-list-5000.json");
    assert_eq!(fixture.schema, "devmanager.ui.task-cockpit.task-list/v1");
    assert_eq!(fixture.task_ids.len(), fixture.expected_count);
    assert_eq!(fixture.expected_count, 5_000);

    let model = model_from_ids(&fixture.task_ids);
    let before = model.clone();
    let first = TaskList::from_model(&model);
    let second = TaskList::from_model(&model);

    assert_eq!(model, before, "projection must not mutate ClientModel");
    assert_eq!(first.task_ids(), fixture.task_ids.as_slice());
    assert_eq!(
        first, second,
        "same model must produce the same local projection"
    );
    assert_eq!(first.overflow(), None);
    assert_eq!(first.len(), 5_000);
    assert_eq!(first.virtual_window().overscan(), fixture.expected_overscan);
    assert!(first.virtual_window().overscan() <= 80);
}

#[test]
fn task_list_reports_overflow_instead_of_silently_dropping_tasks() {
    let fixture = fixture("task-list-5000.json");
    let mut ids = fixture.task_ids;
    ids.push(task_id_from_index(5_000));
    let model = model_from_ids(&ids);
    let list = TaskList::from_model(&model);

    assert_eq!(list.len(), MAX_TASK_LIST_ITEMS);
    let overflow = list.overflow().expect("overflow must be explicit");
    assert_eq!(overflow.limit, MAX_TASK_LIST_ITEMS);
    assert_eq!(overflow.total_count, 5_001);
    assert_eq!(overflow.retained_count, MAX_TASK_LIST_ITEMS);
    assert_eq!(list.task_ids().last(), ids.get(MAX_TASK_LIST_ITEMS - 1));
}

#[test]
fn virtual_window_keeps_visible_rows_and_fixed_bounded_overscan_local() {
    let fixture = fixture("task-list-5000.json");
    let model = model_from_ids(&fixture.task_ids);
    let mut list = TaskList::from_model(&model);
    list.set_viewport(2_500, 40).expect("valid local viewport");

    let window: VirtualWindow = list.virtual_window();
    assert_eq!(window.visible_range(), 2_500..2_540);
    assert_eq!(window.overscan(), fixture.expected_overscan);
    assert_eq!(
        window.render_range(list.len()),
        2_500 - fixture.expected_overscan..2_540 + fixture.expected_overscan
    );
    assert!(window.render_range(list.len()).len() <= 40 + 2 * 80);

    list.set_viewport(4_990, 40)
        .expect("viewport clamps at list end");
    assert_eq!(list.virtual_window().visible_range(), 4_990..5_000);
    assert!(list.virtual_window().render_range(list.len()).end <= list.len());

    let before = list.virtual_window();
    assert!(list.set_viewport(0, 0).is_err());
    assert_eq!(
        list.virtual_window(),
        before,
        "invalid viewport has zero effects"
    );
}
