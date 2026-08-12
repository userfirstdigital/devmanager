use devmanager::client::model::{MAX_CLIENT_SEARCH_CHARS, MAX_CLIENT_SEARCH_POSTING_BYTES};
use devmanager::client::{
    ClientModel, ClientModelBuilder, InboxHostController, InboxPreferenceStore,
};
use devmanager::domain::agent::{
    AgentRole, AgentSessionFacts, AgentSessionLifecycle, ProviderSessionId,
};
use devmanager::domain::event::{DomainEvent, Event};
use devmanager::domain::id::{
    AgentSessionId, EnvironmentId, EventId, ProjectId, ResourceId, SnapshotId, TaskId,
};
use devmanager::domain::resource::{
    OwnerKind, ResourceFacts, ResourceKind, ResourceLifecycle, ResourceRecipe,
};
use devmanager::domain::snapshot::{SnapshotItem, SnapshotPage, SnapshotSection, TaskSnapshotItem};
use devmanager::domain::task::{
    ReviewReadiness, TaskActivity, TaskAssignment, TaskAttention, TaskConnectivity, TaskFacts,
    TaskLifecycle, WorkspaceRef,
};
use devmanager::providers::ProviderKind;
use devmanager::ui::components::AccessibleRole;
use devmanager::ui::preview::{
    parse_preview_args, PreviewApplication, PreviewDismiss, PreviewError, PreviewOutputCapability,
    PreviewPathPolicy, PreviewRequest, PREVIEW_SCHEMA,
};
use devmanager::ui::task_cockpit::{
    Inbox, InboxError, InboxFilter, InboxItemKey, InboxPresentationWidth, InboxRenderItem,
    InboxRuntime, InboxSection, InboxState, PrimaryProviderIcon, PrimaryProviderState,
    RuntimeSummary, UnreadCursor, DEFAULT_VISIBLE_ROWS,
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

fn task_cockpit_fixture() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/ui/task-cockpit.json")
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
fn task_cockpit_headless_preview_instantiates_the_actual_native_shell() {
    let _lock = HEADLESS_INIT_TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let policy = repository_policy();
    let request = PreviewRequest::validate(
        task_cockpit_fixture(),
        repository_output("task-cockpit-native-shell-headless.png"),
        &policy,
    )
    .expect("checked-in task cockpit fixture should be accepted");
    let preview = PreviewApplication::load(request, &policy).expect("fixture should load");

    let report = preview
        .initialize_headless()
        .expect("task cockpit headless initialization should construct the shell");
    assert!(report.root_constructed);
    assert!(report.native_shell_instantiated);
    assert!(!report.production_host_started);
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
    inbox_task_item_with_workspace(
        id,
        title,
        lifecycle,
        connectivity,
        attention,
        activity,
        review_readiness,
        created_at_ms,
        WorkspaceRef::Main,
    )
}

fn inbox_task_item_with_workspace(
    id: TaskId,
    title: &str,
    lifecycle: TaskLifecycle,
    connectivity: TaskConnectivity,
    attention: TaskAttention,
    activity: TaskActivity,
    review_readiness: ReviewReadiness,
    created_at_ms: i64,
    workspace: WorkspaceRef,
) -> SnapshotItem {
    SnapshotItem::Task(TaskSnapshotItem {
        task: TaskFacts {
            id,
            environment_id: EnvironmentId::from_bytes(fixed_uuid_v7(0x10)).expect("environment"),
            title: title.into(),
            description: None,
            project_id: ProjectId::from_bytes(fixed_uuid_v7(0x11)).expect("project"),
            workspace,
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
    inbox_model_with_related(items, Vec::new(), Vec::new())
}

fn inbox_model_with_related(
    task_items: Vec<SnapshotItem>,
    agent_items: Vec<SnapshotItem>,
    resource_items: Vec<SnapshotItem>,
) -> ClientModel {
    let snapshot_id = SnapshotId::from_bytes(fixed_uuid_v7(0x20)).expect("snapshot");
    let mut builder = ClientModelBuilder::new();
    builder
        .ingest_page(SnapshotPage {
            snapshot_id,
            through_sequence: 1,
            section: SnapshotSection::Tasks,
            after_item: None,
            items: task_items,
            encoded_bytes: 1,
            next_cursor: None,
        })
        .expect("task page");
    builder
        .ingest_page(SnapshotPage {
            snapshot_id,
            through_sequence: 1,
            section: SnapshotSection::AgentSessions,
            after_item: None,
            items: agent_items,
            encoded_bytes: 1,
            next_cursor: None,
        })
        .expect("agent page");
    for (section, items) in [
        (SnapshotSection::Artifacts, Vec::new()),
        (SnapshotSection::Resources, resource_items),
        (SnapshotSection::Operations, Vec::new()),
    ] {
        builder
            .ingest_page(SnapshotPage {
                snapshot_id,
                through_sequence: 1,
                section,
                after_item: None,
                items,
                encoded_bytes: 1,
                next_cursor: None,
            })
            .expect("empty related section");
    }
    builder.finish().expect("complete client model")
}

fn inbox_agent_item(
    task_id: TaskId,
    agent_id: AgentSessionId,
    provider_kind: ProviderKind,
) -> SnapshotItem {
    SnapshotItem::AgentSession(AgentSessionFacts {
        id: agent_id,
        task_id,
        role: AgentRole::Primary,
        provider_kind,
        provider_session_id: Some(
            ProviderSessionId::new(format!("session-{agent_id}")).expect("provider session"),
        ),
        lifecycle: AgentSessionLifecycle::Open,
        runtime_generation: 1,
        revision: 1,
    })
}

fn inbox_resource_item(
    task_id: TaskId,
    resource_id: ResourceId,
    kind: ResourceKind,
) -> SnapshotItem {
    let (recipe, lifecycle) = match kind {
        ResourceKind::Terminal => (
            ResourceRecipe::Terminal { cols: 80, rows: 24 },
            ResourceLifecycle::Active,
        ),
        ResourceKind::BrowserContext => (
            ResourceRecipe::Browser {
                start_url: "https://example.test".into(),
            },
            ResourceLifecycle::Active,
        ),
        ResourceKind::Service => (
            ResourceRecipe::Service {
                command: "npm run dev".into(),
            },
            ResourceLifecycle::Releasing,
        ),
    };
    SnapshotItem::Resource(ResourceFacts {
        id: resource_id,
        task_id: Some(task_id),
        owner_kind: OwnerKind::Task,
        resource_kind: kind,
        recipe,
        lifecycle,
        runtime_generation: 1,
        updated_at_ms: 1,
    })
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
    let mut unread = UnreadCursor::default();
    assert!(unread.observe_event(inbox_task_id(0), 1));
    assert!(unread.observe_event(inbox_task_id(0), 2));
    assert!(unread.observe_event(inbox_task_id(0), 3));
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
fn inbox_exposes_active_identity_and_viewport_accessors_synchronized() {
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
        inbox.task_ids().collect::<Vec<_>>(),
        vec![inbox_task_id(1), inbox_task_id(0)]
    );
    inbox
        .set_active_viewport(1, 1)
        .expect("active viewport must remain usable");
    assert_eq!(inbox.active_virtual_window(), inbox.virtual_window());
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
    assert_eq!(archived.len(), 0);
    assert_eq!(archived.history_rows().len(), 1);
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
    let render = inbox.render_model(InboxPresentationWidth::Narrow);
    let rendered_row_count = render
        .items
        .iter()
        .filter(|item| matches!(item, InboxRenderItem::Row(_)))
        .count();
    assert_eq!(
        rendered_row_count,
        fixture["report"]["rendered_rows_bound"]
            .as_u64()
            .expect("rendered row bound") as usize,
        "the shell-facing render model must preserve the virtualized row bound"
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
        Some(devmanager::ui::task_cockpit::InboxOverflow {
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

#[test]
fn inbox_fixture_drives_bounded_display_data_and_compact_render_items() {
    let fixture: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/ui/task-inbox.json"))
            .expect("task inbox fixture");
    let display = &fixture["behavioral_cases"]["display_rows"];
    let rich = &display[0];
    let missing = &display[1];
    let rich_id = inbox_task_id(30);
    let missing_id = inbox_task_id(31);
    let rich_agent = AgentSessionId::from_bytes(fixed_uuid_v7(0x30)).expect("rich agent");
    let rich_workspace = WorkspaceRef::worktree(
        rich["workspace_path"].as_str().expect("workspace path"),
        rich["worktree_branch"].as_str().expect("worktree branch"),
    )
    .expect("fixture workspace");
    let rich_task = {
        let SnapshotItem::Task(mut task) = inbox_task_item_with_workspace(
            rich_id,
            rich["title"].as_str().expect("rich title"),
            TaskLifecycle::Open,
            TaskConnectivity::Connected,
            TaskAttention::NeedsAnswer,
            TaskActivity::Idle,
            ReviewReadiness::NotReady,
            30,
            rich_workspace,
        ) else {
            unreachable!("inbox task helper returns a task")
        };
        task.primary_agent_id = Some(rich_agent);
        SnapshotItem::Task(task)
    };
    let model = inbox_model_with_related(
        vec![
            rich_task,
            inbox_task_item(
                missing_id,
                missing["title"].as_str().expect("missing title"),
                TaskLifecycle::Open,
                TaskConnectivity::Connected,
                TaskAttention::None,
                TaskActivity::Working,
                ReviewReadiness::NotReady,
                31,
            ),
        ],
        vec![inbox_agent_item(
            rich_id,
            rich_agent,
            ProviderKind::parse_wire(rich["provider"].as_str().expect("provider"))
                .expect("canonical provider"),
        )],
        vec![
            inbox_resource_item(
                rich_id,
                ResourceId::from_bytes(fixed_uuid_v7(0x31)).expect("terminal resource"),
                ResourceKind::Terminal,
            ),
            inbox_resource_item(
                rich_id,
                ResourceId::from_bytes(fixed_uuid_v7(0x32)).expect("service resource"),
                ResourceKind::Service,
            ),
        ],
    );

    let inbox = Inbox::from_model(&model);
    let rich_row = inbox.row(rich_id).expect("rich row");
    assert_eq!(
        rich_row.display.primary_provider,
        PrimaryProviderState::Present {
            icon: PrimaryProviderIcon::Claude,
            kind: "Claude".into()
        }
    );
    assert_eq!(
        rich_row.display.runtime,
        RuntimeSummary::Present {
            lifecycle: AgentSessionLifecycle::Open,
            generation: 1,
        }
    );
    assert_eq!(
        rich_row.display.worktree,
        "feature·task-inbox-with-a-deliberately-long-branch-name"
    );
    assert!(!rich_row
        .display
        .worktree
        .contains(rich["workspace_path"].as_str().expect("workspace path")));
    assert_eq!(rich_row.display.resources.terminal_count, 1);
    assert_eq!(rich_row.display.resources.service_count, 1);
    assert_eq!(rich_row.display.resources.releasing_count, 1);
    assert_eq!(
        rich["resources"].as_array().map(|values| values.len()),
        Some(2)
    );
    assert_eq!(rich["resources"][0]["kind"], "terminal");
    assert_eq!(rich["resources"][1]["lifecycle"], "releasing");
    assert!(rich_row.display.project.chars().count() <= 96);
    assert!(rich_row.display.worktree.chars().count() <= 128);

    let missing_row = inbox.row(missing_id).expect("missing provider row");
    assert_eq!(
        missing_row.display.primary_provider,
        PrimaryProviderState::Missing
    );
    assert_eq!(missing_row.display.runtime, RuntimeSummary::Missing);

    let render = inbox.render_model(InboxPresentationWidth::Narrow);
    assert!(render.items.iter().any(|item| matches!(
        item,
        InboxRenderItem::SectionHeader {
            key: InboxItemKey::Section(InboxSection::NeedsMe),
            section: InboxSection::NeedsMe,
            ..
        }
    )));
    assert!(render.items.iter().any(|item| matches!(
        item,
        InboxRenderItem::Row(row)
            if row.key == InboxItemKey::Row(rich_id)
                && row.task_id == rich_id
                && !row.accessible_name.is_empty()
                && !row.accessible_description.is_empty()
                && row.accessibility.role == AccessibleRole::Button
                && !row.accessibility.disabled
                && row.accessibility.value.as_deref() == Some("Needs answer")
                && row.accessible_description.contains("Workspace path hidden")
                && !row.accessible_description.contains(
                    rich["workspace_path"].as_str().expect("workspace path")
                )
                && !row.secondary_text.contains(
                    rich["workspace_path"].as_str().expect("workspace path")
                )
    )));
}

#[test]
fn inbox_accessibility_announces_unread_and_bounded_information() {
    let task_id = inbox_task_id(35);
    let title = "A".repeat(devmanager::ui::task_cockpit::MAX_ACCESSIBLE_NAME_CHARS + 16);
    let model = inbox_model(vec![inbox_task_item(
        task_id,
        &title,
        TaskLifecycle::Open,
        TaskConnectivity::Connected,
        TaskAttention::None,
        TaskActivity::Idle,
        ReviewReadiness::NotReady,
        35,
    )]);
    let mut unread = UnreadCursor::default();
    assert!(unread.observe_event(task_id, 41));
    assert!(unread.observe_event(task_id, 42));
    let render =
        Inbox::from_model_with_unread(&model, &unread).render_model(InboxPresentationWidth::Narrow);
    let row = render
        .items
        .iter()
        .find_map(|item| match item {
            InboxRenderItem::Row(row) if row.task_id == task_id => Some(row),
            _ => None,
        })
        .expect("bounded task row");
    assert_eq!(row.accessibility.name, row.accessible_name);
    assert_eq!(row.accessibility.description, row.accessible_description);
    assert!(row.accessible_description.contains("2 unread events"));
    assert!(row
        .accessible_description
        .contains("Some row details truncated"));
    assert!(row.title.chars().count() <= devmanager::ui::task_cockpit::MAX_ACCESSIBLE_NAME_CHARS);
}

#[test]
fn inbox_redacts_control_path_and_provider_session_data_at_projection_ingress() {
    let task_id = inbox_task_id(36);
    let agent_id = AgentSessionId::from_bytes(fixed_uuid_v7(0x36)).expect("agent");
    let SnapshotItem::Task(mut task) = inbox_task_item_with_workspace(
        task_id,
        "token\u{0000}/title\u{202e}",
        TaskLifecycle::Open,
        TaskConnectivity::Connected,
        TaskAttention::None,
        TaskActivity::Idle,
        ReviewReadiness::NotReady,
        36,
        WorkspaceRef::worktree(r"C:\Users\micro\secret-workspace", "feature/secret-branch")
            .expect("workspace"),
    ) else {
        unreachable!("task fixture must be a task")
    };
    task.primary_agent_id = Some(agent_id);
    let model = inbox_model_with_related(
        vec![SnapshotItem::Task(task)],
        vec![inbox_agent_item(task_id, agent_id, ProviderKind::Codex)],
        Vec::new(),
    );
    let inbox = Inbox::from_model(&model);
    let row = inbox.row(task_id).expect("redacted row");
    assert!(!row.title.chars().any(char::is_control));
    assert!(!row.title.contains('/'));
    assert!(!row.title.contains('\u{202e}'));
    assert_eq!(row.display.worktree, "feature·secret-branch");
    assert_eq!(
        row.display.primary_provider,
        PrimaryProviderState::Present {
            icon: PrimaryProviderIcon::Codex,
            kind: "Codex".into(),
        }
    );
    let render = inbox.render_model(InboxPresentationWidth::Regular);
    let row = render
        .items
        .iter()
        .find_map(|item| match item {
            InboxRenderItem::Row(row) if row.task_id == task_id => Some(row),
            _ => None,
        })
        .expect("rendered redacted row");
    assert!(!row.accessible_description.contains("account-secret"));
    assert!(!row.accessible_description.contains("secret-workspace"));
    assert!(!row.accessible_description.chars().any(char::is_control));
}

#[test]
fn inbox_render_model_has_narrow_empty_filtered_and_error_states() {
    let empty =
        Inbox::from_model(&inbox_model(Vec::new())).render_model(InboxPresentationWidth::Narrow);
    assert!(matches!(
        empty.items.as_slice(),
        [InboxRenderItem::State {
            key: InboxItemKey::Empty,
            ..
        }]
    ));

    let model = inbox_model(vec![inbox_task_item(
        inbox_task_id(40),
        "visible task",
        TaskLifecycle::Open,
        TaskConnectivity::Connected,
        TaskAttention::None,
        TaskActivity::Idle,
        ReviewReadiness::NotReady,
        40,
    )]);
    let filtered = Inbox::from_model_with_filter(
        &model,
        &InboxFilter::new("not present"),
        &UnreadCursor::default(),
    )
    .render_model(InboxPresentationWidth::Narrow);
    assert!(matches!(
        filtered.items.as_slice(),
        [InboxRenderItem::State {
            key: InboxItemKey::FilteredEmpty,
            ..
        }]
    ));

    let error = Inbox::from_error(InboxError::ProjectionUnavailable)
        .render_model(InboxPresentationWidth::Regular);
    assert!(matches!(
        error.items.as_slice(),
        [InboxRenderItem::State {
            key: InboxItemKey::Error,
            ..
        }]
    ));
}

#[test]
fn unread_cursor_is_bounded_semantic_state_and_reconnect_idempotent() {
    let task = inbox_task_id(50);
    let mut cursor = UnreadCursor::default();
    assert!(cursor.observe_event(task, 7));
    assert!(!cursor.observe_event(task, 7), "replay must be idempotent");
    assert!(cursor.observe_event(task, 8));
    assert_eq!(cursor.unread_count(task), 2);
    cursor.mark_read(task);
    assert_eq!(cursor.unread_count(task), 0);
    assert!(
        !cursor.observe_event(task, 8),
        "reconnect replay must stay read"
    );
    assert!(cursor.observe_event(task, 9));
    assert_eq!(cursor.unread_count(task), 1);

    let model = inbox_model(vec![inbox_task_item(
        task,
        "Retained task",
        TaskLifecycle::Open,
        TaskConnectivity::Connected,
        TaskAttention::None,
        TaskActivity::Idle,
        ReviewReadiness::NotReady,
        50,
    )]);
    cursor.prune(&model);
    assert_eq!(cursor.len(), 1);
    cursor.prune(&inbox_model(Vec::new()));
    assert!(cursor.is_empty());
}

#[test]
fn durable_unread_cursor_roundtrips_and_ignores_duplicate_out_of_order_and_foreign_events() {
    let task = inbox_task_id(51);
    let event = |tail: u8, task_id: Option<TaskId>, sequence: u64| DomainEvent {
        id: EventId::from_bytes(fixed_uuid_v7(tail)).expect("event id"),
        task_id,
        sequence,
        task_revision: None,
        occurred_at_ms: sequence as i64,
        payload: Event::TaskReopened,
    };
    let first = event(0x51, Some(task), 7);
    let mut cursor = UnreadCursor::default();
    assert!(cursor.observe_durable_event(&first));
    assert!(
        !cursor.observe_durable_event(&first),
        "duplicate id is idempotent"
    );
    assert!(!cursor.observe_durable_event(&event(0x52, Some(task), 6)));
    assert_eq!(cursor.last_seen_sequence(), 7);
    assert_eq!(cursor.unread_count(task), 1);
    assert!(cursor.observe_durable_event(&event(0x53, None, 8)));
    assert_eq!(cursor.last_seen_sequence(), 8);
    assert_eq!(cursor.unread_count(task), 1);

    let restored =
        UnreadCursor::decode_durable(&cursor.encode_durable().expect("durable cursor encoding"))
            .expect("durable cursor decoding");
    assert_eq!(restored, cursor, "restart must preserve the event cursor");
    assert!(UnreadCursor::decode_durable(b"not-a-cursor").is_err());
}

#[test]
fn native_next_cursor_store_restores_into_runtime_without_legacy_session_state() {
    let task = inbox_task_id(53);
    let event = DomainEvent {
        id: EventId::from_bytes(fixed_uuid_v7(0x54)).expect("event id"),
        task_id: Some(task),
        sequence: 12,
        task_revision: None,
        occurred_at_ms: 12,
        payload: Event::TaskReopened,
    };
    let mut cursor = UnreadCursor::default();
    assert!(cursor.observe_durable_event(&event));
    let encoded = cursor.encode_durable().expect("cursor encoding");

    let profile = tempdir().expect("isolated native-next profile");
    let store = InboxPreferenceStore::at_profile_root(profile.path());
    let controller = InboxHostController::new(store);
    let mut runtime = InboxRuntime::new();
    runtime.restore_unread_cursor(cursor.clone());
    runtime
        .persist_unread_cursor_to_controller(&controller)
        .expect("atomic cursor save");
    let mut restored_runtime = InboxRuntime::new();
    restored_runtime
        .restore_unread_cursor_from_controller(&controller)
        .expect("versioned cursor restore");
    assert_eq!(restored_runtime.unread_cursor(), &cursor);
    assert_eq!(
        runtime.encode_unread_cursor().expect("cursor encoding"),
        encoded
    );
    assert_eq!(
        controller
            .preferences()
            .path()
            .file_name()
            .and_then(|name| name.to_str()),
        Some("inbox-preferences.json")
    );
    assert!(!controller.preferences().path().ends_with("session.json"));
}

#[test]
fn client_projection_index_is_reused_for_100k_models_without_wall_clock_assertions() {
    let items = (0..100_000)
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
    let model = inbox_model(items);
    assert_eq!(model.task_projection_index_len(), 100_000);
    assert_eq!(model.task_projection_index_rebuilds(), 1);
    assert!(
        model.task_projection_index_search_resident_bytes() <= MAX_CLIENT_SEARCH_POSTING_BYTES,
        "compact search resident allocations must stay within the byte budget"
    );

    let first = Inbox::from_model(&model);
    let second = Inbox::from_model(&model);
    assert_eq!(first, second);
    assert_eq!(first.len(), 5_000);
    assert_eq!(first.task_ids().count(), 5_000);
    assert_eq!(model.task_projection_index_rebuilds(), 1);
}

#[test]
fn inbox_applies_one_100k_model_event_incrementally_and_keeps_indexed_search_truthful() {
    let items = (0..100_000)
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
    let mut model = inbox_model(items);
    let mut inbox = Inbox::from_model(&model);
    let target = inbox_task_id(99_999);
    let event = DomainEvent {
        id: EventId::from_bytes(fixed_uuid_v7(0x67)).expect("event id"),
        task_id: Some(target),
        sequence: 2,
        task_revision: Some(100_000),
        occurred_at_ms: 100_001,
        payload: Event::TaskRenamed {
            title: "Renamed 99999".to_string(),
        },
    };
    model.apply_event(&event).expect("rename event applies");
    let full_rebuilds = inbox.full_rebuilds();
    inbox.apply_model_event(&model, Some(target));

    assert_eq!(inbox.full_rebuilds(), full_rebuilds);
    assert_eq!(inbox.incremental_updates(), 1);
    assert_eq!(
        inbox.row(target).expect("target retained").title,
        "Renamed 99999"
    );
    let (matches, count) = model.search_task_ids("renamed 99999", false);
    assert_eq!(count, 1);
    assert_eq!(matches, vec![target]);
}

#[test]
fn indexed_search_keeps_common_and_adversarial_100k_queries_bounded_and_ordered() {
    let items = (0..100_000)
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
    let model = inbox_model(items);

    let queries = vec![
        "t".to_string(),
        "task".to_string(),
        "task ".to_string(),
        "Task 1234".to_string(),
        "task ".repeat(32),
        "é".repeat(160),
        "t".repeat(160),
    ];
    for query in queries {
        let (ids, total, work) = model.search_task_ids_with_work(&query, false);
        if query == "t" || query == "task" || query == "task " {
            assert_eq!(total, 100_000, "truthful total for {query:?}");
            assert_eq!(ids.first().copied(), Some(inbox_task_id(99_999)));
        } else if query == "Task 1234" {
            assert_eq!(total, 11, "truthful specific-prefix total");
            assert_eq!(ids.first().copied(), Some(inbox_task_id(12_349)));
        } else {
            assert_eq!(total, 0, "adversarial query must not fabricate matches");
        }
        assert!(ids.len() <= 5_000);
        assert!(work <= 5_000, "hot-path work must be capped for {query:?}");
    }
}

#[test]
fn indexed_search_is_exact_for_interior_substrings() {
    let items = vec![
        inbox_task_item(
            inbox_task_id(0),
            "Task Alpha",
            TaskLifecycle::Open,
            TaskConnectivity::Connected,
            TaskAttention::None,
            TaskActivity::Idle,
            ReviewReadiness::NotReady,
            0,
        ),
        inbox_task_item(
            inbox_task_id(1),
            "X task",
            TaskLifecycle::Open,
            TaskConnectivity::Connected,
            TaskAttention::None,
            TaskActivity::Idle,
            ReviewReadiness::NotReady,
            1,
        ),
    ];
    let model = inbox_model(items);

    let page = model.search_task_ids_page("task", false, None);

    assert_eq!(page.exact_total, Some(2));
    assert_eq!(page.ids.len(), 2);
    assert!(page.ids.contains(&inbox_task_id(0)));
    assert!(page.ids.contains(&inbox_task_id(1)));
}

#[test]
fn indexed_search_short_scalar_queries_include_interior_matches() {
    let items = vec![
        inbox_task_item(
            inbox_task_id(0),
            "Task Alpha",
            TaskLifecycle::Open,
            TaskConnectivity::Connected,
            TaskAttention::None,
            TaskActivity::Idle,
            ReviewReadiness::NotReady,
            0,
        ),
        inbox_task_item(
            inbox_task_id(1),
            "X task",
            TaskLifecycle::Open,
            TaskConnectivity::Connected,
            TaskAttention::None,
            TaskActivity::Idle,
            ReviewReadiness::NotReady,
            1,
        ),
    ];
    let model = inbox_model(items);

    let page = model.search_task_ids_page("t", false, None);

    assert_eq!(page.exact_total, Some(2));
    assert_eq!(page.ids.len(), 2);
    assert!(page.ids.contains(&inbox_task_id(0)));
    assert!(page.ids.contains(&inbox_task_id(1)));
}

#[test]
fn indexed_search_does_not_treat_a_prefix_posting_as_exhaustive_past_the_source_fence() {
    let items = vec![
        inbox_task_item(
            inbox_task_id(0),
            "task",
            TaskLifecycle::Open,
            TaskConnectivity::Connected,
            TaskAttention::None,
            TaskActivity::Idle,
            ReviewReadiness::NotReady,
            0,
        ),
        inbox_task_item(
            inbox_task_id(1),
            &format!("{}task", "x".repeat(40)),
            TaskLifecycle::Open,
            TaskConnectivity::Connected,
            TaskAttention::None,
            TaskActivity::Idle,
            ReviewReadiness::NotReady,
            1,
        ),
    ];
    let model = inbox_model(items);

    let page = model.search_task_ids_page("task", false, None);

    assert_eq!(page.exact_total, Some(2));
    assert_eq!(page.ids.len(), 2);
    assert!(page.ids.contains(&inbox_task_id(0)));
    assert!(page.ids.contains(&inbox_task_id(1)));
}

#[test]
fn indexed_search_keeps_truthful_matches_after_the_query_bound() {
    let target = inbox_task_id(0);
    let title = format!(
        "{} StraßeNeedle",
        "界".repeat(MAX_CLIENT_SEARCH_CHARS.saturating_add(8))
    );
    let model = inbox_model(vec![inbox_task_item(
        target,
        &title,
        TaskLifecycle::Open,
        TaskConnectivity::Connected,
        TaskAttention::None,
        TaskActivity::Idle,
        ReviewReadiness::NotReady,
        0,
    )]);

    let page = model.search_task_ids_page("strasseneedle", false, None);
    assert_eq!(page.exact_total, Some(1));
    assert_eq!(page.ids, vec![target]);
    assert!(page.work <= 5_000);

    let inbox = Inbox::from_model_with_filter(
        &model,
        &InboxFilter::new("strasseneedle"),
        &Default::default(),
    );
    assert_eq!(
        inbox
            .active_rows()
            .iter()
            .map(|row| row.task_id)
            .collect::<Vec<_>>(),
        vec![target]
    );
}

#[test]
fn indexed_search_uses_full_unicode_casefold_for_expanding_mappings() {
    let model = inbox_model(vec![inbox_task_item(
        inbox_task_id(0),
        "Straße",
        TaskLifecycle::Open,
        TaskConnectivity::Connected,
        TaskAttention::None,
        TaskActivity::Idle,
        ReviewReadiness::NotReady,
        0,
    )]);

    let page = model.search_task_ids_page("STRASSE", false, None);

    assert_eq!(page.exact_total, Some(1));
    assert_eq!(page.ids, vec![inbox_task_id(0)]);
}

#[test]
fn indexed_search_repeated_titles_never_scan_a_full_long_query_posting() {
    let items = (0..100_000)
        .map(|index| {
            inbox_task_item(
                inbox_task_id(index),
                "aaaaaaaaa",
                TaskLifecycle::Open,
                TaskConnectivity::Connected,
                TaskAttention::None,
                TaskActivity::Idle,
                ReviewReadiness::NotReady,
                index as i64,
            )
        })
        .collect();
    let model = inbox_model(items);

    let (ids, known_total, work) = model.search_task_ids_with_work("aaaaaaaaa", false);

    assert_eq!(ids.len(), 5_000);
    assert!(
        known_total <= 5_000,
        "a bounded page cannot claim an exact total"
    );
    assert!(
        work <= 5_000,
        "long repeated-title query scanned the full posting"
    );
}

#[test]
fn inbox_partial_search_exposes_5000_plus_overflow_until_exact_continuation() {
    let items = (0..100_000)
        .map(|index| {
            inbox_task_item(
                inbox_task_id(index),
                "aaaaaaaaa",
                TaskLifecycle::Open,
                TaskConnectivity::Connected,
                TaskAttention::None,
                TaskActivity::Idle,
                ReviewReadiness::NotReady,
                index as i64,
            )
        })
        .collect();
    let model = inbox_model(items);
    let inbox =
        Inbox::from_model_with_filter(&model, &InboxFilter::new("aaaaaaaaa"), &Default::default());
    let overflow = inbox
        .overflow()
        .expect("partial page must be marked over limit");
    assert_eq!(overflow.limit, 5_000);
    assert_eq!(overflow.retained_count, 5_000);
    assert!(overflow.total_count > 5_000, "partial total must be 5000+");
}

#[test]
fn indexed_search_normalizes_expanding_unicode_before_the_shared_bound() {
    let title = "İ".repeat(160);
    let model = inbox_model(vec![inbox_task_item(
        inbox_task_id(0),
        &title,
        TaskLifecycle::Open,
        TaskConnectivity::Connected,
        TaskAttention::None,
        TaskActivity::Idle,
        ReviewReadiness::NotReady,
        0,
    )]);

    let (ids, total, work) = model.search_task_ids_with_work(&title, false);

    assert_eq!(ids, vec![inbox_task_id(0)]);
    assert_eq!(total, 1, "expanded Unicode title must remain searchable");
    assert!(work <= 5_000);
}

#[test]
fn indexed_search_continuations_are_bounded_and_fenced_to_the_current_query() {
    let items = (0..100_000)
        .map(|index| {
            inbox_task_item(
                inbox_task_id(index),
                "aaaaaaaaa",
                TaskLifecycle::Open,
                TaskConnectivity::Connected,
                TaskAttention::None,
                TaskActivity::Idle,
                ReviewReadiness::NotReady,
                index as i64,
            )
        })
        .collect();
    let model = inbox_model(items);

    let mut page = model.search_task_ids_page("aaaaaaaaa", false, None);
    assert_eq!(
        page.status,
        devmanager::client::model::SearchPageStatus::Partial
    );
    assert_eq!(page.known_total, 5_000);
    assert_eq!(page.exact_total, None);
    assert_eq!(page.work, 5_000);
    assert!(page.continuation().is_some());

    let stale = model.search_task_ids_page("bbbbbbbbb", false, page.continuation());
    assert!(
        stale.is_stale(),
        "a prior query must never publish into a new query"
    );

    let mut pages = 1;
    while let Some(continuation) = page.continuation().cloned() {
        page = model.search_task_ids_page("aaaaaaaaa", false, Some(&continuation));
        assert!(page.work <= 5_000);
        pages += 1;
        assert!(pages <= 21, "continuation must make bounded progress");
    }
    assert!(page.is_complete());
    assert_eq!(page.exact_total, Some(100_000));
    assert_eq!(page.ids.len(), 5_000);
}

#[test]
fn indexed_search_empty_page_reports_overflow_and_can_reach_exact_total() {
    let items = (0..100_000)
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
    let model = inbox_model(items);

    let mut page = model.search_task_ids_page("", false, None);
    assert!(page.is_partial(), "5000+ results must be visibly partial");
    assert_eq!(page.exact_total, Some(100_000));
    assert!(page.continuation().is_some());

    let mut pages = 1;
    while let Some(continuation) = page.continuation().cloned() {
        page = model.search_task_ids_page("", false, Some(&continuation));
        pages += 1;
        assert!(page.work <= 5_000);
        assert!(pages <= 21, "empty search must make bounded progress");
    }
    assert!(page.is_complete());
    assert_eq!(page.exact_total, Some(100_000));
}

#[test]
fn search_index_resident_estimate_accounts_for_keys_nodes_and_capacities() {
    let items = (0..100_000)
        .map(|index| {
            let title = format!("{index:08x}-{}", "x".repeat(152));
            inbox_task_item(
                inbox_task_id(index),
                &title,
                TaskLifecycle::Open,
                TaskConnectivity::Connected,
                TaskAttention::None,
                TaskActivity::Idle,
                ReviewReadiness::NotReady,
                index as i64,
            )
        })
        .collect();
    let model = inbox_model(items);

    assert!(
        model.task_projection_index_search_resident_bytes() <= MAX_CLIENT_SEARCH_POSTING_BYTES,
        "resident search allocation estimate must include map nodes, keys, and vector capacities"
    );
    assert!(
        model.task_projection_index_search_index_keys() <= 100_000,
        "search key table must remain compact under adversarial titles"
    );
}

#[test]
fn inbox_filter_normalizes_expanding_unicode_with_the_indexed_title_bound() {
    let title = "İ".repeat(160);
    let model = inbox_model(vec![inbox_task_item(
        inbox_task_id(0),
        &title,
        TaskLifecycle::Open,
        TaskConnectivity::Connected,
        TaskAttention::None,
        TaskActivity::Idle,
        ReviewReadiness::NotReady,
        0,
    )]);
    let inbox =
        Inbox::from_model_with_filter(&model, &InboxFilter::new(&title), &Default::default());
    assert_eq!(
        inbox.len(),
        1,
        "UI filter and index must share normalization"
    );
}

#[test]
fn search_and_filter_bound_multi_megabyte_expanding_unicode_before_work() {
    let hostile = "İ".repeat(2_000_000);
    let model = inbox_model(vec![inbox_task_item(
        inbox_task_id(0),
        &hostile,
        TaskLifecycle::Open,
        TaskConnectivity::Connected,
        TaskAttention::None,
        TaskActivity::Idle,
        ReviewReadiness::NotReady,
        0,
    )]);
    let (ids, total, work) = model.search_task_ids_with_work(&hostile, false);
    assert_eq!(ids, vec![inbox_task_id(0)]);
    assert_eq!(total, 1);
    assert!(work <= 5_000);
    let inbox =
        Inbox::from_model_with_filter(&model, &InboxFilter::new(&hostile), &Default::default());
    assert_eq!(inbox.len(), 1);
}

#[test]
fn indexed_search_retains_title_tie_order_before_page_cap() {
    let items = (0..6_000)
        .map(|index| {
            let title = if index % 2 == 0 {
                format!("Task Alpha {index:04}")
            } else {
                format!("Task Zebra {index:04}")
            };
            inbox_task_item(
                inbox_task_id(index),
                &title,
                TaskLifecycle::Open,
                TaskConnectivity::Connected,
                TaskAttention::None,
                TaskActivity::Idle,
                ReviewReadiness::NotReady,
                1,
            )
        })
        .collect();
    let model = inbox_model(items);
    let (ids, total, work) = model.search_task_ids_with_work("task ", false);

    let mut expected = (0..6_000)
        .map(|index| {
            (
                if index % 2 == 0 {
                    format!("task alpha {index:04}")
                } else {
                    format!("task zebra {index:04}")
                },
                inbox_task_id(index),
            )
        })
        .collect::<Vec<_>>();
    expected.sort_by(|left, right| left.cmp(right));
    assert_eq!(total, 6_000);
    assert_eq!(
        ids,
        expected
            .into_iter()
            .take(5_000)
            .map(|(_, task_id)| task_id)
            .collect::<Vec<_>>()
    );
    assert!(
        work <= 5_000,
        "title-index query scanned too many candidates"
    );
}

#[test]
fn case_only_titles_use_the_same_stable_order_and_fixture_exposes_action_coverage() {
    let fixture: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/ui/task-inbox.json"))
            .expect("task inbox fixture");
    let tie = &fixture["behavioral_cases"]["case_only_title_ties"];
    assert_eq!(tie["titles"], serde_json::json!(["Alpha", "alpha"]));
    assert_eq!(tie["expected_order"], serde_json::json!(["Alpha", "alpha"]));

    let model = inbox_model(vec![
        inbox_task_item(
            inbox_task_id(60),
            "Alpha",
            TaskLifecycle::Open,
            TaskConnectivity::Connected,
            TaskAttention::None,
            TaskActivity::Idle,
            ReviewReadiness::NotReady,
            60,
        ),
        inbox_task_item(
            inbox_task_id(61),
            "alpha",
            TaskLifecycle::Open,
            TaskConnectivity::Connected,
            TaskAttention::None,
            TaskActivity::Idle,
            ReviewReadiness::NotReady,
            60,
        ),
    ]);
    let inbox = Inbox::from_model(&model);
    assert_eq!(
        inbox.task_ids().collect::<Vec<_>>(),
        vec![inbox_task_id(60), inbox_task_id(61)]
    );
    assert_eq!(
        fixture["behavioral_cases"]["actions"]["mark_read"],
        "client-local"
    );
    assert_eq!(
        fixture["behavioral_cases"]["actions"]["capture"],
        "task-id-and-epoch"
    );
}
