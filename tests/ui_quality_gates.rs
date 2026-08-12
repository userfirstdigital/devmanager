use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use sha2::{Digest, Sha256};

use devmanager::client::action::{
    catalog, task_create_command, task_rename_command, ActionArgumentSchema, ActionRequest,
    TaskCreateArguments, TaskRenameArguments, ACTION_HOST_ACTIONS, ACTION_HOST_STATUS,
    ACTION_TASK_CREATE, ACTION_TASK_LIST, ACTION_TASK_RENAME, ACTION_TASK_SHOW,
};
use devmanager::client::model::{MAX_CLIENT_MODEL_ITEMS, MAX_CLIENT_REPLAY_PAGES};
use devmanager::domain::id::{ClientId, CommandId, EnvironmentId, ProjectId, TaskId};
use devmanager::domain::snapshot::{
    PageLimits, MAX_SNAPSHOT_PAGE_ENCODED_BYTES, MAX_SNAPSHOT_PAGE_ITEMS,
};
use devmanager::domain::task::WorkspaceRef;
use devmanager::host::HostCleanupWorker;
use devmanager::ui::components::empty_state::EmptyState;
use devmanager::ui::components::interaction::{
    AccessibleRole, FocusCoordinator, InteractionStateModel, KeyboardKey,
    MAX_ACCESSIBLE_NAME_SCALARS,
};
use devmanager::ui::preview::{
    parse_preview_args, PreviewApplication, PreviewError, PreviewPathPolicy, PreviewRequest,
};
use devmanager::ui::quality::{
    admit_collection_len, assemble_replayed_inbox, load_quality_fixture, load_quality_surface,
    request_from_catalog, CatalogInput, QualityError, VisualGate, INBOX_VIRTUALIZATION_LIMIT,
    MAX_QUALITY_CONTROLS, MAX_QUALITY_STRING_SCALARS, QUALITY_SCHEMA,
    TIMELINE_VIRTUALIZATION_LIMIT, VIRTUALIZATION_WINDOW,
};
use devmanager::ui::tokens::{contrast_ratio, theme, Density, Scale, StatusMeaning, ThemeMode};

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn quality_dir() -> PathBuf {
    workspace_root().join("tests/fixtures/ui/quality")
}

fn policy() -> PreviewPathPolicy {
    PreviewPathPolicy::for_workspace(workspace_root())
}

fn load_named_surface(
    name: &str,
    focus: &mut FocusCoordinator,
) -> devmanager::ui::quality::QualitySurface {
    load_quality_surface(quality_dir().join(name), &policy(), focus)
        .expect("quality fixture should load")
}

fn fixed_uuid_v7(tail: u8) -> [u8; 16] {
    [
        0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        tail,
    ]
}

fn require_hold(error: QualityError, needles: &[&str]) {
    let QualityError::Hold { missing } = error else {
        panic!("typed HOLD must stay Err(Hold); promotion to Ok/other is a false green: {error}");
    };
    for needle in needles {
        assert!(
            missing.contains(needle),
            "HOLD must cite production proof {needle:?}, got {missing:?}"
        );
    }
}

fn require_missing_files(paths: &[&str]) {
    for relative in paths {
        assert!(
            !workspace_root().join(relative).exists(),
            "{relative} exists; this isolated quality slice must not treat that dependency as satisfied until Phase 5 union tests pass"
        );
    }
}

fn require_present_insufficient(paths: &[&str]) {
    let contract = load_promotion_contract();
    for relative in paths {
        let path = workspace_root().join(relative);
        assert!(
            path.exists(),
            "{relative} must exist as a present_insufficient promotion artifact"
        );
        let recorded = contract
            .gates
            .iter()
            .flat_map(|gate| &gate.artifacts)
            .any(|artifact| {
                artifact.path == *relative
                    && artifact.kind == "present_insufficient"
                    && artifact.sha256 == "LIVE"
                    && artifact.run_identity == PROMOTION_HOLD_RUN
            });
        assert!(
            recorded,
            "{relative} must stay present_insufficient LIVE under {PROMOTION_HOLD_RUN}; source presence alone cannot promote"
        );
    }
}

const PROMOTION_SCHEMA: &str = "devmanager.ui.quality.promotion/v1";
const PROMOTION_AUTHORITY: &str = "tests/fixtures/ui/phase5-promotion-contract.json";
const PROMOTION_HOLD_RUN: &str = "cargo test --test ui_quality_gates quality_phase5_promotion_contract_rejects_hold_proxies_and_disconnected_models -- --exact";
const REQUIRED_SURFACE_IDS: &[&str] = &[
    "accessibility",
    "focus_epochs",
    "keyboard_wrap",
    "virtualization",
    "scales",
    "cancellation",
    "content_states",
];
const REQUIRED_PROMOTION_GATES: &[&str] = &[
    "focus_click_through",
    "raw_terminal_preservation",
    "semantic_timeline_20k",
    "dpi_200",
    "accesskit_actions",
    "preview_pixels",
    "shutdown_5s",
    "perf_p95_idle",
];
const REQUIRED_VERIFICATION_ORDER: &[&str] = &[
    "cargo test --test ui_quality_gates quality_phase5_promotion_contract_rejects_hold_proxies_and_disconnected_models -- --exact",
    "cargo test --test ui_quality_gates -- --test-threads=1",
    "rerun exact promotion test after shell/virtual_list/renderers/ui_focus/Capture exist; must RED until measurements exist. Do not run --lib.",
];

#[derive(Debug, Deserialize)]
struct PromotionContract {
    schema: String,
    id: String,
    rule: String,
    authority: String,
    fixture_sentinel: String,
    verification_order: Vec<String>,
    forbidden_siblings: Vec<String>,
    shared_absent: Vec<String>,
    stale_screenshot_roots: Vec<String>,
    surfaces: Vec<PromotionSurface>,
    shell_union: ShellUnionPlan,
    gates: Vec<PromotionGate>,
}

#[derive(Debug, Clone, Deserialize)]
struct PromotionSurface {
    id: String,
    fixture_id: String,
    file: String,
    role: String,
    owning_test: String,
    owning_gate: String,
}

