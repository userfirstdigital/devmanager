use std::path::PathBuf;

use devmanager::client::{ClientModel, ClientModelBuilder};
use devmanager::domain::id::TaskId;
use devmanager::domain::snapshot::{SnapshotItem, SnapshotPage};
use devmanager::domain::task::{TaskActivity, VisibleTaskStatus};
use devmanager::ui::shell::Shell;
use devmanager::ui::task_cockpit::{
    HeaderAction, HeaderField, PrimaryAgentProjection, TaskHeaderModel, TopBarModel,
    TopBarProjectionInput, WorkspaceProjection,
};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
struct HeaderFixture {
    snapshot_pages: Vec<SnapshotPage>,
    selected_task_id: TaskId,
    top_bar: TopBarProjectionInput,
}

fn fixture() -> HeaderFixture {
    serde_json::from_str(include_str!("fixtures/ui/header-states.json"))
        .expect("header fixture must be valid")
}

fn model_from_pages(pages: &[SnapshotPage]) -> ClientModel {
    let mut builder = ClientModelBuilder::new();
    for page in pages {
        builder
            .ingest_page(page.clone())
            .expect("fixture snapshot page must be admitted");
    }
    builder.finish().expect("fixture snapshot must finish")
}

#[test]
fn header_projects_task_context_and_stable_agent_status_links() {
    let fixture = fixture();
    let model = model_from_pages(&fixture.snapshot_pages);

    let header = TaskHeaderModel::from_model(&model, fixture.selected_task_id)
        .expect("selected task must project");

    assert_eq!(header.identity.task_id, fixture.selected_task_id);
    assert_eq!(header.identity.revision, 9);
    assert_eq!(header.identity.action_epoch, 4);
    assert_eq!(header.title, "Ship the native cockpit");
    assert_eq!(
        header.project.id.to_string(),
        "018f60b0-9c1a-7001-8000-000000000011"
    );
    assert_eq!(header.project.label, "018f60b0-9c1a-7001-8000-000000000011");
    assert_eq!(
        header.workspace,
        WorkspaceProjection::Worktree {
            path: PathBuf::from(r"C:\Code\demo\worktree"),
            branch: "codex/header".to_string(),
        }
    );

    let primary = match &header.primary {
        PrimaryAgentProjection::Present(primary) => primary,
        other => panic!("expected Primary projection, got {other:?}"),
    };
    assert_eq!(primary.provider, "claude");
    assert_eq!(
        primary.identity.agent_id.to_string(),
        "018f60b0-9c1a-7001-8000-000000000022"
    );
    assert_eq!(primary.identity.runtime_generation, 3);
    assert_eq!(header.specialists.len(), 1);
    assert_eq!(header.specialists[0].label, "reviewer");
    assert_eq!(header.specialists[0].provider, "codex");

    assert_eq!(header.turn.activity, TaskActivity::Working);
    assert_eq!(header.turn.status, VisibleTaskStatus::Working);
    assert_eq!(
        header.status.action,
        HeaderAction::OpenCommandCenter {
            identity: header.identity,
        }
    );
    assert!(header.status.description.contains("working"));
    assert!(header
        .accessible_description
        .contains("Ship the native cockpit"));
}

#[test]
fn shell_projects_only_its_captured_selected_task() {
    let fixture = fixture();
    let model = model_from_pages(&fixture.snapshot_pages);
    let shell = Shell::new(Some(fixture.selected_task_id));

    let header = shell.task_header(&model).expect("selected task header");
    assert_eq!(header.identity.task_id, fixture.selected_task_id);
}

#[test]
fn top_bar_keeps_global_facts_hides_stale_quota_and_labels_cpu_diagnostics() {
    let fixture = fixture();
    let top_bar = TopBarModel::from_input(&fixture.top_bar);

    assert_eq!(top_bar.quotas.len(), 1);
    assert_eq!(top_bar.quotas[0].provider, "claude");
    assert_eq!(top_bar.quotas[0].detail, "72% remaining");
    assert_eq!(
        top_bar.resources.as_ref().unwrap().memory_bytes,
        Some(12_345_678)
    );

    let resources = top_bar.resources.as_ref().expect("host resources");
    let cpu = resources.cpu.as_ref().expect("aggregate CPU");
    assert_eq!(cpu.whole_machine_percent, 100.0);
    let diagnostic = cpu.diagnostic.as_ref().expect("core-equivalent diagnostic");
    assert_eq!(diagnostic.label, "Core-equivalent CPU (diagnostic)");
    assert!((diagnostic.core_equivalent - 64.0).abs() < f32::EPSILON);
    assert!(top_bar.accessible_description.contains("healthy"));
    assert!(top_bar.accessible_description.contains("connected"));
}

#[test]
fn narrow_header_uses_priority_overflow_with_accessible_text() {
    let fixture = fixture();
    let model = model_from_pages(&fixture.snapshot_pages);
    let header = TaskHeaderModel::from_model(&model, fixture.selected_task_id)
        .expect("selected task must project");

    let layout = header.responsive_layout(360);

    assert!(layout.inline.contains(&HeaderField::Title));
    assert!(layout.inline.contains(&HeaderField::TurnStatus));
    assert!(layout.overflow.contains(&HeaderField::Project));
    assert!(layout.overflow.contains(&HeaderField::Workspace));
    assert!(layout.overflow.contains(&HeaderField::Specialists));
    let overflow = layout.overflow_control.expect("overflow control");
    assert_eq!(overflow.label, "More task details");
    assert!(!overflow.description.is_empty());
    assert!(layout.accessible_description.contains("More task details"));
}

#[test]
fn header_does_not_invent_a_primary_provider_when_the_reference_is_missing() {
    let fixture = fixture();
    let mut pages = fixture.snapshot_pages.clone();
    for page in &mut pages {
        for item in &mut page.items {
            if let SnapshotItem::Task(task) = item {
                task.primary_agent_id = None;
            }
        }
    }
    let model = model_from_pages(&pages);

    let header = TaskHeaderModel::from_model(&model, fixture.selected_task_id)
        .expect("selected task must project");

    assert!(matches!(
        header.primary,
        PrimaryAgentProjection::Unavailable { .. }
    ));
    assert!(header
        .accessible_description
        .contains("Primary provider unavailable"));
}
