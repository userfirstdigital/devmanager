use devmanager::client::{ClientModel, ClientModelBuilder};
use devmanager::domain::id::{EnvironmentId, ProjectId, SnapshotId, TaskId};
use devmanager::domain::snapshot::{SnapshotItem, SnapshotPage, SnapshotSection, TaskSnapshotItem};
use devmanager::domain::task::{
    ReviewReadiness, TaskActivity, TaskAssignment, TaskAttention, TaskConnectivity, TaskFacts,
    TaskLifecycle, WorkspaceRef,
};
use devmanager::ui::preview::{
    parse_preview_args, PreviewApplication, PreviewDismiss, PreviewError, PreviewOutputCapability,
    PreviewPathPolicy, PreviewRequest, PREVIEW_SCHEMA,
};
use devmanager::ui::task_cockpit::{
    Inbox, InboxError, InboxFilter, InboxSection, InboxState, UnreadCursor, DEFAULT_VISIBLE_ROWS,
};
use devmanager::ui::{self, PreviewInitReport};
use std::cell::Cell;
use std::fs;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Mutex;
use tempfile::{tempdir, TempDir};

const FIXTURE_JSON: &str = r#"{
  "schema": "devmanager.ui.preview/v1",
  "id": "theme-gallery",
  "title": "Theme Gallery",
  "capture": {
    "cursor": "excluded",
    "border": "excluded"
  },
  "root": {
    "kind": "minimal",
    "label": "DevManager native preview"
  }
}"#;

static HEADLESS_INIT_TEST_LOCK: Mutex<()> = Mutex::new(());

fn fixed_uuid_v7(tail: u8) -> [u8; 16] {
    [
        0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        tail,
    ]
}

fn repository_policy() -> PreviewPathPolicy {
    PreviewPathPolicy::for_workspace(env!("CARGO_MANIFEST_DIR"))
}

fn repository_fixture() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/ui/theme-gallery.json")
}

fn repository_output(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join(".devmanager-next/evidence/phase-05/screenshots")
        .join(name)
}

fn temporary_policy() -> (TempDir, PreviewPathPolicy) {
    let root = tempdir().expect("temporary preview root");
    let fixture_root = root.path().join("fixtures/ui");
    let output_root = root.path().join("evidence/screenshots");
    fs::create_dir_all(&fixture_root).expect("fixture root");
    fs::create_dir_all(&output_root).expect("output root");
    let policy = PreviewPathPolicy::new(&fixture_root, &output_root, root.path().join("temp"));
    (root, policy)
}

fn write_fixture(policy: &PreviewPathPolicy, name: &str, contents: &str) -> PathBuf {
    let path = policy.fixture_root().join(name);
    fs::write(&path, contents).expect("fixture contents");
    path
}

fn valid_request(policy: &PreviewPathPolicy) -> PreviewRequest {
    let fixture = write_fixture(policy, "valid.json", FIXTURE_JSON);
    let output = policy.output_root().join("preview.png");
    PreviewRequest::validate(fixture, output, policy).expect("valid preview request")
}

#[test]
fn component_init_registers_devmanager_resources_once() {
    let _lock = HEADLESS_INIT_TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let policy = repository_policy();
    let request = PreviewRequest::validate(
        repository_fixture(),
        repository_output("component-init.png"),
        &policy,
    )
    .expect("checked-in fixture should be accepted");
    let preview = PreviewApplication::load(request, &policy).expect("fixture should load");

    let before = ui::component_init_count();
    let report: PreviewInitReport = preview
        .initialize_headless()
        .expect("headless initialization should be isolated");
    let repeated = preview
        .initialize_headless()
        .expect("repeated initialization should reuse the first report");

    assert_eq!(report, repeated);
    assert_eq!(report.component_init_count, before + 1);
    assert!(report.assets_registered);
    assert!(report.fonts_registered);
    assert!(report.actions_registered);
    assert!(report.root_constructed);
    assert!(!report.production_host_started);
    assert_eq!(ui::component_init_count(), report.component_init_count);
}