#[derive(Debug, Deserialize)]
struct ShellUnionPlan {
    delete: Vec<String>,
    port: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct PromotionGate {
    id: String,
    hold_ids: Vec<String>,
    measurement: String,
    artifacts: Vec<PromotionArtifact>,
    rejects: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct PromotionArtifact {
    path: String,
    kind: String,
    sha256: String,
    run_identity: String,
}

fn live_sha256(path: &Path) -> String {
    if path.is_file() {
        return format!("{:x}", Sha256::digest(fs::read(path).expect("hash file")));
    }
    assert!(
        path.is_dir(),
        "present_insufficient path must be a live file or directory: {}",
        path.display()
    );
    let mut entries = fs::read_dir(path)
        .expect("hash directory")
        .map(|entry| {
            entry
                .expect("directory entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .collect::<Vec<_>>();
    entries.sort();
    format!("{:x}", Sha256::digest(entries.join("\n").as_bytes()))
}

fn quality_gate_source() -> String {
    fs::read_to_string(workspace_root().join("tests/ui_quality_gates.rs")).expect("gate source")
}

#[derive(Debug, Deserialize)]
struct FixtureAuthorityView {
    id: String,
    visual_gate: FixtureVisualGateView,
}

#[derive(Debug, Deserialize)]
struct FixtureVisualGateView {
    reason: String,
    missing: Vec<String>,
}

fn require_sentinel_missing(missing: &[String], sentinel: &str) -> Result<(), String> {
    if missing != [sentinel] {
        return Err(format!(
            "missing list must be exactly [{sentinel}], not a hold-union sibling matrix"
        ));
    }
    Ok(())
}

fn parse_fixture_authority(
    path: &Path,
    sentinel: &str,
    expected_id: &str,
) -> Result<FixtureAuthorityView, String> {
    let bytes = fs::read(path).map_err(|error| error.to_string())?;
    let parsed: FixtureAuthorityView =
        serde_json::from_slice(&bytes).map_err(|error| error.to_string())?;
    if parsed.id != expected_id {
        return Err(format!(
            "fixture {} id {} does not match mapped {expected_id}",
            path.display(),
            parsed.id
        ));
    }
    require_sentinel_missing(&parsed.visual_gate.missing, sentinel)
        .map_err(|error| format!("fixture {} {error}", parsed.id))?;
    if parsed.visual_gate.reason.contains("hold-union") {
        return Err(format!(
            "fixture {} still names hold-union instead of the promotion contract",
            parsed.id
        ));
    }
    if !parsed.visual_gate.reason.contains(PROMOTION_AUTHORITY)
        || !parsed.visual_gate.reason.contains("HOLD")
        || !parsed.visual_gate.reason.contains("not PASS")
    {
        return Err(format!(
            "fixture {} must defer to {PROMOTION_AUTHORITY}",
            parsed.id
        ));
    }
    Ok(parsed)
}

fn validate_surface_matrix(
    surfaces: &[PromotionSurface],
    gates: &[PromotionGate],
    test_src: &str,
    sentinel: &str,
) -> Result<(), String> {
    admit_collection_len(surfaces.len(), MAX_QUALITY_CONTROLS, "surfaces")
        .map_err(|error| error.to_string())?;
    let gate_ids: BTreeSet<&str> = gates.iter().map(|gate| gate.id.as_str()).collect();
    let mut ids = BTreeSet::new();
    let mut files = BTreeSet::new();
    let mut owners = BTreeSet::new();
    let mut fixture_ids = BTreeSet::new();
    for surface in surfaces {
        if surface.id.trim().is_empty()
            || surface.fixture_id.trim().is_empty()
            || surface.file.trim().is_empty()
            || surface.owning_test.trim().is_empty()
            || surface.owning_gate.trim().is_empty()
        {
            return Err(format!(
                "surface mapping is missing required fields: {surface:?}"
            ));
        }
        if !ids.insert(surface.id.as_str()) {
            return Err(format!("duplicate surface id {}", surface.id));
        }
        if !files.insert(surface.file.as_str()) {
            return Err(format!("duplicate surface file {}", surface.file));
        }
        if !owners.insert(surface.owning_test.as_str()) {
            return Err(format!("duplicate owning test {}", surface.owning_test));
        }
        if !fixture_ids.insert(surface.fixture_id.as_str()) {
            return Err(format!("duplicate fixture id {}", surface.fixture_id));
        }
        if !gate_ids.contains(surface.owning_gate.as_str()) {
            return Err(format!(
                "surface {} maps to unknown owning gate {}",
                surface.id, surface.owning_gate
            ));
        }
        let fn_needle = format!("fn {}(", surface.owning_test);
        if !test_src.contains(&fn_needle) {
            return Err(format!(
                "surface {} owning test {} is missing from ui_quality_gates.rs",
                surface.id, surface.owning_test
            ));
        }
        parse_fixture_authority(
            &workspace_root().join(&surface.file),
            sentinel,
            &surface.fixture_id,
        )?;
    }
    for required in REQUIRED_SURFACE_IDS {
        if !ids.contains(required) {
            return Err(format!("missing required surface mapping {required}"));
        }
    }
    Ok(())
}

fn claims_manual_visual_hold_passed(source: &str) -> bool {
    let hold_passed = concat!("hold", " passed");
    let dpi_passed = concat!("dpi", " passed");
    let occlusion_passed = concat!("occlusion", " passed");
    let manual_dpi = concat!("manual", " dpi");
    let occlusion_hold = concat!("occlusion", " hold");
    source.lines().any(|line| {
        let lower = line.to_ascii_lowercase();
        let claims_pass = lower.contains(hold_passed)
            || lower.contains(dpi_passed)
            || lower.contains(occlusion_passed)
            || lower.contains(manual_dpi) && lower.contains("pass")
            || lower.contains(occlusion_hold) && lower.contains("pass");
        claims_pass && !lower.contains("not pass") && !lower.contains("cannot")
    })
}

fn load_promotion_contract() -> PromotionContract {
    let path = workspace_root().join(PROMOTION_AUTHORITY);
    let bytes = fs::read(&path).expect("promotion contract");
    assert!(
        bytes.len() <= MAX_QUALITY_STRING_SCALARS * 64,
        "promotion contract must stay bounded"
    );
    let contract: PromotionContract =
        serde_json::from_slice(&bytes).expect("promotion contract JSON");
    assert_eq!(contract.schema, PROMOTION_SCHEMA);
    assert_eq!(contract.id, "phase5-promotion-contract");
    assert_eq!(contract.authority, PROMOTION_AUTHORITY);
    assert_eq!(contract.fixture_sentinel, "phase5-promotion-contract");
    assert!(contract.rule.contains("HOLD is not PASS"));
    assert_eq!(
        contract.verification_order, REQUIRED_VERIFICATION_ORDER,
        "later verification order is frozen in the promotion contract"
    );
    admit_collection_len(
        contract.gates.len(),
        MAX_QUALITY_CONTROLS,
        "promotion-gates",
    )
    .expect("promotion gate count is bounded");
    admit_collection_len(
        contract.shared_absent.len(),
        MAX_QUALITY_CONTROLS,
        "shared-absent",
    )
    .expect("shared absent paths are bounded");
    admit_collection_len(
        contract.stale_screenshot_roots.len(),
        8,
        "stale-screenshot-roots",
    )
    .expect("screenshot roots are bounded");
    admit_collection_len(contract.surfaces.len(), MAX_QUALITY_CONTROLS, "surfaces")
        .expect("surface registry is bounded");
    contract
}

fn deterministic_task_id(index: u16) -> TaskId {
    let mut bytes = fixed_uuid_v7(0);
    bytes[9] = 0x21;
    bytes[14] = (index >> 8) as u8;
    bytes[15] = index as u8;
    TaskId::from_bytes(bytes).expect("task id")
}

#[test]
fn quality_accessibility_fixture_projects_named_roles_states_and_color_independent_status() {
    let mut focus = FocusCoordinator::new();
    let surface = load_named_surface("accessibility.json", &mut focus);
    assert_eq!(surface.fixture_id(), "quality-accessibility");

    let save = surface
        .control("save")
        .expect("named save control must exist");
    assert_eq!(save.accessibility.role, AccessibleRole::Button);
    assert_eq!(save.accessibility.name, "Save changes");
    assert!(!save.accessibility.disabled);
    assert!(save.interactive);

    let status = surface
        .control("host-health")
        .expect("named status control must exist");
    assert_eq!(status.accessibility.role, AccessibleRole::Status);
    assert_eq!(status.status_meaning, Some(StatusMeaning::Success));
    assert!(
        !status.accessibility.description.is_empty(),
        "status meaning must be available as screen-reader text, not color alone"
    );
    assert!(!status.interactive);

    let field = surface
        .control("prompt")
        .expect("named text field must exist");
    assert_eq!(field.accessibility.role, AccessibleRole::TextField);
    assert!(!field.accessibility.name.trim().is_empty());

    let tokens = surface.theme_tokens();
    assert_eq!(tokens.density.motion.reduced_motion_ms, 0);
    assert!(
        contrast_ratio(tokens.text.primary, tokens.surfaces.canvas) >= 4.5,
        "quality surface must keep token contrast, not invent cockpit colors"
    );
}

#[test]
fn quality_focus_epoch_uses_shared_interaction_state_model() {
    let mut focus = FocusCoordinator::new();
    let first = focus.current();
    let mut model = InteractionStateModel::default();
    model.set_focus_epoch(first);
    assert!(model.pointer_down(11, first));
    let second = focus.advance();
    assert!(model.set_focus_epoch(second));
    assert!(
        !model.pointer_up(11, first),
        "stale pointer sequences must not activate after a shared focus-epoch advance"
    );
    assert!(model.pointer_down(12, second));
    assert!(model.pointer_up(12, second));
    require_present_insufficient(&[
        "src/ui/shell.rs",
        "tests/ui_focus.rs",
        "src/ui/task_cockpit",
    ]);
}

#[test]
fn quality_keyboard_activation_uses_shared_interaction_state_model() {
    let focus = FocusCoordinator::new();
    let epoch = focus.current();
    let mut model = InteractionStateModel::default();
    model.set_focus_epoch(epoch);
    assert!(
        !model.key_activate(KeyboardKey::Enter, epoch),
        "unfocused InteractionStateModel must not activate"
    );
    assert!(model.focus());
    assert!(model.key_activate(KeyboardKey::Enter, epoch));
    assert!(model.key_activate(KeyboardKey::Space, epoch));
    assert!(!model.key_activate(KeyboardKey::Escape, epoch));
    assert!(
        !model.key_activate(KeyboardKey::Tab, epoch),
        "Tab is not an InteractionStateModel activation; cockpit wrap remains HOLD"
    );
    require_present_insufficient(&["src/ui/shell.rs", "tests/ui_focus.rs"]);
}

#[test]
fn quality_content_states_consume_canonical_empty_error_and_hold_partial() {
    let mut focus = FocusCoordinator::new();
    let surface = load_named_surface("content-states.json", &mut focus);
    let samples = surface.samples().expect("content samples");
    assert!(samples.long_text.chars().count() > MAX_ACCESSIBLE_NAME_SCALARS);
    assert!(samples.long_text.chars().count() <= MAX_QUALITY_STRING_SCALARS);
    assert!(samples.unicode.contains('界'));

    assert!(
        EmptyState::new(&samples.long_text, &samples.empty_description).is_err(),
        "canonical name bound must reject oversized titles"
    );
    let empty = EmptyState::new(&samples.empty_title, &samples.long_text)
        .expect("long text must fit the canonical description bound");
    assert_eq!(empty.accessibility().role, AccessibleRole::Region);
    assert_eq!(empty.title(), samples.empty_title);
    assert!(empty.description().contains(' '));
    assert!(!empty.rendered_payload().is_empty());

    let unicode = EmptyState::new(&samples.unicode, &samples.empty_description)
        .expect("unicode must be accepted by EmptyState");
    assert!(unicode.title().contains('界'));

    let error = surface.error_boundary().expect("error state");
    assert_eq!(error.accessibility().role, AccessibleRole::Alert);
    assert!(error.accessibility().invalid);
    assert_eq!(error.title(), samples.error_title);

    let partial = surface
        .partial_projection()
        .expect_err("no parent PartialState component");
    require_hold(partial, &["PartialState"]);
}

#[test]
fn quality_scale_contracts_cover_100_125_150_and_200_percent() {
    let mut focus = FocusCoordinator::new();
    let surface = load_named_surface("scales.json", &mut focus);
    let contracts = surface.scale_contracts();
    assert_eq!(
        contracts
            .iter()
            .map(|contract| contract.percent)
            .collect::<Vec<_>>(),
        vec![100, 125, 150, 200]
    );

    let mut previous_height = 0;
    for contract in &contracts {
        let physical = contract.physical;
        assert!(physical.control_height >= physical.icon_size + 2 * physical.control_padding);
        assert!(physical.row_height >= physical.body_line_height + 2 * physical.row_padding);
        assert!(physical.label_min_width >= physical.icon_size);
        assert!(physical.focus_ring_width >= 1);
        assert_eq!(contract.reduced_motion_ms, 0);
        assert!(physical.control_height >= previous_height);
        previous_height = physical.control_height;

        let tokens = theme(ThemeMode::Dark, Density::Comfortable, contract.scale);
        assert_eq!(tokens.density.physical(), physical);
    }
    require_hold(
        surface
            .pixel_readback()
            .expect_err("token DPI math is not PNG/200% proof"),
        &["gpui_png_readback"],
    );
    require_present_insufficient(&["scripts/native-next/Capture-UiPreviews.ps1"]);
}

#[test]
fn quality_inbox_window_replays_client_model_task_ids() {
    let mut focus = FocusCoordinator::new();
    let surface = load_named_surface("virtualization.json", &mut focus);
    let model = assemble_replayed_inbox(INBOX_VIRTUALIZATION_LIMIT).expect("replayed inbox");
    assert_eq!(model.tasks().len(), INBOX_VIRTUALIZATION_LIMIT);
    assert_eq!(
        model.last_applied_sequence(),
        INBOX_VIRTUALIZATION_LIMIT as u64
    );

    let inbox = surface
        .project_inbox_window(&model, 0)
        .expect("inbox window");
    assert_eq!(inbox.total, INBOX_VIRTUALIZATION_LIMIT);
    assert_eq!(inbox.projected_count, VIRTUALIZATION_WINDOW);
    assert!(inbox.work_units <= VIRTUALIZATION_WINDOW);
    assert!(!inbox.cancelled);
    let first_id = model.tasks().keys().next().expect("first task").to_string();
    assert_eq!(inbox.rows[0].id, first_id);
    assert!(!inbox.rows[0].id.starts_with("inbox-"));
    assert!(INBOX_VIRTUALIZATION_LIMIT <= MAX_CLIENT_MODEL_ITEMS);
    assert!(
        INBOX_VIRTUALIZATION_LIMIT / MAX_SNAPSHOT_PAGE_ITEMS as usize <= MAX_CLIENT_REPLAY_PAGES
    );
    assert!(inbox.rows.len() <= VIRTUALIZATION_WINDOW);
    assert!(
        surface.project_timeline_window(0).is_err(),
        "5k inbox replay must not promote a 20k semantic timeline"
    );
}

#[test]
fn quality_page_limits_reject_oversize_before_model_allocation() {
    assert!(PageLimits::new(0, MAX_SNAPSHOT_PAGE_ENCODED_BYTES).is_err());
    assert!(PageLimits::new(MAX_SNAPSHOT_PAGE_ITEMS, 0).is_err());
    let limits = PageLimits::new(MAX_SNAPSHOT_PAGE_ITEMS, MAX_SNAPSHOT_PAGE_ENCODED_BYTES)
        .expect("canonical page limits");
    let rejected = admit_collection_len(
        limits.max_items as usize + 1,
        limits.max_items as usize,
        "snapshot-page-items",
    )
    .expect_err("oversize page must fail before allocation");
    assert!(matches!(
        rejected,
        QualityError::CollectionBoundExceeded { .. }
    ));
    assert!(assemble_replayed_inbox(INBOX_VIRTUALIZATION_LIMIT + 1).is_err());
}

#[test]
fn quality_actions_resolve_through_the_shared_action_catalog() {
    assert!(catalog()
        .iter()
        .any(|descriptor| descriptor.id == ACTION_TASK_SHOW));
    let task_id = deterministic_task_id(1);
    let show = request_from_catalog(ACTION_TASK_SHOW, CatalogInput::TaskId(task_id))
        .expect("catalog task.show");
    assert_eq!(show, ActionRequest::TaskShow { task_id });
    assert_eq!(show.id(), ACTION_TASK_SHOW);
    assert_eq!(
        show.descriptor().argument_schema,
        ActionArgumentSchema::TaskId
    );

    assert_eq!(
        request_from_catalog(ACTION_TASK_LIST, CatalogInput::None).expect("catalog task.list"),
        ActionRequest::TaskList
    );
    assert_eq!(
        request_from_catalog(ACTION_HOST_STATUS, CatalogInput::None).expect("catalog host.status"),
        ActionRequest::HostStatus
    );
    assert_eq!(
        request_from_catalog(ACTION_HOST_ACTIONS, CatalogInput::None)
            .expect("catalog host.actions"),
        ActionRequest::HostActions
    );

    let create_args = TaskCreateArguments {
        task_id,
        environment_id: EnvironmentId::from_bytes(fixed_uuid_v7(0x10)).expect("env"),
        title: "New Task".into(),
        description: None,
        project_id: ProjectId::from_bytes(fixed_uuid_v7(0x11)).expect("project"),
        workspace: WorkspaceRef::Main,
    };
    let create = request_from_catalog(
        ACTION_TASK_CREATE,
        CatalogInput::Create(create_args.clone()),
    )
    .expect("canonical task.create request");
    assert_eq!(create, ActionRequest::TaskCreate(create_args.clone()));
    task_create_command(
        CommandId::from_bytes(fixed_uuid_v7(0x30)).expect("command"),
        ClientId::from_bytes(fixed_uuid_v7(0x31)).expect("client"),
        1_725_000_000_100,
        create_args,
    )
    .expect("canonical create factory");

    let rename_args = TaskRenameArguments {
        task_id,
        title: "Renamed Task".into(),
    };
    let rename = request_from_catalog(
        ACTION_TASK_RENAME,
        CatalogInput::Rename {
            args: rename_args.clone(),
            expected_revision: 7,
        },
    )
    .expect("canonical task.rename request");
    assert_eq!(rename, ActionRequest::TaskRename(rename_args.clone()));
    task_rename_command(
        CommandId::from_bytes(fixed_uuid_v7(0x32)).expect("command"),
        ClientId::from_bytes(fixed_uuid_v7(0x33)).expect("client"),
        1_725_000_000_100,
        7,
        rename_args,
    )
    .expect("canonical rename factory");
}

#[test]
fn quality_host_cleanup_and_shutdown_hold_without_command_bus() {
    let mut focus = FocusCoordinator::new();
    let surface = load_named_surface("cancellation.json", &mut focus);
    let hold = surface
        .bind_host_cleanup_worker(HostCleanupWorker)
        .expect_err("CommandBus host cleanup cannot run on the isolated preview surface");
    require_hold(hold, &["HostCleanupWorker", "CommandBus"]);
    let shutdown = surface
        .shutdown_evidence()
        .expect_err("private shutdown bool is not host lifecycle evidence");
    require_hold(shutdown, &["CommandBus"]);
    let host_src = fs::read_to_string(workspace_root().join("src/host/connection.rs"))
        .expect("host connection");
    assert!(
        host_src.contains("QUIT_TERMINAL_ACK_TIMEOUT") && host_src.contains("from_secs(5)"),
        "future shutdown proof is the 5s absolute quit-terminal deadline, not a quality bool"
    );
}

#[test]
fn quality_timeline_20k_holds_without_a_semantic_event_journal() {
    let mut focus = FocusCoordinator::new();
    let surface = load_named_surface("virtualization.json", &mut focus);
    let hold = surface
        .project_timeline_window(0)
        .expect_err("timeline must not synthesize fake event ids");
    require_hold(hold, &["DomainEvent", "renderer"]);
    assert_eq!(TIMELINE_VIRTUALIZATION_LIMIT, 20_000);
    let events = fs::read_to_string(workspace_root().join("src/domain/event.rs")).expect("events");
    assert!(
        !events.contains("Message {")
            && !events.contains("Question")
            && !events.contains("ToolUse"),
        "semantic timeline cannot be closed by inventing message/tool/question events"
    );
    require_present_insufficient(&["src/ui/renderers"]);
    require_missing_files(&["src/ui/virtual_list.rs", "tests/renderer_registry.rs"]);
    let quality = fs::read_to_string(workspace_root().join("src/ui/quality.rs")).expect("quality");
    assert!(
        !quality.contains("format!(\"{kind}-{index") && !quality.contains("inbox-0000"),
        "quality must not synthesize inbox-/timeline-NNNN stand-in ids"
    );
}

#[test]
fn quality_path_authority_is_preview_not_a_forked_scanner() {
    let source = fs::read_to_string(workspace_root().join("src/ui/quality.rs")).expect("quality");
    assert!(
        !source.contains("fn is_sensitive_path"),
        "quality must reuse preview path authority"
    );
    assert!(
        !source.contains("fn is_within"),
        "quality must reuse preview path membership"
    );
    assert!(
        !source.contains("fn parse_action"),
        "quality must not keep a private action matcher"
    );
    assert!(
        !source.contains("struct CancelToken"),
        "quality must not fork a private cancel token"
    );
    assert!(
        !source.contains("fn host_started(") && !source.contains("host_started: false"),
        "private host_started bool cannot count as host evidence"
    );
    assert!(
        !source.contains("fn shutdown(") && !source.contains("self.shutdown = true"),
        "private shutdown bool cannot count as host cleanup"
    );
}

#[test]
fn quality_preview_cli_and_pixel_readback_hold_without_canonical_shell() {
    require_present_insufficient(&["src/ui/shell.rs", "src/ui/task_cockpit", "src/ui/renderers"]);

    let args = [
        "--ui-preview".to_string(),
        workspace_root()
            .join("tests/fixtures/ui/component-gallery.json")
            .to_string_lossy()
            .into_owned(),
        "--output".to_string(),
        workspace_root()
            .join(".devmanager-next/evidence/phase-05/screenshots/quality-cli-hold.png")
            .to_string_lossy()
            .into_owned(),
    ];
    let request = parse_preview_args(args, &policy()).expect("gallery path is under preview root");
    let preview = PreviewApplication::load(request, &policy()).expect("preview schema can load");
    let output = workspace_root()
        .join(".devmanager-next/evidence/phase-05/screenshots/quality-cli-hold.png");
    let rendered = preview
        .render_to_output()
        .expect_err("headless PNG remains unsupported");
    assert!(matches!(
        rendered,
        PreviewError::HeadlessRenderingUnsupported
    ));
    assert!(
        !output.exists(),
        "pixel proxy must not write a PNG when readback HOLDs"
    );

    let mut focus = FocusCoordinator::new();
    let surface = load_named_surface("manifest.json", &mut focus);
    assert!(!surface.visual_gate().approved_for_pixel_inspection());
    let pixel = surface
        .pixel_readback()
        .expect_err("constant visual gate cannot approve pixels");
    require_hold(pixel, &["gpui_png_readback"]);
}

#[test]
fn quality_promotion_parser_rejects_missing_or_duplicate_surface_mappings() {
    let contract = load_promotion_contract();
    let test_src = quality_gate_source();
    validate_surface_matrix(
        &contract.surfaces,
        &contract.gates,
        &test_src,
        &contract.fixture_sentinel,
    )
    .expect("canonical contract surface matrix must parse");

    let mut missing = contract.surfaces.clone();
    missing.retain(|surface| surface.id != "keyboard_wrap");
    let missing_error = validate_surface_matrix(
        &missing,
        &contract.gates,
        &test_src,
        &contract.fixture_sentinel,
    )
    .expect_err("parser must fail when a required mapping is missing");
    assert!(
        missing_error.contains("missing required surface mapping keyboard_wrap"),
        "{missing_error}"
    );

    let mut duplicate = contract.surfaces.clone();
    duplicate.push(duplicate[1].clone());
    let duplicate_error = validate_surface_matrix(
        &duplicate,
        &contract.gates,
        &test_src,
        &contract.fixture_sentinel,
    )
    .expect_err("parser must fail on a duplicate mapping");
    assert!(duplicate_error.contains("duplicate"), "{duplicate_error}");

    let sibling = require_sentinel_missing(
        &[contract.fixture_sentinel.clone(), "inbox".to_string()],
        &contract.fixture_sentinel,
    )
    .expect_err("parser must reject a hold-union sibling missing list");
    assert!(sibling.contains("hold-union sibling matrix"), "{sibling}");
}

#[test]
fn quality_source_rejects_manual_dpi_or_occlusion_hold_claimed_as_pass() {
    let contract = load_promotion_contract();
    assert!(
        !claims_manual_visual_hold_passed(
            &fs::read_to_string(workspace_root().join(PROMOTION_AUTHORITY)).expect("contract")
        ),
        "promotion contract must keep DPI/occlusion as a typed HOLD, not PASS"
    );
    for entry in fs::read_dir(quality_dir()).expect("quality fixtures") {
        let path = entry.expect("fixture entry").path();
        if path.extension().and_then(|extension| extension.to_str()) != Some("json") {
            continue;
        }
        let source = fs::read_to_string(&path).expect("fixture");
        assert!(
            !claims_manual_visual_hold_passed(&source),
            "{} describes a DPI/occlusion HOLD as success",
            path.display()
        );
    }
    let gate_src = quality_gate_source()
        .lines()
        .filter(|line| {
            !line.contains("claims_manual_visual_hold_passed")
                && !line.contains("concat!")
                && !line.contains("quality_source_rejects")
                && !line.contains("manual_dpi")
                && !line.contains("occlusion_hold")
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        !claims_manual_visual_hold_passed(&gate_src),
        "quality gate tests must not claim a visual HOLD succeeded"
    );
    let quality = fs::read_to_string(workspace_root().join("src/ui/quality.rs")).expect("quality");
    assert!(
        !claims_manual_visual_hold_passed(&quality),
        "quality.rs must keep DPI/occlusion as a typed HOLD, not PASS"
    );

    let scales = contract
        .surfaces
        .iter()
        .find(|surface| surface.id == "scales")
        .expect("scales mapping");
    assert_eq!(scales.owning_gate, "dpi_200");
    let content = contract
        .surfaces
        .iter()
        .find(|surface| surface.id == "content_states")
        .expect("content_states mapping");
    assert_eq!(content.owning_gate, "anatomy");
    require_present_insufficient(&["scripts/native-next/Capture-UiPreviews.ps1"]);
    let mut focus = FocusCoordinator::new();
    let surface = load_named_surface("scales.json", &mut focus);
    require_hold(
        surface
            .pixel_readback()
            .expect_err("token DPI math is not pixel proof"),
        &["gpui_png_readback"],
    );
    require_hold(
        surface
            .anatomy_evidence()
            .expect_err("occlusion/anatomy remains HOLD"),
        &["desktop_mobile_anatomy"],
    );
    assert!(!surface.visual_gate().approved_for_pixel_inspection());
}

#[test]
fn quality_phase5_promotion_contract_rejects_hold_proxies_and_disconnected_models() {
    let contract = load_promotion_contract();
    validate_surface_matrix(
        &contract.surfaces,
        &contract.gates,
        &quality_gate_source(),
        &contract.fixture_sentinel,
    )
    .expect("promotion contract surface matrix must stay complete and unique");
    let shared_absent: Vec<&str> = contract.shared_absent.iter().map(String::as_str).collect();
    require_missing_files(&shared_absent);
    let forbidden: Vec<&str> = contract
        .forbidden_siblings
        .iter()
        .map(String::as_str)
        .collect();
    require_missing_files(&forbidden);
    assert!(
        !quality_dir().join("hold-union.json").exists(),
        "hold-union.json is a sibling matrix and must stay deleted"
    );

    let mut registered_surfaces = BTreeSet::new();
    for surface in &contract.surfaces {
        assert!(
            registered_surfaces.insert(surface.file.clone()),
            "duplicate surface {}",
            surface.file
        );
        assert!(
            workspace_root().join(&surface.file).exists(),
            "registered surface {} must exist",
            surface.file
        );
        assert!(
            !surface.role.contains("pass") || surface.role.contains("not"),
            "surface role {} must not claim PASS",
            surface.role
        );
    }
    let mut discovered = BTreeSet::new();
    for entry in fs::read_dir(quality_dir()).expect("quality fixtures") {
        let path = entry.expect("fixture entry").path();
        if path.extension().and_then(|extension| extension.to_str()) != Some("json") {
            continue;
        }
        let relative = format!(
            "tests/fixtures/ui/quality/{}",
            path.file_name()
                .and_then(|name| name.to_str())
                .expect("fixture name")
        );
        assert!(
            discovered.insert(relative.clone()),
            "duplicate discovered fixture {relative}"
        );
        let mut focus = FocusCoordinator::new();
        let loaded = load_quality_surface(&path, &policy(), &mut focus).expect("surface");
        let VisualGate::RequiresCanonicalShell { missing, reason } = loaded.visual_gate();
        assert_eq!(
            missing.as_slice(),
            [contract.fixture_sentinel.clone()].as_slice(),
            "{relative} must not carry a sibling missing matrix"
        );
        assert!(
            reason.contains("HOLD")
                && reason.contains("not PASS")
                && reason.contains(PROMOTION_AUTHORITY),
            "{relative} must defer to the promotion contract"
        );
        assert!(
            !reason.to_ascii_lowercase().contains(" is pass"),
            "{relative} must not describe HOLD as PASS"
        );
        assert!(
            !loaded.visual_gate().approved_for_pixel_inspection(),
            "{relative} cannot approve pixels"
        );
    }
    assert_eq!(
        discovered, registered_surfaces,
        "every quality fixture must be registered on the promotion contract and no extras may exist"
    );

    let mut focus = FocusCoordinator::new();
    let surface = load_named_surface("manifest.json", &mut focus);

    let mut contract_holds = BTreeSet::new();
    let mut seen_gates = BTreeSet::new();
    for gate in &contract.gates {
        assert!(
            seen_gates.insert(gate.id.as_str()),
            "duplicate promotion gate {}",
            gate.id
        );
        assert!(
            !gate.measurement.trim().is_empty()
                && gate.measurement.chars().count() <= MAX_QUALITY_STRING_SCALARS,
            "gate {} must cite a bounded production measurement",
            gate.id
        );
        admit_collection_len(gate.hold_ids.len(), MAX_QUALITY_CONTROLS, "gate-hold-ids")
            .expect("hold ids are bounded");
        admit_collection_len(gate.artifacts.len(), 8, "gate-artifacts").expect("artifacts bounded");
        admit_collection_len(gate.rejects.len(), 8, "gate-rejects").expect("rejects bounded");
        for reject in &gate.rejects {
            assert!(
                matches!(
                    reject.as_str(),
                    "HOLD"
                        | "synthetic fixture"
                        | "private boolean"
                        | "stale screenshot"
                        | "disconnected InteractionStateModel without shell"
                        | "disconnected ClientModel TaskCreated as terminal"
                        | "disconnected ClientModel TaskCreated replay"
                        | "inbox-NNNN stand-in ids"
                        | "token physical math as 200 percent proof"
                        | "AccessibleRole on isolated Button"
                        | "quality shutdown bool"
                        | "synthetic timing fixture"
                        | "disconnected catalog request without GPUI"
                ),
                "gate {} has an unbound reject {reject}",
                gate.id
            );
        }
        assert!(
            gate.rejects.iter().any(|item| item == "HOLD")
                && gate.rejects.iter().any(|item| item == "synthetic fixture")
                && gate.rejects.iter().any(|item| item == "private boolean"),
            "gate {} must reject HOLD, synthetic fixtures, and private booleans",
            gate.id
        );
        assert!(
            !gate.artifacts.is_empty(),
            "gate {} must cite a production artifact",
            gate.id
        );
        for artifact in &gate.artifacts {
            let path = workspace_root().join(&artifact.path);
            assert_eq!(
                artifact.run_identity, PROMOTION_HOLD_RUN,
                "gate {} artifact {} must bind the exact HOLD run identity, not a synthetic timing/pixel/census run",
                gate.id, artifact.path
            );
            match artifact.kind.as_str() {
                "absent" => {
                    assert!(
                        !path.exists(),
                        "{} is absent-proof for {}; presence would require a new promotion measurement",
                        artifact.path,
                        gate.id
                    );
                    assert_eq!(
                        artifact.sha256, "ABSENT",
                        "absent {} must hash as ABSENT, not a synthetic digest",
                        artifact.path
                    );
                }
                "present_insufficient" => {
                    assert!(
                        path.exists(),
                        "{} must exist as an insufficient stand-in for {}",
                        artifact.path,
                        gate.id
                    );
                    assert_eq!(
                        artifact.sha256, "LIVE",
                        "present {} must bind a live file hash, not a frozen fixture digest",
                        artifact.path
                    );
                    let digest = live_sha256(&path);
                    assert_eq!(digest.len(), 64, "live sha256 must be 64 hex chars");
                    assert_ne!(
                        digest, "ABSENT",
                        "live hash of {} cannot promote a HOLD",
                        artifact.path
                    );
                }
                other => panic!("gate {} has unknown artifact kind {other}", gate.id),
            }
        }
        for hold_id in &gate.hold_ids {
            assert!(
                contract_holds.insert(hold_id.as_str()),
                "hold id {hold_id} is owned by more than one gate"
            );
        }
    }
    for required in REQUIRED_PROMOTION_GATES {
        assert!(
            seen_gates.contains(required),
            "promotion contract must include special-attention gate {required}"
        );
    }
    assert!(
        !contract_holds.is_empty(),
        "promotion contract must own HOLD identities"
    );
    assert!(
        !contract.shell_union.delete.is_empty() && !contract.shell_union.port.is_empty(),
        "shell union must name delete and port paths"
    );
    assert!(
        contract
            .shell_union
            .delete
            .iter()
            .any(|path| path.ends_with("hold-union.json")),
        "shell union must keep hold-union.json on the delete list"
    );

    for root in &contract.stale_screenshot_roots {
        let dir = workspace_root().join(root);
        if dir.exists() {
            for entry in fs::read_dir(&dir).expect("screenshot root") {
                let path = entry.expect("screenshot entry").path();
                assert!(
                    path.extension().and_then(|extension| extension.to_str()) != Some("png"),
                    "stale PNG {} cannot promote preview_pixels",
                    path.display()
                );
            }
        }
    }

    let quality = fs::read_to_string(workspace_root().join("src/ui/quality.rs")).expect("quality");
    assert!(
        !quality.contains("fn host_started(") && !quality.contains("host_started: false"),
        "private host_started bool cannot promote host evidence"
    );
    assert!(
        !quality.contains("fn shutdown(") && !quality.contains("self.shutdown = true"),
        "private shutdown bool cannot promote the 5s CommandBus deadline"
    );
    assert!(
        !quality.to_ascii_lowercase().contains("accesskit"),
        "quality.rs must not claim AccessKit actions"
    );

    let events = fs::read_to_string(workspace_root().join("src/domain/event.rs")).expect("events");
    assert!(
        !events.contains("Message {")
            && !events.contains("Question")
            && !events.contains("ToolUse"),
        "raw terminal preservation cannot be closed by inventing semantic chat events"
    );
    let connection =
        fs::read_to_string(workspace_root().join("src/host/connection.rs")).expect("connection");
    assert!(
        connection.contains("QUIT_TERMINAL_ACK_TIMEOUT") && connection.contains("from_secs(5)"),
        "shutdown_5s measurement is the host 5s quit-terminal deadline"
    );

    require_hold(
        surface.host_started_evidence().expect_err("host HOLD"),
        &["host"],
    );
    require_hold(
        surface.pixel_readback().expect_err("pixel HOLD"),
        &["gpui_png_readback"],
    );
    require_hold(
        surface.shutdown_evidence().expect_err("shutdown HOLD"),
        &["CommandBus"],
    );
    require_hold(
        surface.anatomy_evidence().expect_err("anatomy HOLD"),
        &["desktop_mobile_anatomy"],
    );
    require_hold(
        surface.performance_evidence().expect_err("perf HOLD"),
        &["performance_budgets"],
    );
    require_hold(
        surface
            .project_timeline_window(0)
            .expect_err("timeline HOLD"),
        &["DomainEvent"],
    );
    assert!(
        !surface.visual_gate().approved_for_pixel_inspection(),
        "VisualGate has no success arm; pixels and stale screenshots cannot PASS"
    );

    let inbox = assemble_replayed_inbox(INBOX_VIRTUALIZATION_LIMIT).expect("connected inbox model");
    assert_eq!(inbox.tasks().len(), INBOX_VIRTUALIZATION_LIMIT);
    assert!(
        surface.project_timeline_window(0).is_err(),
        "connected ClientModel replay must not satisfy 20k semantic virtualization"
    );
    assert_eq!(TIMELINE_VIRTUALIZATION_LIMIT, 20_000);
    assert!(INBOX_VIRTUALIZATION_LIMIT <= MAX_CLIENT_MODEL_ITEMS);
}

#[test]
fn quality_performance_and_anatomy_remain_hold_under_promotion_contract() {
    let contract = load_promotion_contract();
    assert!(contract.gates.iter().any(|gate| gate.id == "anatomy"));
    assert!(contract.gates.iter().any(|gate| gate.id == "perf_p95_idle"));
    let mut focus = FocusCoordinator::new();
    let surface = load_named_surface("manifest.json", &mut focus);
    let VisualGate::RequiresCanonicalShell { missing, reason } = surface.visual_gate();
    assert_eq!(
        missing.as_slice(),
        ["phase5-promotion-contract".to_string()].as_slice()
    );
    assert!(reason.contains("HOLD") && reason.contains("not PASS"));
    assert!(
        !reason.to_ascii_lowercase().contains(" is pass"),
        "HOLD fixture must not describe itself as PASS"
    );
    require_hold(
        surface
            .anatomy_evidence()
            .expect_err("anatomy HOLD cannot promote"),
        &["desktop_mobile_anatomy"],
    );
    require_hold(
        surface
            .performance_evidence()
            .expect_err("performance HOLD cannot promote"),
        &["performance_budgets"],
    );
}

#[test]
fn quality_private_bools_cannot_produce_host_or_pixel_success() {
    let mut focus = FocusCoordinator::new();
    let surface = load_named_surface("manifest.json", &mut focus);
    let host = surface
        .host_started_evidence()
        .expect_err("stored host_started=false is not observed host evidence");
    assert!(matches!(host, QualityError::Hold { missing } if missing.contains("host")));
    let VisualGate::RequiresCanonicalShell { .. } = surface.visual_gate();
    assert!(!surface.visual_gate().approved_for_pixel_inspection());
    require_hold(
        surface
            .pixel_readback()
            .expect_err("pixel HOLD cannot promote"),
        &["gpui_png_readback"],
    );
    require_hold(
        surface
            .shutdown_evidence()
            .expect_err("shutdown HOLD cannot promote"),
        &["CommandBus"],
    );
}

#[test]
fn quality_fixture_collections_are_bounded_before_live_allocation() {
    assert!(MAX_QUALITY_CONTROLS > 0);
    assert!(admit_collection_len(0, MAX_QUALITY_CONTROLS, "controls").is_ok());
    assert!(admit_collection_len(MAX_QUALITY_CONTROLS, MAX_QUALITY_CONTROLS, "controls").is_ok());
    assert!(matches!(
        admit_collection_len(MAX_QUALITY_CONTROLS + 1, MAX_QUALITY_CONTROLS, "controls"),
        Err(QualityError::CollectionBoundExceeded { .. })
    ));
    let mut focus = FocusCoordinator::new();
    let surface = load_named_surface("accessibility.json", &mut focus);
    assert!(surface.control("save").is_some());
}

#[test]
fn quality_helpers_and_fixtures_contain_no_hard_coded_cockpit_colors() {
    let quality_source =
        fs::read_to_string(workspace_root().join("src/ui/quality.rs")).expect("quality helper");
    assert!(
        !contains_direct_color_literal(&quality_source),
        "src/ui/quality.rs must use tokens, not hex/RGB literals"
    );

    for entry in fs::read_dir(quality_dir()).expect("quality fixtures") {
        let path = entry.expect("fixture entry").path();
        if path.extension().and_then(|extension| extension.to_str()) != Some("json") {
            continue;
        }
        let source = fs::read_to_string(&path).expect("fixture source");
        assert!(
            !contains_direct_color_literal(&source),
            "{} embeds a hard-coded color",
            path.display()
        );
    }
}

#[test]
fn quality_fixtures_stay_on_the_isolated_preview_surface_and_refuse_production_paths() {
    let document = load_quality_fixture(quality_dir().join("manifest.json"), &policy())
        .expect("manifest should load");
    assert_eq!(document.schema(), QUALITY_SCHEMA);
    assert_eq!(document.surface_kind(), "isolated_preview");
    let VisualGate::RequiresCanonicalShell { missing, reason } = document.visual_gate();
    assert_eq!(
        missing.as_slice(),
        ["phase5-promotion-contract".to_string()].as_slice()
    );
    assert!(
        reason.contains(PROMOTION_AUTHORITY),
        "manifest must defer to the promotion contract, not a sibling missing list"
    );

    let preview_request = PreviewRequest::validate(
        quality_dir().join("accessibility.json"),
        workspace_root()
            .join(".devmanager-next/evidence/phase-05/screenshots/quality-accessibility.png"),
        &policy(),
    )
    .expect("quality fixtures live under the approved preview fixture root");
    let preview_error = PreviewApplication::load(preview_request, &policy())
        .expect_err("quality schema is not a silent PNG preview");
    assert!(
        preview_error.to_string().contains("schema") || preview_error.to_string().contains("root")
    );

    let production =
        Path::new(r"C:\Users\micro\AppData\Roaming\com.userfirst.devmanager\config.json");
    let refused = load_quality_fixture(production, &policy())
        .expect_err("production config must never be a quality fixture");
    assert!(matches!(
        refused,
        QualityError::SensitivePath { .. } | QualityError::OutsideQualityRoot { .. }
    ));
}

#[test]
fn quality_browser_artifact_count_uses_token_contrast_pair() {
    let source = fs::read_to_string(workspace_root().join("src/ui/task_cockpit/browser_panel.rs"))
        .expect("browser panel source");
    assert!(
        source.contains("tokens.text.muted")
            && source.contains("tokens.surfaces.canvas")
            && source.contains("ThemeTokens"),
        "artifact-count label must use ThemeTokens text_muted on canvas"
    );
    assert!(
        !source.contains("TEXT_DIM")
            && !source.contains("PANEL_BG")
            && !source.contains("crate::theme"),
        "browser panel artifact surface must not keep the legacy TEXT_DIM/PANEL_BG pair"
    );
    for mode in [ThemeMode::Dark, ThemeMode::Light] {
        let tokens = theme(mode, Density::Comfortable, Scale::Scale100);
        assert!(
            contrast_ratio(tokens.text.muted, tokens.surfaces.canvas) >= 4.5,
            "{mode:?} text_muted on canvas must keep the 4.5:1 token contrast invariant"
        );
    }
}

fn contains_direct_color_literal(source: &str) -> bool {
    source.lines().any(|line| {
        let lower = line.to_ascii_lowercase();
        if lower.contains("rgb(") || lower.contains("rgba(") {
            if let Some(start) = lower.find("rgb") {
                let arguments = lower[start..].split_once('(').map(|(_, rest)| rest);
                if arguments.is_some_and(|value| {
                    value.trim_start().starts_with("0x")
                        || value
                            .chars()
                            .next()
                            .is_some_and(|character| character.is_ascii_digit())
                }) {
                    return true;
                }
            }
        }
        let bytes = line.as_bytes();
        for index in 0..bytes.len().saturating_sub(1) {
            let has_hash = bytes[index] == b'#';
            let has_hex_prefix = bytes[index] == b'0' && matches!(bytes[index + 1], b'x' | b'X');
            if !has_hash && !has_hex_prefix {
                continue;
            }
            let start = if has_hash { index + 1 } else { index + 2 };
            let mut end = start;
            while end < bytes.len() && bytes[end].is_ascii_hexdigit() {
                end += 1;
            }
            if matches!(end - start, 6 | 8) {
                return true;
            }
        }
        false
    })
}
