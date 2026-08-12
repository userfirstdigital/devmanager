use devmanager::ui::preview::{
    parse_preview_args, PreviewApplication, PreviewDismiss, PreviewError, PreviewOutputCapability,
    PreviewPathPolicy, PreviewRequest, PREVIEW_SCHEMA,
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
  "root": {
    "kind": "minimal",
    "label": "DevManager native preview"
  }
}"#;

static HEADLESS_INIT_TEST_LOCK: Mutex<()> = Mutex::new(());

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
fn preview_output_metadata_is_deterministic_and_refuses_fake_png() {
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
        PreviewOutputCapability::HeadlessProjectionOnly
    );
    assert!(!first.output_written);
    assert!(!first.host_started);

    let refusal = preview
        .render_to_output()
        .expect_err("PNG rendering must remain an explicit capability refusal");
    assert_eq!(refusal, PreviewError::HeadlessRenderingUnsupported);
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

#[test]
fn preview_execution_returns_explicit_headless_support_error_without_writing_output() {
    let (_root, policy) = temporary_policy();
    let request = valid_request(&policy);
    let output = request.output_path().to_path_buf();
    let preview = PreviewApplication::load(request, &policy).expect("fixture should load");

    let error = preview
        .render_to_output()
        .expect_err("unproven Windows headless PNG rendering must be visible");
    assert!(matches!(error, PreviewError::HeadlessRenderingUnsupported));
    assert!(!output.exists());
}