#[test]
fn preview_output_metadata_is_deterministic_and_does_not_claim_write() {
    let (_root, policy) = temporary_policy();
    let request = valid_request(&policy);
    let output = request.output_path().to_path_buf();
    let preview = PreviewApplication::load(request, &policy).expect("fixture should load");

    let first = preview.output_metadata();
    let second = preview.output_metadata();

    assert_eq!(first, second);
    assert_eq!(first.schema, PREVIEW_SCHEMA);
    assert_eq!(first.fixture_id, "theme-gallery");
    assert_eq!(first.output_path, output);
    assert_eq!(first.format, "png");
    assert_eq!(
        first.capability,
        if cfg!(windows) {
            PreviewOutputCapability::VisibleWindowsNativeCapture
        } else {
            PreviewOutputCapability::HeadlessProjectionOnly
        }
    );
    assert!(!first.output_written);
    assert!(!first.host_started);
    assert!(!output.exists());
}

#[test]
fn preview_fixture_rejects_whitespace_only_required_fields() {
    let (_root, policy) = temporary_policy();
    let fixture = write_fixture(
        &policy,
        "whitespace.json",
        &FIXTURE_JSON
            .replace("theme-gallery", "   ")
            .replace("Theme Gallery", "  ")
            .replace("DevManager native preview", " "),
    );
    let request = PreviewRequest::validate(
        fixture,
        policy.output_root().join("whitespace.png"),
        &policy,
    )
    .expect("path validation should precede fixture parsing");

    let error = PreviewApplication::load(request, &policy)
        .expect_err("whitespace-only fixture fields must be rejected");
    assert!(matches!(error, PreviewError::MalformedFixture { .. }));
}

#[test]
fn preview_root_projection_is_deterministic() {
    let (_root, policy) = temporary_policy();
    let first = PreviewApplication::load(valid_request(&policy), &policy).expect("first fixture");

    let fixture = write_fixture(&policy, "valid-again.json", FIXTURE_JSON);
    let second_request = PreviewRequest::validate(
        fixture,
        policy.output_root().join("preview-again.png"),
        &policy,
    )
    .expect("second request");
    let second = PreviewApplication::load(second_request, &policy).expect("second fixture");

    assert_eq!(first.root_snapshot(), second.root_snapshot());
    assert_eq!(first.resources(), second.resources());
    assert_eq!(
        first.root_snapshot().body,
        "DevManager native preview: Theme Gallery"
    );
}

#[test]
fn preview_validation_accepts_only_checked_fixture_and_png_roots() {
    let (_root, policy) = temporary_policy();
    let request = valid_request(&policy);

    assert_eq!(
        request.fixture_path(),
        policy.fixture_root().join("valid.json")
    );
    assert_eq!(
        request.output_path(),
        policy.output_root().join("preview.png")
    );
}

#[test]
fn preview_validation_rejects_no_args() {
    let policy = repository_policy();
    let error = parse_preview_args(Vec::<String>::new(), &policy).expect_err("no args must fail");

    assert!(matches!(error, PreviewError::Usage(_)));
}

#[test]
fn preview_validation_rejects_traversal_and_outside_fixture() {
    let (root, policy) = temporary_policy();
    let outside_fixture = root.path().join("outside.json");
    fs::write(&outside_fixture, FIXTURE_JSON).expect("outside fixture");
    let output = policy.output_root().join("outside.png");

    let error = PreviewRequest::validate(outside_fixture, output, &policy)
        .expect_err("outside fixture must fail");
    assert!(matches!(error, PreviewError::OutsideApprovedRoot { .. }));

    let fixture = write_fixture(&policy, "traversal.json", FIXTURE_JSON);
    let traversal = policy.output_root().join("..").join("escaped.png");
    let error = PreviewRequest::validate(fixture, traversal, &policy)
        .expect_err("traversal output must fail");
    assert!(matches!(error, PreviewError::OutsideApprovedRoot { .. }));
}

#[test]
fn preview_validation_rejects_missing_oversized_and_malformed_fixtures() {
    let (_root, policy) = temporary_policy();
    let missing = policy.fixture_root().join("missing.json");
    let missing_error =
        PreviewRequest::validate(&missing, policy.output_root().join("missing.png"), &policy)
            .expect_err("missing fixture must fail");
    assert!(matches!(missing_error, PreviewError::FixtureMissing { .. }));

    let malformed = write_fixture(&policy, "malformed.json", "{ not json");
    let malformed_request = PreviewRequest::validate(
        malformed,
        policy.output_root().join("malformed.png"),
        &policy,
    )
    .expect("path validation should precede parsing");
    let malformed_error = PreviewApplication::load(malformed_request, &policy)
        .expect_err("malformed fixture must fail");
    assert!(matches!(
        malformed_error,
        PreviewError::MalformedFixture { .. }
    ));

    let oversized = write_fixture(
        &policy,
        "oversized.json",
        &format!("{}{}", FIXTURE_JSON, "x".repeat(300_000)),
    );
    let oversized_error = PreviewRequest::validate(
        oversized,
        policy.output_root().join("oversized.png"),
        &policy,
    )
    .expect_err("oversized fixture must fail");
    assert!(matches!(
        oversized_error,
        PreviewError::FixtureTooLarge { .. }
    ));
}

#[test]
fn preview_validation_rejects_existing_sensitive_output() {
    let (_root, policy) = temporary_policy();
    let fixture = write_fixture(&policy, "existing.json", FIXTURE_JSON);
    let output = policy.output_root().join("existing.png");
    fs::write(&output, b"do not overwrite").expect("existing output");

    let error = PreviewRequest::validate(fixture, output, &policy)
        .expect_err("existing output must not be overwritten");
    assert!(matches!(error, PreviewError::OutputAlreadyExists { .. }));

    let production_fixture = write_fixture(&policy, "production.json", FIXTURE_JSON);
    let production = Path::new(r"C:\Users\micro\AppData\Roaming\DevManager\config.json");
    let error = PreviewRequest::validate(production_fixture, production, &policy)
        .expect_err("production config path must fail");
    assert!(matches!(error, PreviewError::SensitivePath { .. }));

    let sensitive_fixture = write_fixture(&policy, "config.json", FIXTURE_JSON);
    let error = PreviewRequest::validate(
        sensitive_fixture,
        policy.output_root().join("fixture.png"),
        &policy,
    )
    .expect_err("sensitive fixture path must fail");
    assert!(matches!(error, PreviewError::SensitivePath { .. }));
}

#[cfg(not(windows))]
#[test]
fn preview_execution_returns_explicit_visible_windows_unavailable_error_without_writing_output() {
    let (_root, policy) = temporary_policy();
    let request = valid_request(&policy);
    let output = request.output_path().to_path_buf();
    let preview = PreviewApplication::load(request, &policy).expect("fixture should load");

    let error = preview
        .render_to_output()
        .expect_err("non-Windows visual capture must be unavailable");
    assert!(matches!(
        error,
        PreviewError::VisibleWindowsCaptureUnavailable { .. }
    ));
    assert!(!output.exists());
}

#[test]
fn preview_request_construction_rejects_production_paths_before_load() {
    let (root, policy) = temporary_policy();
    let production_root = root.path().join("production");
    fs::create_dir_all(&production_root).expect("production fixture directory");
    let production_fixture = production_root.join("session.json");
    fs::write(&production_fixture, FIXTURE_JSON).expect("production fixture");

    let error = PreviewRequest::validate(
        production_fixture,
        policy.output_root().join("bypass.png"),
        &policy,
    )
    .expect_err("PreviewRequest construction must not bypass isolation");
    assert!(matches!(error, PreviewError::OutsideApprovedRoot { .. }));
}