#[test]
#[ignore = "GPUI 0.2.2 has no official isolated pixel readback or PNG encoder"]
fn preview_renders_a_concrete_png_from_the_native_gpui_root() {
    let (_root, policy) = temporary_policy();
    let request = valid_request(&policy);
    let output = request.output_path().to_path_buf();
    let preview = PreviewApplication::load(request, &policy).expect("fixture should load");

    preview
        .render_to_output()
        .expect("the native GPUI root must render to the requested PNG");

    let bytes = fs::read(&output).expect("preview PNG should be written");
    assert_eq!(&bytes[..8], b"\x89PNG\r\n\x1a\n");
    assert_eq!(&bytes[12..16], b"IHDR");
    assert_eq!(u32::from_be_bytes(bytes[16..20].try_into().unwrap()), 320);
    assert_eq!(u32::from_be_bytes(bytes[20..24].try_into().unwrap()), 160);
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

fn dock_uuid(tail: u8) -> [u8; 16] {
    [
        0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        tail,
    ]
}

fn dock_task(tail: u8) -> devmanager::domain::TaskId {
    devmanager::domain::TaskId::from_bytes(dock_uuid(tail)).expect("task id")
}

fn dock_agent(tail: u8) -> devmanager::domain::AgentSessionId {
    devmanager::domain::AgentSessionId::from_bytes(dock_uuid(tail)).expect("agent session id")
}

fn dock_resource(tail: u8) -> devmanager::domain::ResourceId {
    devmanager::domain::ResourceId::from_bytes(dock_uuid(tail)).expect("resource id")
}

fn dock_snapshot(
    task_tail: u8,
    agent_tail: u8,
    resource_tail: u8,
    runtime_generation: u64,
    resource_generation: u64,
) -> devmanager::domain::TaskSnapshot {
    use devmanager::domain::{
        AgentRole, AgentSessionFacts, EnvironmentId, OwnerKind, ProjectId, ResourceFacts,
        ResourceKind, ResourceRecipe, ReviewReadiness, TaskActivity, TaskAssignment, TaskAttention,
        TaskConnectivity, TaskFacts, TaskSnapshot, WorkspaceRef,
    };
    use std::collections::BTreeMap;

    let task_id = dock_task(task_tail);
    let mut task = TaskFacts::new(
        EnvironmentId::from_bytes(dock_uuid(0x01)).expect("env"),
        "Dock task",
        None,
        ProjectId::from_bytes(dock_uuid(0x02)).expect("project"),
        WorkspaceRef::Main,
        TaskAssignment::LocalOwner,
        1,
    )
    .expect("task facts");
    task.id = task_id;

    let mut agent =
        AgentSessionFacts::new(task_id, AgentRole::Primary, "claude", None).expect("agent facts");
    agent.id = dock_agent(agent_tail);
    agent.runtime_generation = runtime_generation;

    let mut resource = ResourceFacts::new(
        Some(task_id),
        OwnerKind::Task,
        ResourceKind::Terminal,
        ResourceRecipe::Terminal { cols: 40, rows: 8 },
        1,
    )
    .expect("resource facts");
    resource.id = dock_resource(resource_tail);
    resource.runtime_generation = resource_generation;
    let agent_id = agent.id;
    let resource_id = resource.id;

    TaskSnapshot {
        task,
        connectivity: TaskConnectivity::Connected,
        attention: TaskAttention::None,
        activity: TaskActivity::Idle,
        review_readiness: ReviewReadiness::NotReady,
        agents: BTreeMap::from([(agent_id, agent)]),
        primary_agent_id: Some(agent_id),
        artifacts: BTreeMap::new(),
        resources: BTreeMap::from([(resource_id, resource)]),
    }
}

fn dock_bind(
    dock: &mut devmanager::ui::task_cockpit::dock::ContextDock,
    model: &devmanager::client::model::ClientModel,
    task_id: devmanager::domain::TaskId,
) {
    dock.follow_task(task_id);
    dock.bind_from_model(model).expect("host binding");
}

fn dock_select(
    dock: &mut devmanager::ui::task_cockpit::dock::ContextDock,
    model: &devmanager::client::model::ClientModel,
    tool: devmanager::ui::task_cockpit::dock::DockTool,
) {
    use devmanager::domain::RequestId;
    use devmanager::ui::task_cockpit::dock::DockShortcut;
    let index = match tool {
        devmanager::ui::task_cockpit::dock::DockTool::Changes => 1,
        devmanager::ui::task_cockpit::dock::DockTool::Files => 2,
        devmanager::ui::task_cockpit::dock::DockTool::Terminal => 3,
        devmanager::ui::task_cockpit::dock::DockTool::Browser => 4,
        devmanager::ui::task_cockpit::dock::DockTool::Services => 5,
        devmanager::ui::task_cockpit::dock::DockTool::Artifacts => 6,
        devmanager::ui::task_cockpit::dock::DockTool::Review => 7,
    };
    dock.dispatch_shortcut(DockShortcut::AltTool(index), RequestId::new(), model)
        .expect("dispatch tool");
}

#[test]
fn dock_exposes_seven_tools_and_keeps_one_active() {
    use devmanager::ui::task_cockpit::dock::{ContextDock, DockEdge, DockTool};

    let mut dock = ContextDock::new(DockEdge::Right);
    assert_eq!(
        ContextDock::tools(),
        &[
            DockTool::Changes,
            DockTool::Files,
            DockTool::Terminal,
            DockTool::Browser,
            DockTool::Services,
            DockTool::Artifacts,
            DockTool::Review,
        ]
    );
    let model = dock_model(0x21, 0x21, 0x21, 1, 1);
    dock.follow_task(dock_task(0x21));
    assert_eq!(dock.active_tool(), DockTool::Files);
    dock_select(&mut dock, &model, DockTool::Terminal);
    assert_eq!(dock.active_tool(), DockTool::Terminal);
    dock_select(&mut dock, &model, DockTool::Browser);
    assert_eq!(dock.active_tool(), DockTool::Browser);
    assert!(!dock.showing_raw_terminal());
}

#[test]
fn dock_remembers_tool_and_size_per_task() {
    use devmanager::ui::task_cockpit::dock::{ContextDock, DockEdge, DockTool};

    let mut dock = ContextDock::new(DockEdge::Right);
    let first_model = dock_model(0x31, 0x31, 0x31, 1, 1);
    let second_model = dock_model(0x32, 0x32, 0x32, 1, 1);
    let first = dock_task(0x31);
    let second = dock_task(0x32);
    dock.follow_task(first);
    dock_select(&mut dock, &first_model, DockTool::Terminal);
    assert!(dock.resize(0.42).is_ok());
    dock.follow_task(second);
    dock_select(&mut dock, &second_model, DockTool::Artifacts);
    assert!(dock.resize(0.28).is_ok());
    dock.follow_task(first);
    assert_eq!(dock.active_tool(), DockTool::Terminal);
    assert!((dock.size_ratio() - 0.42).abs() < f32::EPSILON);
    dock.follow_task(second);
    assert_eq!(dock.active_tool(), DockTool::Artifacts);
    assert!((dock.size_ratio() - 0.28).abs() < f32::EPSILON);
}

#[test]
fn dock_collapse_and_reopen_preserves_remembered_tool() {
    use devmanager::ui::task_cockpit::dock::{ContextDock, DockEdge, DockTool};

    let mut dock = ContextDock::new(DockEdge::Bottom);
    let model = dock_model(0x41, 0x41, 0x41, 1, 1);
    dock.follow_task(dock_task(0x41));
    dock_select(&mut dock, &model, DockTool::Review);
    dock.collapse();
    assert!(dock.is_collapsed());
    assert_eq!(dock.active_tool(), DockTool::Review);
    dock.reopen();
    assert!(!dock.is_collapsed());
    assert_eq!(dock.active_tool(), DockTool::Review);
}

#[test]
fn dock_marks_unprojected_tools_unavailable() {
    use devmanager::ui::task_cockpit::dock::{
        ContextDock, DockEdge, DockTool, DockUnavailableReason,
    };

    let mut dock = ContextDock::new(DockEdge::Right);
    assert_eq!(
        dock.tool_availability(DockTool::Files)
            .expect_err("no task")
            .reason,
        DockUnavailableReason::NoTaskSelected
    );

    dock.follow_task(dock_task(0x51));
    for tool in [
        DockTool::Changes,
        DockTool::Files,
        DockTool::Browser,
        DockTool::Services,
        DockTool::Artifacts,
        DockTool::Review,
    ] {
        assert_eq!(
            dock.tool_availability(tool)
                .expect_err("missing panel")
                .reason,
            DockUnavailableReason::MissingHostProjection
        );
    }
    assert_eq!(
        dock.tool_availability(DockTool::Terminal)
            .expect_err("unbound terminal")
            .reason,
        DockUnavailableReason::NoMatchingTerminal
    );
}

#[test]
fn dock_rejects_forgeable_partial_binding_and_foreign_snapshots() {
    use devmanager::ui::task_cockpit::dock::{
        ContextDock, DockEdge, DockProjectionError, HostStreamCursor,
    };

    let mut dock = ContextDock::new(DockEdge::Right);
    let model = dock_model(0x61, 0x61, 0x61, 3, 3);
    dock_bind(&mut dock, &model, dock_task(0x61));
    let first = HostStreamCursor::delta(&model, dock_task(0x61), 1).expect("cursor");
    dock.admit_host_cursor(first).expect("seq 1");
    assert!(dock.replica_view().is_none());

    let foreign = dock_snapshot(0x99, 0x61, 0x61, 3, 3);
    assert!(matches!(
        dock.bind_from_projection(&foreign),
        Err(DockProjectionError::ForeignIdentity)
    ));
    let before = dock.projection_fingerprint();
    let bumped = dock_model(0x61, 0x61, 0x61, 99, 3);
    let bumped_cursor = HostStreamCursor::delta(&bumped, dock_task(0x61), 2).expect("bumped");
    assert!(matches!(
        dock.admit_host_cursor(bumped_cursor),
        Err(DockProjectionError::GenerationMismatch { .. })
    ));
    assert_eq!(dock.projection_fingerprint(), before);

    let zero = HostStreamCursor::delta(&model, dock_task(0x61), 0).expect("zero");
    assert!(matches!(
        dock.admit_host_cursor(zero),
        Err(DockProjectionError::ZeroSequence)
    ));
    let stale = HostStreamCursor::delta(&model, dock_task(0x61), 1).expect("stale");
    assert!(matches!(
        dock.admit_host_cursor(stale),
        Err(DockProjectionError::RegressedSequence { last: 1, actual: 1 })
    ));
}

#[test]
fn dock_preserves_viewport_across_task_switch_and_evicts_257th() {
    use devmanager::terminal::view::{
        TerminalScrollbarModel, TerminalSearchHighlight, TerminalSearchUiModel,
        TerminalSelectionSnapshot,
    };
    use devmanager::ui::task_cockpit::dock::{
        ContextDock, DockEdge, DockTool, TerminalPresentation, TerminalViewport,
    };

    let mut dock = ContextDock::new(DockEdge::Right);
    let first = dock_model(0x71, 0x71, 0x71, 1, 1);
    dock_bind(&mut dock, &first, dock_task(0x71));
    dock_select(&mut dock, &first, DockTool::Terminal);
    dock.dispatch_shortcut(
        devmanager::ui::task_cockpit::dock::DockShortcut::ToggleRawTerminal,
        devmanager::domain::RequestId::new(),
        &first,
    )
    .expect("raw");
    dock.set_viewport(TerminalViewport {
        selection: Some(TerminalSelectionSnapshot {
            start_row: 0,
            start_column: 1,
            end_row: 0,
            end_column: 4,
        }),
        search: Some(TerminalSearchUiModel {
            query: "hel".into(),
            summary: "1/1".into(),
            case_sensitive: false,
        }),
        search_highlight: Some(TerminalSearchHighlight {
            row: 0,
            start_column: 0,
            end_column: 3,
        }),
        scrollbar: Some(TerminalScrollbarModel {
            thumb_top_ratio: 0.25,
            thumb_height_ratio: 0.5,
        }),
        focused: true,
    });
    let second = dock_model(0x72, 0x72, 0x72, 1, 1);
    dock_bind(&mut dock, &second, dock_task(0x72));
    dock_select(&mut dock, &second, DockTool::Artifacts);
    assert!(dock.resize(0.28).is_ok());

    dock.follow_task(dock_task(0x71));
    assert_eq!(dock.active_tool(), DockTool::Terminal);
    assert_eq!(dock.terminal_presentation(), TerminalPresentation::Raw);
    assert_eq!(dock.viewport().selection.unwrap().start_column, 1);
    assert_eq!(dock.viewport().search.as_ref().unwrap().query, "hel");
    assert_eq!(dock.viewport().search_highlight.unwrap().end_column, 3);
    assert_eq!(dock.viewport().scrollbar.unwrap().thumb_top_ratio, 0.25);

    for _ in 0..255 {
        dock.follow_task(devmanager::domain::TaskId::new());
    }
    dock.follow_task(dock_task(0x71));
    assert_eq!(dock.active_tool(), DockTool::Files);
    assert!(dock.viewport().selection.is_none());
}

#[test]
fn dock_generation_replacement_clears_only_replaced_runtime_viewport() {
    use devmanager::terminal::view::TerminalSelectionSnapshot;
    use devmanager::ui::task_cockpit::dock::{ContextDock, DockEdge, DockTool, TerminalViewport};

    let mut dock = ContextDock::new(DockEdge::Right);
    let gen1 = dock_model(0x81, 0x81, 0x81, 1, 1);
    dock_bind(&mut dock, &gen1, dock_task(0x81));
    dock_select(&mut dock, &gen1, DockTool::Terminal);
    dock.set_viewport(TerminalViewport {
        selection: Some(TerminalSelectionSnapshot {
            start_row: 2,
            start_column: 2,
            end_row: 2,
            end_column: 6,
        }),
        search: None,
        search_highlight: None,
        scrollbar: None,
        focused: true,
    });
    let other = dock_model(0x82, 0x82, 0x82, 1, 1);
    dock_bind(&mut dock, &other, dock_task(0x82));
    dock_select(&mut dock, &other, DockTool::Changes);

    let gen2 = dock_model(0x81, 0x81, 0x81, 2, 2);
    dock.follow_task(dock_task(0x81));
    dock.bind_from_model(&gen2).expect("gen2");
    assert!(dock.viewport().selection.is_none());
    dock.follow_task(dock_task(0x82));
    assert_eq!(dock.active_tool(), DockTool::Changes);
}

#[test]
fn dock_view_switch_requires_process_manager_census() {
    use devmanager::ui::task_cockpit::dock::{ContextDock, DependencyUnavailable, DockEdge};

    let mut dock = ContextDock::new(DockEdge::Right);
    let model = dock_model(0x91, 0x91, 0x91, 9, 9);
    dock_bind(&mut dock, &model, dock_task(0x91));

    let without = dock
        .switch_to_semantic(&model, None)
        .expect("semantic switch");
    assert_eq!(
        without.identity().map(|identity| identity.task_id()),
        Some(dock_task(0x91))
    );
    assert_eq!(without.census(), Err(DependencyUnavailable::RuntimeCensus));
}

#[test]
fn dock_reconnect_overlay_rejects_live_mismatch_and_keeps_last_grid() {
    use devmanager::terminal::view::render_terminal_surface;
    use devmanager::ui::task_cockpit::dock::{
        ContextDock, DockEdge, DockProjectionError, TerminalSurfaceState,
    };

    let mut dock = ContextDock::new(DockEdge::Right);
    let model = dock_model(0xa1, 0xa1, 0xa1, 2, 2);
    dock_bind(&mut dock, &model, dock_task(0xa1));
    assert!(matches!(
        dock.present_host_overlay(&model, TerminalSurfaceState::Live, None),
        Err(DockProjectionError::OverlayViewRejected)
    ));
    dock.present_host_overlay(
        &model,
        TerminalSurfaceState::Reconnecting,
        Some("secret=super-secret-value and done"),
    )
    .expect("overlay");

    let pane = dock.terminal_pane_model();
    assert!(pane.session.is_none());
    assert_eq!(
        pane.blocking_notice.as_deref(),
        Some("Reconnecting to terminal")
    );
    let _surface = render_terminal_surface(&pane, None);
    let exit = dock.present_host_overlay(
        &model,
        TerminalSurfaceState::Exited,
        Some("credential=hunter2"),
    );
    assert!(exit.is_ok());
    assert!(!dock
        .terminal_pane_model()
        .blocking_notice
        .unwrap_or_default()
        .contains("hunter2"));
}

#[test]
fn dock_rejects_nan_size_and_keeps_single_view_truth() {
    use devmanager::ui::task_cockpit::dock::{
        ContextDock, DockEdge, DockProjectionError, DockTool,
    };

    let mut dock = ContextDock::new(DockEdge::Right);
    let model = dock_model(0xb1, 0xb1, 0xb1, 1, 1);
    dock.follow_task(dock_task(0xb1));
    dock_select(&mut dock, &model, DockTool::Terminal);
    dock.dispatch_shortcut(
        devmanager::ui::task_cockpit::dock::DockShortcut::ToggleRawTerminal,
        devmanager::domain::RequestId::new(),
        &model,
    )
    .expect("raw");
    assert!(dock.showing_raw_terminal());
    dock_select(&mut dock, &model, DockTool::Files);
    assert!(!dock.showing_raw_terminal());
    assert_eq!(dock.active_tool(), DockTool::Files);
    assert!(matches!(
        dock.resize(f32::NAN),
        Err(DockProjectionError::NonFiniteSize)
    ));
    assert!(dock.resize(0.42).is_ok());
}

#[test]
fn dock_chrome_projects_tabs_splitter_and_unavailable_panels() {
    use devmanager::domain::RequestId;
    use devmanager::ui::task_cockpit::dock::{ContextDock, DockEdge, DockShortcut, DockTool};
    use devmanager::ui::tokens::{theme, Density, Scale, ThemeMode};

    assert_eq!(
        ContextDock::placement_for_aspect(1600.0, 900.0, None),
        DockEdge::Right
    );
    assert_eq!(
        ContextDock::placement_for_aspect(800.0, 1200.0, None),
        DockEdge::Bottom
    );
    assert_eq!(
        ContextDock::placement_for_aspect(800.0, 1200.0, Some(DockEdge::Right)),
        DockEdge::Right
    );

    let mut dock = ContextDock::new(ContextDock::placement_for_aspect(1440.0, 900.0, None));
    let model = dock_model(0xc1, 0xc1, 0xc1, 1, 1);
    dock.follow_task(dock_task(0xc1));
    let chrome = dock.chrome();
    assert_eq!(chrome.edge, DockEdge::Right);
    assert_eq!(chrome.tabs.len(), 7);
    assert!(chrome.tabs.iter().all(|tab| !tab.name.is_empty()));
    assert_eq!(chrome.tabs[1].tool, DockTool::Files);
    assert!(chrome.resize_handle.is_some());
    assert!(chrome.unavailable.is_some());
    dock.dispatch_shortcut(DockShortcut::AltTool(3), RequestId::new(), &model)
        .expect("alt-3");
    assert_eq!(dock.active_tool(), DockTool::Terminal);
    let tokens = theme(ThemeMode::Dark, Density::Compact, Scale::Scale100);
    let _element = dock.render_context_dock(tokens);
}

#[test]
fn dock_resource_only_generation_mismatch_is_distinct_and_does_not_mutate() {
    use devmanager::ui::task_cockpit::dock::{
        ContextDock, DockEdge, DockProjectionError, HostStreamCursor,
    };

    let mut dock = ContextDock::new(DockEdge::Right);
    let model = dock_model(0xd1, 0xd1, 0xd1, 1, 1);
    dock_bind(&mut dock, &model, dock_task(0xd1));
    dock.admit_host_cursor(HostStreamCursor::delta(&model, dock_task(0xd1), 1).expect("seq1"))
        .expect("live");
    let before = dock.projection_fingerprint();
    let resource_only = dock_model(0xd1, 0xd1, 0xd1, 1, 8);
    let cursor = HostStreamCursor::delta(&resource_only, dock_task(0xd1), 2).expect("seq2");
    assert!(matches!(
        dock.admit_host_cursor(cursor),
        Err(DockProjectionError::GenerationMismatch {
            expected_runtime: 1,
            actual_runtime: 1,
            expected_resource: 1,
            actual_resource: 8,
        })
    ));
    assert_eq!(dock.projection_fingerprint(), before);
}

#[test]
fn dock_host_stream_and_native_mount_remain_typed_holds() {
    use devmanager::ui::task_cockpit::dock::{
        ContextDock, DependencyUnavailable, DockEdge, HostStreamCursor,
    };
    use devmanager::ui::task_cockpit::shell::TaskCockpitShell;

    let model = dock_model(0xd2, 0xd2, 0xd2, 1, 1);
    let mut dock = ContextDock::new(DockEdge::Right);
    dock_bind(&mut dock, &model, dock_task(0xd2));
    let report = dock
        .admit_host_cursor(HostStreamCursor::delta(&model, dock_task(0xd2), 1).expect("cursor"))
        .expect("admit");
    assert_eq!(
        report.stream(),
        Err(DependencyUnavailable::HostTerminalStream)
    );
    assert_eq!(
        dock.emit_terminal_mouse_to_host(),
        Err(DependencyUnavailable::PtyInput)
    );
    let shell = TaskCockpitShell::new(DockEdge::Right);
    assert_eq!(
        shell.native_bin_mount(),
        Err(DependencyUnavailable::NativeShellMount)
    );
}

#[test]
fn dock_shell_mounts_tabs_and_dispatches_captured_gpui_actions() {
    use devmanager::client::action;
    use devmanager::domain::RequestId;
    use devmanager::ui::task_cockpit::dock::DockTool;
    use devmanager::ui::task_cockpit::shell::TaskCockpitShell;

    assert_eq!(action::catalog().len(), 6);
    let model = dock_model(0xe1, 0xe1, 0xe1, 1, 1);
    let mut shell = TaskCockpitShell::new(devmanager::ui::task_cockpit::dock::DockEdge::Right);
    shell.follow_task(dock_task(0xe1));
    shell.follow_projection(model.clone());
    let request_id = RequestId::new();
    shell
        .handle_tool_action(DockTool::Terminal, request_id)
        .expect("captured dispatch");
    assert_eq!(shell.dock().active_tool(), DockTool::Terminal);
    let stale = shell
        .dock()
        .capture_action(DockTool::Files, RequestId::new())
        .expect("capture");
    shell
        .handle_tool_action(DockTool::Browser, RequestId::new())
        .expect("advance epochs");
    assert!(shell.dock_mut().dispatch_action(stale, &model).is_err());
}

fn dock_model(
    task_tail: u8,
    agent_tail: u8,
    resource_tail: u8,
    runtime_generation: u64,
    resource_generation: u64,
) -> devmanager::client::model::ClientModel {
    dock_model_with_resources(
        task_tail,
        agent_tail,
        runtime_generation,
        vec![(resource_tail, resource_generation)],
        true,
    )
}

fn dock_model_with_resources(
    task_tail: u8,
    agent_tail: u8,
    runtime_generation: u64,
    terminals: Vec<(u8, u64)>,
    primary: bool,
) -> devmanager::client::model::ClientModel {
    use devmanager::client::model::ClientModelBuilder;
    use devmanager::domain::{
        AgentRole, AgentSessionFacts, AgentSessionLifecycle, EnvironmentId, OwnerKind, ProjectId,
        ResourceFacts, ResourceKind, ResourceLifecycle, ResourceRecipe, ReviewReadiness,
        SnapshotId, SnapshotItem, SnapshotPage, SnapshotSection, TaskActivity, TaskAssignment,
        TaskAttention, TaskConnectivity, TaskFacts, TaskLifecycle, TaskSnapshotItem, WorkspaceRef,
    };

    let task_id = dock_task(task_tail);
    let agent_id = dock_agent(agent_tail);
    let snap = SnapshotId::from_bytes(dock_uuid(0x10)).expect("snapshot");
    let task_item = TaskSnapshotItem {
        task: TaskFacts {
            id: task_id,
            environment_id: EnvironmentId::from_bytes(dock_uuid(0x01)).expect("env"),
            title: "Dock task".into(),
            description: None,
            project_id: ProjectId::from_bytes(dock_uuid(0x02)).expect("project"),
            workspace: WorkspaceRef::Main,
            assignment: TaskAssignment::LocalOwner,
            lifecycle: TaskLifecycle::Open,
            action_epoch: 0,
            revision: 1,
            created_at_ms: 1,
        },
        connectivity: TaskConnectivity::Connected,
        attention: TaskAttention::None,
        activity: TaskActivity::Idle,
        review_readiness: ReviewReadiness::NotReady,
        primary_agent_id: primary.then_some(agent_id),
    };
    let agent = AgentSessionFacts {
        id: agent_id,
        task_id,
        role: if primary {
            AgentRole::Primary
        } else {
            AgentRole::specialist("reviewer").expect("specialist")
        },
        provider_kind: "claude".into(),
        provider_session_id: None,
        lifecycle: AgentSessionLifecycle::Open,
        runtime_generation,
        revision: 0,
    };
    let resources: Vec<_> = terminals
        .into_iter()
        .map(|(tail, generation)| {
            SnapshotItem::Resource(ResourceFacts {
                id: dock_resource(tail),
                task_id: Some(task_id),
                owner_kind: OwnerKind::Task,
                resource_kind: ResourceKind::Terminal,
                recipe: ResourceRecipe::Terminal { cols: 40, rows: 8 },
                lifecycle: ResourceLifecycle::Active,
                runtime_generation: generation,
                updated_at_ms: 1,
            })
        })
        .collect();

    let page = |section, items| SnapshotPage {
        snapshot_id: snap,
        through_sequence: 1,
        section,
        after_item: None,
        items,
        encoded_bytes: 1,
        next_cursor: None,
    };

    let mut builder = ClientModelBuilder::new();
    builder
        .ingest_page(page(
            SnapshotSection::Tasks,
            vec![SnapshotItem::Task(task_item)],
        ))
        .expect("tasks");
    builder
        .ingest_page(page(
            SnapshotSection::AgentSessions,
            vec![SnapshotItem::AgentSession(agent)],
        ))
        .expect("agents");
    builder
        .ingest_page(page(SnapshotSection::Artifacts, Vec::new()))
        .expect("artifacts");
    builder
        .ingest_page(page(SnapshotSection::Resources, resources))
        .expect("resources");
    builder
        .ingest_page(page(SnapshotSection::Operations, Vec::new()))
        .expect("operations");
    builder.finish().expect("client model")
}

#[test]
fn dock_bind_requires_client_model_and_rejects_hand_built_snapshot() {
    use devmanager::ui::task_cockpit::dock::{
        ContextDock, DockEdge, DockProjectionError, HostTerminalBinding,
    };

    let model = dock_model(0xf1, 0xf1, 0xf1, 1, 1);
    let mut dock = ContextDock::new(DockEdge::Right);
    dock.follow_task(dock_task(0xf1));
    let hand_built = dock_snapshot(0xf1, 0xf1, 0xf1, 1, 1);
    assert!(matches!(
        dock.bind_from_projection(&hand_built),
        Err(DockProjectionError::ForeignIdentity)
    ));
    dock.bind_from_model(&model).expect("sealed model bind");
    assert!(HostTerminalBinding::from_client_model(&model, dock_task(0xf1)).is_ok());
    assert!(HostTerminalBinding::from_client_model(&model, dock_task(0x99)).is_err());
}

#[test]
fn dock_rejects_two_terminal_ambiguity_and_specialist_without_primary() {
    use devmanager::ui::task_cockpit::dock::{
        ContextDock, DockEdge, DockProjectionError, HostTerminalBinding,
    };

    let two = dock_model_with_resources(0xf2, 0xf2, 1, vec![(0xf2, 1), (0xf3, 1)], true);
    assert!(matches!(
        HostTerminalBinding::from_client_model(&two, dock_task(0xf2)),
        Err(DockProjectionError::BindingMismatch)
    ));
    let specialist = dock_model_with_resources(0xf4, 0xf4, 1, vec![(0xf4, 1)], false);
    let mut dock = ContextDock::new(DockEdge::Right);
    dock.follow_task(dock_task(0xf4));
    assert!(matches!(
        dock.bind_from_model(&specialist),
        Err(DockProjectionError::Unbound)
    ));
}

#[test]
fn dock_sequence_gap_marks_resync_and_rejects_deltas_until_full_snapshot() {
    use devmanager::ui::task_cockpit::dock::{
        ContextDock, DependencyUnavailable, DockEdge, DockProjectionError, HostStreamCursor,
    };

    let model = dock_model(0xf5, 0xf5, 0xf5, 2, 2);
    let mut dock = ContextDock::new(DockEdge::Right);
    dock.follow_task(dock_task(0xf5));
    dock.bind_from_model(&model).expect("bind");
    let first = HostStreamCursor::delta(&model, dock_task(0xf5), 1).expect("cursor 1");
    let report = dock.admit_host_cursor(first).expect("seq 1");
    assert_eq!(
        report.stream(),
        Err(DependencyUnavailable::HostTerminalStream)
    );
    assert!(dock.replica_view().is_none());
    let before = dock.projection_fingerprint();
    let gap = HostStreamCursor::delta(&model, dock_task(0xf5), 3).expect("cursor 3");
    assert!(matches!(
        dock.admit_host_cursor(gap),
        Err(DockProjectionError::SequenceGap { last: 1, actual: 3 })
    ));
    assert!(dock.needs_resync());
    assert_eq!(
        dock.projection_fingerprint().last_sequence,
        before.last_sequence
    );
    let skipped = HostStreamCursor::delta(&model, dock_task(0xf5), 2).expect("cursor 2");
    assert!(matches!(
        dock.admit_host_cursor(skipped),
        Err(DockProjectionError::NeedsResync)
    ));
    let snapshot = HostStreamCursor::full_snapshot(&model, dock_task(0xf5), 4).expect("snap");
    let recovered = dock.admit_host_cursor(snapshot).expect("full snapshot");
    assert!(!dock.needs_resync());
    assert_eq!(
        recovered.stream(),
        Err(DependencyUnavailable::HostTerminalStream)
    );
    let next = HostStreamCursor::delta(&model, dock_task(0xf5), 5).expect("cursor 5");
    assert!(dock.admit_host_cursor(next).is_ok());
}

#[test]
fn dock_shortcuts_and_toggle_use_catalog_dispatch_only() {
    use devmanager::client::action::{self, ActionRequest};
    use devmanager::domain::RequestId;
    use devmanager::ui::task_cockpit::dock::{ContextDock, DockEdge, DockShortcut, DockTool};

    assert_eq!(action::catalog().len(), 6);
    let model = dock_model(0xf6, 0xf6, 0xf6, 1, 1);
    let mut dock = ContextDock::new(DockEdge::Right);
    dock.follow_task(dock_task(0xf6));
    dock.bind_from_model(&model).expect("bind");
    dock.dispatch_shortcut(DockShortcut::AltTool(3), RequestId::new(), &model)
        .expect("alt-3");
    assert_eq!(dock.active_tool(), DockTool::Terminal);
    let first = dock
        .capture_action(DockTool::Files, RequestId::new())
        .expect("capture files");
    assert!(matches!(
        first.catalog_request(),
        ActionRequest::TaskShow { task_id } if *task_id == dock_task(0xf6)
    ));
    dock.dispatch_shortcut(DockShortcut::ToggleRawTerminal, RequestId::new(), &model)
        .expect("toggle");
    assert!(dock.showing_raw_terminal());
    assert!(dock.dispatch_action(first, &model).is_err());
    let replay = dock
        .capture_action(DockTool::Browser, RequestId::new())
        .expect("capture browser");
    let request_id = replay.request_id();
    dock.dispatch_action(replay, &model).expect("browser");
    let replayed = dock
        .capture_action(DockTool::Files, request_id)
        .expect("replay capture");
    assert!(dock.dispatch_action(replayed, &model).is_err());
}

fn dock_two_task_model() -> devmanager::client::model::ClientModel {
    use devmanager::client::model::ClientModelBuilder;
    use devmanager::domain::{
        AgentRole, AgentSessionFacts, AgentSessionLifecycle, EnvironmentId, OwnerKind, ProjectId,
        ResourceFacts, ResourceKind, ResourceLifecycle, ResourceRecipe, ReviewReadiness,
        SnapshotId, SnapshotItem, SnapshotPage, SnapshotSection, TaskActivity, TaskAssignment,
        TaskAttention, TaskConnectivity, TaskFacts, TaskLifecycle, TaskSnapshotItem, WorkspaceRef,
    };

    let snap = SnapshotId::from_bytes(dock_uuid(0x10)).expect("snapshot");
    let page = |section, items| SnapshotPage {
        snapshot_id: snap,
        through_sequence: 1,
        section,
        after_item: None,
        items,
        encoded_bytes: 1,
        next_cursor: None,
    };
    let task_item = |tail: u8, agent_id| TaskSnapshotItem {
        task: TaskFacts {
            id: dock_task(tail),
            environment_id: EnvironmentId::from_bytes(dock_uuid(0x01)).expect("env"),
            title: "Dock task".into(),
            description: None,
            project_id: ProjectId::from_bytes(dock_uuid(0x02)).expect("project"),
            workspace: WorkspaceRef::Main,
            assignment: TaskAssignment::LocalOwner,
            lifecycle: TaskLifecycle::Open,
            action_epoch: 0,
            revision: 1,
            created_at_ms: 1,
        },
        connectivity: TaskConnectivity::Connected,
        attention: TaskAttention::None,
        activity: TaskActivity::Idle,
        review_readiness: ReviewReadiness::NotReady,
        primary_agent_id: Some(agent_id),
    };
    let agent = |tail: u8| AgentSessionFacts {
        id: dock_agent(tail),
        task_id: dock_task(tail),
        role: AgentRole::Primary,
        provider_kind: "claude".into(),
        provider_session_id: None,
        lifecycle: AgentSessionLifecycle::Open,
        runtime_generation: 1,
        revision: 0,
    };
    let resource = |tail: u8| {
        SnapshotItem::Resource(ResourceFacts {
            id: dock_resource(tail),
            task_id: Some(dock_task(tail)),
            owner_kind: OwnerKind::Task,
            resource_kind: ResourceKind::Terminal,
            recipe: ResourceRecipe::Terminal { cols: 40, rows: 8 },
            lifecycle: ResourceLifecycle::Active,
            runtime_generation: 1,
            updated_at_ms: 1,
        })
    };
    let first_agent = agent(0xa1);
    let second_agent = agent(0xa2);
    let mut builder = ClientModelBuilder::new();
    builder
        .ingest_page(page(
            SnapshotSection::Tasks,
            vec![
                SnapshotItem::Task(task_item(0xa1, first_agent.id)),
                SnapshotItem::Task(task_item(0xa2, second_agent.id)),
            ],
        ))
        .expect("tasks");
    builder
        .ingest_page(page(
            SnapshotSection::AgentSessions,
            vec![
                SnapshotItem::AgentSession(first_agent),
                SnapshotItem::AgentSession(second_agent),
            ],
        ))
        .expect("agents");
    builder
        .ingest_page(page(SnapshotSection::Artifacts, Vec::new()))
        .expect("artifacts");
    builder
        .ingest_page(page(
            SnapshotSection::Resources,
            vec![resource(0xa1), resource(0xa2)],
        ))
        .expect("resources");
    builder
        .ingest_page(page(SnapshotSection::Operations, Vec::new()))
        .expect("operations");
    builder.finish().expect("two-task model")
}

#[test]
fn dock_follow_projection_does_not_invent_task_from_map_order() {
    use devmanager::ui::task_cockpit::dock::DockEdge;
    use devmanager::ui::task_cockpit::shell::TaskCockpitShell;

    let two = dock_two_task_model();
    let mut shell = TaskCockpitShell::new(DockEdge::Right);
    shell.follow_projection(two);
    assert!(shell.dock().selected_task().is_none());
    assert!(shell.dock().terminal_binding().is_none());

    let one = dock_model(0xa3, 0xa3, 0xa3, 1, 1);
    let mut unbound = TaskCockpitShell::new(DockEdge::Right);
    unbound.follow_projection(one);
    assert!(unbound.dock().selected_task().is_none());
    assert!(unbound.dock().terminal_binding().is_none());
}

#[test]
fn dock_canonical_mount_sequence_gates_identity_resync_pointer_paint_and_holds() {
    use devmanager::client::action::{self, ActionRequest};
    use devmanager::domain::RequestId;
    use devmanager::services::ProcessManager;
    use devmanager::terminal::view::render_terminal_surface;
    use devmanager::ui::task_cockpit::dock::{
        ContextDock, DependencyUnavailable, DockEdge, DockPointerSurface, DockProjectionError,
        DockShortcut, DockTool, HostStreamCursor, PointerButton, PointerPhase, PointerPress,
        ProcessManagerCensus,
    };
    use devmanager::ui::task_cockpit::shell::TaskCockpitShell;

    assert_eq!(action::catalog().len(), 6);

    let model = dock_model(0xb2, 0xb2, 0xb2, 4, 4);
    let foreign = dock_model(0xb3, 0xb3, 0xb3, 4, 4);
    let bumped = dock_model(0xb2, 0xb2, 0xb2, 9, 4);
    let mut dock = ContextDock::new(DockEdge::Right);
    dock.follow_task(dock_task(0xb2));
    assert!(matches!(
        dock.bind_from_projection(&dock_snapshot(0xb2, 0xb2, 0xb2, 4, 4)),
        Err(DockProjectionError::ForeignIdentity)
    ));
    dock.bind_from_model(&model).expect("sealed bind");

    let raw = dock.switch_to_raw_terminal(&model, None).expect("raw");
    let semantic = dock.switch_to_semantic(&model, None).expect("semantic");
    assert_eq!(raw.identity(), semantic.identity());
    assert_eq!(
        raw.identity().map(|identity| identity.task_id()),
        Some(dock_task(0xb2))
    );
    assert_eq!(
        raw.identity().map(|identity| identity.agent_session_id()),
        Some(dock_agent(0xb2))
    );
    assert_eq!(
        raw.identity().map(|identity| identity.runtime_generation()),
        Some(4)
    );
    assert_eq!(raw.census(), Err(DependencyUnavailable::RuntimeCensus));
    assert!(matches!(
        dock.switch_to_raw_terminal(&foreign, None),
        Err(DockProjectionError::ForeignIdentity)
    ));
    assert!(matches!(
        dock.switch_to_raw_terminal(&bumped, None),
        Err(DockProjectionError::GenerationMismatch { .. })
    ));
    assert!(!dock.showing_raw_terminal());

    dock.dispatch_shortcut(DockShortcut::AltTool(3), RequestId::new(), &model)
        .expect("terminal tool");
    let captured = dock
        .capture_action(DockTool::Terminal, RequestId::new())
        .expect("catalog");
    assert!(matches!(
        captured.catalog_request(),
        ActionRequest::TaskShow { task_id } if *task_id == dock_task(0xb2)
    ));
    dock.dispatch_shortcut(DockShortcut::ToggleRawTerminal, RequestId::new(), &model)
        .expect("raw catalog");
    dock.focus_terminal();

    let first = HostStreamCursor::delta(&model, dock_task(0xb2), 1).expect("seq1");
    assert_eq!(
        dock.admit_host_cursor(first).expect("admit").stream(),
        Err(DependencyUnavailable::HostTerminalStream)
    );
    assert!(matches!(
        dock.admit_host_cursor(HostStreamCursor::delta(&model, dock_task(0xb2), 3).expect("gap")),
        Err(DockProjectionError::SequenceGap { last: 1, actual: 3 })
    ));
    assert!(dock.needs_resync());
    assert!(matches!(
        dock.admit_host_cursor(HostStreamCursor::delta(&model, dock_task(0xb2), 2).expect("skip")),
        Err(DockProjectionError::NeedsResync)
    ));
    let blocked = PointerPress {
        pointer_id: 11,
        button: PointerButton::Left,
        surface: DockPointerSurface::TerminalGrid,
    };
    assert!(!dock.handle_gpui_pointer(PointerPhase::Down, blocked));
    assert!(matches!(
        dock.dispatch_shortcut(DockShortcut::ToggleRawTerminal, RequestId::new(), &model),
        Err(DockProjectionError::NeedsResync)
    ));
    assert_eq!(
        dock.admit_host_cursor(
            HostStreamCursor::full_snapshot(&model, dock_task(0xb2), 4).expect("full")
        )
        .expect("resync")
        .stream(),
        Err(DependencyUnavailable::HostTerminalStream)
    );
    assert!(!dock.needs_resync());

    let down = PointerPress {
        pointer_id: 11,
        button: PointerButton::Left,
        surface: DockPointerSurface::TerminalGrid,
    };
    assert!(dock.handle_gpui_pointer(PointerPhase::Down, down));
    assert!(!dock.handle_gpui_pointer(
        PointerPhase::Up,
        PointerPress {
            pointer_id: 11,
            button: PointerButton::Left,
            surface: DockPointerSurface::Sidebar,
        }
    ));
    assert!(!dock.handle_gpui_pointer(
        PointerPhase::Cancel,
        PointerPress {
            pointer_id: 99,
            button: PointerButton::Left,
            surface: DockPointerSurface::TerminalGrid,
        }
    ));
    assert!(dock.handle_gpui_pointer(PointerPhase::Cancel, down));
    assert!(dock.press_owner().is_none());

    assert!(dock.replica_view().is_none());
    let pane = dock.terminal_pane_model();
    assert!(pane.session.is_none());
    let _native = render_terminal_surface(&pane, None);

    let manager = ProcessManager::new();
    let census = ProcessManagerCensus::new(&manager);
    assert_eq!(
        census.one_provider_one_pty_proof(),
        Err(DependencyUnavailable::LiveRuntimeCensus)
    );
    assert_eq!(
        dock.emit_terminal_mouse_to_host(),
        Err(DependencyUnavailable::PtyInput)
    );
    let shell = TaskCockpitShell::new(DockEdge::Right);
    assert_eq!(
        shell.native_bin_mount(),
        Err(DependencyUnavailable::NativeShellMount)
    );
}