#[test]
fn task_cockpit_actions_are_registered_and_dispatch_through_gpui() {
    let dispatched = Rc::new(Cell::new(false));
    let dispatched_in_app = Rc::clone(&dispatched);
    let application = gpui::Application::headless();

    application.run(move |cx| {
        let expected = [
            "host.actions",
            "host.status",
            "task.list",
            "task.show",
            "task.create",
            "task.rename",
        ];
        for action_name in expected {
            assert!(
                cx.all_action_names().contains(&action_name),
                "{action_name} must be an actual GPUI action registration"
            );
            let action = cx
                .build_action(action_name, None)
                .expect("registered actions must be dynamically buildable");
            cx.dispatch_action(action.as_ref());
        }

        cx.on_action::<PreviewDismiss>(move |_, _| dispatched_in_app.set(true));
        cx.dispatch_action(&PreviewDismiss);
        assert!(
            dispatched.get(),
            "GPUI must dispatch the retained PreviewDismiss action"
        );
        cx.quit();
    });
}

#[test]
fn components_gallery_fixture_is_consumed_and_validated_structurally() {
    let policy = repository_policy();
    let request = PreviewRequest::validate(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/ui/component-gallery.json"),
        repository_output("component-gallery-structural.png"),
        &policy,
    )
    .expect("component gallery path should be accepted");
    let preview = PreviewApplication::load(request, &policy).expect("gallery should load");
    let gallery = preview
        .component_gallery()
        .expect("component gallery must be present in the preview projection");

    assert_eq!(gallery.themes.len(), 2);
    assert_eq!(gallery.densities.len(), 2);
    assert_eq!(gallery.scales, vec![100, 125, 150, 200]);
    assert!(gallery.states.len() >= 7);
    assert!(gallery.samples.long_text.chars().count() > 256);
    assert!(gallery.samples.unicode.contains('界'));
    assert!(!gallery.samples.missing.is_empty());
    assert!(!gallery.samples.error.is_empty());
    assert!(!gallery.samples.loading.is_empty());
    assert!(!gallery.samples.empty.is_empty());
    assert!(!gallery.samples.overflow.is_empty());
}

#[test]
fn components_gallery_fixture_rejects_missing_state_coverage() {
    let (_root, policy) = temporary_policy();
    let fixture = write_fixture(
        &policy,
        "component-gallery-missing-state.json",
        &include_str!("fixtures/ui/component-gallery.json")
            .replace("\"destructive\"", "\"not-a-state\""),
    );
    let request = PreviewRequest::validate(
        fixture,
        policy
            .output_root()
            .join("component-gallery-missing-state.png"),
        &policy,
    )
    .expect("path validation should precede fixture parsing");
    let error = PreviewApplication::load(request, &policy)
        .expect_err("unknown component state must fail closed");
    assert!(matches!(error, PreviewError::MalformedFixture { .. }));
}

#[test]
fn components_models_project_deterministically_for_both_token_themes() {
    use devmanager::client::action::ActionRequest;
    use devmanager::ui::components::button::Button;
    use devmanager::ui::tokens::{theme, Density, Scale, ThemeMode};
    let button = Button::new("Inspect", ActionRequest::TaskList).expect("button");
    let dark = button.presentation(theme(ThemeMode::Dark, Density::Compact, Scale::Scale100));
    let light = button.presentation(theme(ThemeMode::Light, Density::Compact, Scale::Scale100));
    assert_ne!(dark.background, light.background);
    assert_eq!(dark.focus_ring, None);
    assert_eq!(light.focus_ring, None);
}

fn inbox_task_item(
    id: TaskId,
    title: &str,
    lifecycle: TaskLifecycle,
    connectivity: TaskConnectivity,
    attention: TaskAttention,
    activity: TaskActivity,
    review_readiness: ReviewReadiness,
    created_at_ms: i64,
) -> SnapshotItem {
    SnapshotItem::Task(TaskSnapshotItem {
        task: TaskFacts {
            id,
            environment_id: EnvironmentId::from_bytes(fixed_uuid_v7(0x10)).expect("environment"),
            title: title.into(),
            description: None,
            project_id: ProjectId::from_bytes(fixed_uuid_v7(0x11)).expect("project"),
            workspace: WorkspaceRef::Main,
            assignment: TaskAssignment::LocalOwner,
            lifecycle,
            action_epoch: 0,
            revision: created_at_ms as u64,
            created_at_ms,
        },
        connectivity,
        attention,
        activity,
        review_readiness,
        primary_agent_id: None,
    })
}

fn inbox_model(items: Vec<SnapshotItem>) -> ClientModel {
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

fn inbox_task_id(index: u32) -> TaskId {
    let mut bytes = fixed_uuid_v7(0);
    bytes[12..].copy_from_slice(&index.to_be_bytes());
    TaskId::from_bytes(bytes).expect("task id")
}

#[test]
fn inbox_attention_order_is_deterministic_and_selection_is_task_id_based() {
    let states = [
        (
            TaskConnectivity::Connected,
            TaskAttention::None,
            TaskActivity::Idle,
            ReviewReadiness::NotReady,
        ),
        (
            TaskConnectivity::Connected,
            TaskAttention::Failed,
            TaskActivity::Idle,
            ReviewReadiness::NotReady,
        ),
        (
            TaskConnectivity::Connected,
            TaskAttention::None,
            TaskActivity::Idle,
            ReviewReadiness::Ready,
        ),
        (
            TaskConnectivity::Disconnected,
            TaskAttention::None,
            TaskActivity::Idle,
            ReviewReadiness::NotReady,
        ),
        (
            TaskConnectivity::Connected,
            TaskAttention::NeedsAnswer,
            TaskActivity::Idle,
            ReviewReadiness::NotReady,
        ),
        (
            TaskConnectivity::Connected,
            TaskAttention::None,
            TaskActivity::Settling,
            ReviewReadiness::NotReady,
        ),
        (
            TaskConnectivity::Connected,
            TaskAttention::None,
            TaskActivity::Working,
            ReviewReadiness::NotReady,
        ),
        (
            TaskConnectivity::Connected,
            TaskAttention::NeedsApproval,
            TaskActivity::Idle,
            ReviewReadiness::NotReady,
        ),
        (
            TaskConnectivity::Connected,
            TaskAttention::UncertainOutcome,
            TaskActivity::Idle,
            ReviewReadiness::NotReady,
        ),
    ];
    let mut items = states
        .into_iter()
        .enumerate()
        .map(
            |(index, (connectivity, attention, activity, review_readiness))| {
                inbox_task_item(
                    inbox_task_id(index as u32),
                    "same title",
                    TaskLifecycle::Open,
                    connectivity,
                    attention,
                    activity,
                    review_readiness,
                    1_000,
                )
            },
        )
        .collect::<Vec<_>>();
    items.extend([
        inbox_task_item(
            inbox_task_id(9),
            "Beta",
            TaskLifecycle::Open,
            TaskConnectivity::Connected,
            TaskAttention::None,
            TaskActivity::Idle,
            ReviewReadiness::NotReady,
            2_000,
        ),
        inbox_task_item(
            inbox_task_id(10),
            "alpha",
            TaskLifecycle::Open,
            TaskConnectivity::Connected,
            TaskAttention::None,
            TaskActivity::Idle,
            ReviewReadiness::NotReady,
            2_000,
        ),
    ]);
    let model = inbox_model(items);
    let before = model.clone();
    let unread = UnreadCursor::from([(inbox_task_id(0), 3)]);
    let inbox = Inbox::from_model_with_unread(&model, &unread);

    assert_eq!(
        model, before,
        "inbox projection must not mutate ClientModel"
    );
    assert_eq!(inbox.section_rows(InboxSection::NeedsMe).len(), 5);
    assert_eq!(inbox.section_rows(InboxSection::Running).len(), 2);
    assert_eq!(inbox.section_rows(InboxSection::Ready).len(), 1);
    assert_eq!(inbox.section_rows(InboxSection::Recent).len(), 3);
    assert_eq!(
        inbox
            .section_rows(InboxSection::NeedsMe)
            .iter()
            .map(|row| row.task_id)
            .collect::<Vec<_>>(),
        vec![
            inbox_task_id(3),
            inbox_task_id(1),
            inbox_task_id(8),
            inbox_task_id(7),
            inbox_task_id(4)
        ]
    );
    assert_eq!(
        inbox
            .section_rows(InboxSection::Running)
            .iter()
            .map(|row| row.task_id)
            .collect::<Vec<_>>(),
        vec![inbox_task_id(6), inbox_task_id(5)]
    );
    assert_eq!(
        inbox
            .section_rows(InboxSection::Recent)
            .iter()
            .map(|row| row.task_id)
            .collect::<Vec<_>>(),
        vec![inbox_task_id(10), inbox_task_id(9), inbox_task_id(0)]
    );
    assert_eq!(inbox.row(inbox_task_id(0)).unwrap().unread_event_count, 3);
    assert_eq!(inbox.select_task(inbox_task_id(8)), Some(inbox_task_id(8)));
    assert_eq!(inbox.select_task(TaskId::new()), None);
}

#[test]
fn inbox_keeps_legacy_task_list_identity_and_viewport_accessors_synchronized() {
    let model = inbox_model(vec![
        inbox_task_item(
            inbox_task_id(0),
            "recent",
            TaskLifecycle::Open,
            TaskConnectivity::Connected,
            TaskAttention::None,
            TaskActivity::Idle,
            ReviewReadiness::NotReady,
            1,
        ),
        inbox_task_item(
            inbox_task_id(1),
            "failed",
            TaskLifecycle::Open,
            TaskConnectivity::Connected,
            TaskAttention::Failed,
            TaskActivity::Idle,
            ReviewReadiness::NotReady,
            2,
        ),
    ]);
    let mut inbox = Inbox::from_model(&model);

    assert_eq!(
        inbox.task_list().task_ids(),
        &[inbox_task_id(1), inbox_task_id(0)]
    );
    inbox
        .task_list_mut()
        .set_viewport(1, 1)
        .expect("legacy viewport accessor must remain usable");
    assert_eq!(inbox.virtual_window(), inbox.task_list().virtual_window());
    assert_eq!(inbox.visible_rows()[0].task_id, inbox_task_id(0));
}

#[test]
fn inbox_search_includes_archived_only_when_explicit_and_reports_empty_states() {
    let model = inbox_model(vec![
        inbox_task_item(
            inbox_task_id(20),
            "keep me",
            TaskLifecycle::Open,
            TaskConnectivity::Connected,
            TaskAttention::None,
            TaskActivity::Idle,
            ReviewReadiness::NotReady,
            1,
        ),
        inbox_task_item(
            inbox_task_id(21),
            "archived target",
            TaskLifecycle::Archived,
            TaskConnectivity::Connected,
            TaskAttention::None,
            TaskActivity::Idle,
            ReviewReadiness::NotReady,
            2,
        ),
    ]);
    let normal = Inbox::from_model(&model);
    assert_eq!(normal.state(), InboxState::Ready);
    assert_eq!(normal.len(), 1);

    let filtered = Inbox::from_model_with_filter(
        &model,
        &InboxFilter::new("missing"),
        &UnreadCursor::default(),
    );
    assert_eq!(filtered.state(), InboxState::FilteredEmpty);

    let archived = Inbox::from_model_with_filter(
        &model,
        &InboxFilter::new("archived").including_archived(),
        &UnreadCursor::default(),
    );
    assert_eq!(archived.len(), 1);
    assert_eq!(
        archived.row(inbox_task_id(21)).unwrap().title,
        "archived target"
    );
    assert_eq!(
        Inbox::from_model(&inbox_model(Vec::new())).state(),
        InboxState::Empty
    );
    let projected_error = Inbox::from_projection(
        Err(InboxError::ProjectionUnavailable),
        &InboxFilter::new("keep"),
        &UnreadCursor::default(),
    );
    assert_eq!(
        projected_error.state(),
        InboxState::Error(InboxError::ProjectionUnavailable)
    );
    assert_eq!(projected_error.filter().query(), "keep");

    assert_eq!(
        Inbox::from_error(InboxError::ProjectionUnavailable).state(),
        InboxState::Error(InboxError::ProjectionUnavailable)
    );
}

#[test]
fn inbox_5000_task_fixture_is_bounded_and_virtualized() {
    let fixture: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/ui/task-inbox.json"))
            .expect("task inbox fixture");
    assert_eq!(fixture["expected_count"], 5_000);
    assert_eq!(fixture["expected_overscan"], 32);
    assert_eq!(fixture["report"]["projection"], "client-model-only");
    assert_eq!(fixture["report"]["rendered_rows_bound"], 104);
    assert_eq!(fixture["report"]["selection"], "task-id");

    let items = (0..5_000)
        .map(|index| {
            inbox_task_item(
                inbox_task_id(index),
                &format!("Task {index}"),
                TaskLifecycle::Open,
                TaskConnectivity::Connected,
                TaskAttention::None,
                TaskActivity::Idle,
                ReviewReadiness::NotReady,
                index as i64,
            )
        })
        .collect();
    let mut inbox = Inbox::from_model(&inbox_model(items));
    assert_eq!(inbox.len(), 5_000);
    assert_eq!(inbox.virtual_window().overscan(), 32);
    assert_eq!(
        inbox.virtual_window().visible_range(),
        0..DEFAULT_VISIBLE_ROWS
    );
    assert_eq!(
        inbox.rendered_rows().len(),
        DEFAULT_VISIBLE_ROWS + inbox.virtual_window().overscan()
    );
    inbox
        .set_viewport(2_500, DEFAULT_VISIBLE_ROWS)
        .expect("valid local viewport");
    assert_eq!(
        inbox.virtual_window().visible_range(),
        2_500..2_500 + DEFAULT_VISIBLE_ROWS
    );
    assert_eq!(
        inbox.virtual_window().render_range(inbox.len()),
        2_500 - 32..2_500 + DEFAULT_VISIBLE_ROWS + 32
    );
    assert_eq!(
        inbox.rendered_rows().len(),
        fixture["report"]["rendered_rows_bound"]
            .as_u64()
            .expect("rendered row bound") as usize
    );
}

#[test]
fn inbox_overflow_retains_attention_order_before_capping() {
    let mut items = (0..5_000)
        .map(|index| {
            inbox_task_item(
                inbox_task_id(index),
                &format!("Recent {index}"),
                TaskLifecycle::Open,
                TaskConnectivity::Connected,
                TaskAttention::None,
                TaskActivity::Idle,
                ReviewReadiness::NotReady,
                index as i64,
            )
        })
        .collect::<Vec<_>>();
    items.push(inbox_task_item(
        inbox_task_id(5_000),
        "Needs attention",
        TaskLifecycle::Open,
        TaskConnectivity::Connected,
        TaskAttention::Failed,
        TaskActivity::Idle,
        ReviewReadiness::NotReady,
        5_000,
    ));

    let inbox = Inbox::from_model(&inbox_model(items));

    assert_eq!(inbox.len(), 5_000);
    assert_eq!(
        inbox.overflow(),
        Some(devmanager::ui::task_cockpit::TaskListOverflow {
            limit: 5_000,
            total_count: 5_001,
            retained_count: 5_000,
        })
    );
    assert!(
        inbox.row(inbox_task_id(5_000)).is_some(),
        "high-attention rows must be retained before the finite cap"
    );
    assert!(
        inbox.row(inbox_task_id(0)).is_none(),
        "the lowest-priority row should be the overflow victim"
    );
}

#[test]
fn inbox_projection_does_not_probe_runtime_or_process_state() {
    let source = include_str!("../src/ui/task_cockpit/inbox.rs");
    for forbidden in [
        "std::process",
        "process_monitor",
        "host_client",
        "reqwest",
        "network",
    ] {
        assert!(
            !source.contains(forbidden),
            "inbox must remain a pure projection: {forbidden}"
        );
    }
}
