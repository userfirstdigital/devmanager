use devmanager::client::{ClientModel, ClientModelBuilder};
use devmanager::domain::id::{EnvironmentId, ProjectId, SnapshotId, TaskId};
use devmanager::domain::snapshot::{SnapshotItem, SnapshotPage, SnapshotSection};
use devmanager::domain::task::{
    ReviewReadiness, TaskActivity, TaskAssignment, TaskAttention, TaskConnectivity, TaskFacts,
    TaskLifecycle, WorkspaceRef,
};
use devmanager::ui::actions::{KeyboardShortcut, ShortcutKey};
use devmanager::ui::components::{ActionRequest, ActivationSource};
use devmanager::ui::native_shell::{
    isolated_dev_profile, AccessibilityTree, NativeHeaderAttachment, NativeHostState,
    NativeInteraction, NativeShell, NativeShellError, TerminalDockState,
};
use devmanager::ui::shell::{NavigationResult, PointerButton, TerminalPressRejection};
use devmanager::ui::task_cockpit::inbox::{
    InboxPresentationWidth, InboxRenderItem, MAX_TASK_SOURCE_IDS,
};
use devmanager::ui::task_cockpit::TaskList;
use devmanager::ui::tokens::{Density, RuntimePreferencesSnapshot, Scale, ThemeMode};
use gpui::AppContext;
use std::sync::Arc;
use tempfile::tempdir;

fn task_id(tail: u8) -> TaskId {
    let mut bytes = [
        0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00,
    ];
    bytes[15] = tail;
    TaskId::from_bytes(bytes).expect("fixed UUIDv7 task id")
}

fn task_id_index(index: usize) -> TaskId {
    let mut bytes = [
        0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00,
    ];
    let encoded = (index as u64).to_be_bytes();
    bytes[9..16].copy_from_slice(&encoded[1..]);
    TaskId::from_bytes(bytes).expect("unique UUIDv7 task id")
}

fn model_with_tasks(ids: &[TaskId]) -> ClientModel {
    let snapshot_id = SnapshotId::from_bytes([
        0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x20,
    ])
    .expect("snapshot id");
    let mut builder = ClientModelBuilder::new();
    builder
        .ingest_page(SnapshotPage {
            snapshot_id,
            through_sequence: 7,
            section: SnapshotSection::Tasks,
            after_item: None,
            items: ids
                .iter()
                .enumerate()
                .map(|(ordinal, id)| {
                    SnapshotItem::Task(devmanager::domain::snapshot::TaskSnapshotItem {
                        task: TaskFacts {
                            id: *id,
                            environment_id: EnvironmentId::from_bytes([
                                0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00,
                                0x00, 0x00, 0x00, 0x00, 0x10,
                            ])
                            .expect("environment id"),
                            title: format!("Task {ordinal}"),
                            description: None,
                            project_id: ProjectId::from_bytes([
                                0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00,
                                0x00, 0x00, 0x00, 0x00, 0x11,
                            ])
                            .expect("project id"),
                            workspace: WorkspaceRef::Main,
                            assignment: TaskAssignment::LocalOwner,
                            lifecycle: TaskLifecycle::Open,
                            action_epoch: 3,
                            revision: 4,
                            created_at_ms: 1_725_000_000_000 + ordinal as i64,
                        },
                        connectivity: TaskConnectivity::Connected,
                        attention: TaskAttention::None,
                        activity: TaskActivity::Idle,
                        review_readiness: ReviewReadiness::NotReady,
                        primary_agent_id: None,
                    })
                })
                .collect(),
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
                through_sequence: 7,
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
fn isolated_native_profile_rejects_default_and_foreign_profile_inputs() {
    let workspace = tempdir().expect("workspace tempdir");
    let profile = isolated_dev_profile(workspace.path()).expect("isolated profile");

    let canonical_workspace = std::fs::canonicalize(workspace.path()).expect("canonical workspace");
    assert!(profile.root().starts_with(canonical_workspace));
    assert!(!profile
        .root()
        .to_string_lossy()
        .contains("com.userfirst.devmanager"));
    assert!(matches!(
        isolated_dev_profile(profile.root()),
        Err(NativeShellError::WorkspaceMustBeRoot { .. })
    ));
}

#[test]
fn native_interaction_captures_focus_task_generation_and_stops_click_through() {
    let first = task_id(1);
    let second = task_id(2);
    let mut interaction = NativeInteraction::new(Some(first));
    let task_list = TaskList::empty();

    let rejected = interaction.navigation_mouse_down(second, &task_list);
    assert!(rejected.consumed);
    assert!(rejected.propagation_stopped);
    assert_eq!(rejected.task_id, second);
    assert!(rejected.request_generation > 0);
    assert_eq!(
        rejected.navigation,
        NavigationResult::Rejected {
            reason: devmanager::ui::shell::NavigationRejection::TaskNotInInbox,
        }
    );

    let terminal = interaction.terminal_mouse_down(7, second, PointerButton::Primary, Some(first));
    assert!(terminal.consumed);
    assert!(terminal.propagation_stopped);
    assert_eq!(terminal.task_id, second);
    assert_eq!(
        terminal.capture,
        Err(TerminalPressRejection::TaskNotSelected)
    );
    assert_ne!(rejected.focus_epoch, terminal.focus_epoch);
}

#[test]
fn native_semantic_tree_and_virtual_rows_are_bounded() {
    let tree = AccessibilityTree::for_task_list(&TaskList::empty(), None);
    assert_eq!(tree.root().name(), "Task Cockpit");
    assert!(tree
        .root()
        .children()
        .iter()
        .any(|node| node.name() == "Task inbox"));
    assert!(tree
        .nodes()
        .iter()
        .all(|node| !node.name().is_empty() && !node.description().is_empty()));
    assert!(tree.rendered_task_count() <= 104);

    let dock = TerminalDockState::unavailable();
    assert!(!dock.is_live());
    assert!(dock
        .message()
        .contains("src/terminal/view.rs::render_terminal_surface"));
}

struct NativeGpuiSmokeReport {
    root_constructed: bool,
    semantic_nodes: usize,
    rendered_task_rows: usize,
    host_profile: std::path::PathBuf,
    profile_root: std::path::PathBuf,
    host_state: NativeHostState,
    root_focusable: bool,
    setup_canvas: bool,
    header_projection: bool,
    model_sequence: Option<u64>,
    model_rendered: usize,
    preferences: RuntimePreferencesSnapshot,
    platform_bridge: bool,
    platform_nodes: usize,
    platform_roles: Vec<accesskit::Role>,
    platform_focus_is_root: bool,
}

fn native_gpui_smoke_report() -> NativeGpuiSmokeReport {
    let workspace = tempdir().expect("workspace tempdir");
    let profile = isolated_dev_profile(workspace.path()).expect("isolated profile");
    let model = Arc::new(model_with_tasks(&[task_id(21), task_id(22)]));
    let report_slot = std::rc::Rc::new(std::cell::RefCell::new(None));
    let report_slot_for_app = std::rc::Rc::clone(&report_slot);
    gpui::Application::headless().run(move |cx| {
        devmanager::ui::init(cx);
        let first = cx.new(|cx| NativeShell::new_for_headless(profile.clone(), cx));
        let first_report = first.update(cx, |shell, _cx| {
            let platform_tree = shell.platform_accessibility_tree_for_test();
            (
                shell.accessibility_tree().nodes().len(),
                shell.rendered_task_count(),
                shell.host_connection().profile_root().to_path_buf(),
                shell.profile().root().to_path_buf(),
                shell.host_state().clone(),
                shell
                    .accessibility_tree()
                    .gpui_nodes()
                    .iter()
                    .any(|node| node.element_id == "native-shell-root" && node.focusable),
                {
                    let nodes = shell.accessibility_tree().gpui_nodes();
                    nodes
                        .iter()
                        .any(|node| node.element_id == "native-header-settings")
                        && !nodes
                            .iter()
                            .any(|node| node.element_id == "native-task-inbox")
                        && !nodes
                            .iter()
                            .any(|node| node.element_id == "native-setup-add-project")
                },
                shell.platform_accessibility_available(),
                shell.platform_accessibility_node_count(),
                platform_tree
                    .nodes
                    .iter()
                    .map(|(_, node)| node.role())
                    .collect::<Vec<_>>(),
                platform_tree.focus == accesskit::NodeId::from(0),
            )
        });
        drop(first);

        let header = cx.new(|cx| NativeShell::new_for_headless(profile.clone(), cx));
        let header_projection = header.update(cx, |shell, _cx| {
            assert!(matches!(
                shell.header_attachment(),
                NativeHeaderAttachment::Unavailable { .. }
            ));
            shell.attach_header_projection(NativeHeaderAttachment::projection(
                "Task Cockpit",
                "Connected",
                "Remote unavailable",
                "Quota unavailable",
            ));
            matches!(
                shell.header_attachment(),
                NativeHeaderAttachment::Projection { .. }
            )
        });
        drop(header);

        let projected = cx.new(|cx| NativeShell::new_for_headless(profile.clone(), cx));
        let model_report = projected.update(cx, |shell, _cx| {
            shell
                .apply_client_model(Arc::clone(&model))
                .expect("client model projection");
            (
                shell
                    .client_model_snapshot()
                    .map(|model| model.last_applied_sequence()),
                shell.rendered_task_count(),
            )
        });
        drop(projected);

        let preferences_shell = cx.new(|cx| NativeShell::new_for_headless(profile, cx));
        let preferences = preferences_shell.update(cx, |shell, _cx| {
            let next = RuntimePreferencesSnapshot::new(
                ThemeMode::Light,
                Density::Compact,
                Scale::Scale150,
            );
            shell.queue_preferences_for_test(next);
            shell.controller_tick_for_test(32);
            shell.preferences()
        });
        drop(preferences_shell);

        *report_slot_for_app.borrow_mut() = Some(NativeGpuiSmokeReport {
            root_constructed: true,
            semantic_nodes: first_report.0,
            rendered_task_rows: first_report.1,
            host_profile: first_report.2,
            profile_root: first_report.3,
            host_state: first_report.4,
            root_focusable: first_report.5,
            setup_canvas: first_report.6,
            header_projection,
            model_sequence: model_report.0,
            model_rendered: model_report.1,
            preferences,
            platform_bridge: first_report.7,
            platform_nodes: first_report.8,
            platform_roles: first_report.9,
            platform_focus_is_root: first_report.10,
        });
        cx.quit();
    });
    let report = report_slot
        .borrow_mut()
        .take()
        .expect("native GPUI smoke report");
    report
}

#[test]
fn native_gpui_smokes_share_one_headless_lifetime_authority() {
    let report = native_gpui_smoke_report();
    assert!(report.root_constructed);
    assert!(report.semantic_nodes > 0);
    assert!(report.rendered_task_rows <= 104);
    assert_eq!(report.host_profile, report.profile_root);
    assert_eq!(report.host_state, NativeHostState::Disconnected);
    assert!(report.root_focusable);
    assert!(report.setup_canvas);
    assert!(report.header_projection);
    assert_eq!(report.model_sequence, Some(7));
    assert_eq!(report.model_rendered, 2);
    assert_eq!(
        report.preferences,
        RuntimePreferencesSnapshot::new(ThemeMode::Light, Density::Compact, Scale::Scale150)
    );
    assert!(!report.platform_bridge);
    assert!(report.platform_nodes >= report.semantic_nodes);
    assert!(report.platform_roles.contains(&accesskit::Role::Region));
    assert!(report.platform_roles.contains(&accesskit::Role::Status));
    assert!(report.platform_focus_is_root);
}

#[test]
fn native_shell_exposes_no_external_runtime_injection_authority() {
    let source = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/src/ui/native_shell.rs"
    ))
    .expect("native shell source");
    for forbidden in [
        "pub trait NativeHostRuntimePort",
        "pub struct NativeHostRuntimeStub",
        "pub struct NativeHostRuntimeStubHandle",
        "pub enum NativeHostRuntimeAttachment",
        "pub fn new_with_host_runtime_port",
        "pub fn attach_host_runtime_port",
        "pub fn connect_blocking",
        "pub fn connect(profile:",
        "pub fn new(client: HostClient)",
        "pub struct NativeHostClientRuntime",
        "pub fn run_native_shell_with_runtime",
        "pub fn new_with_host_runtime(",
        "pub fn new_with_host_runtime_and_preferences(",
        "pub fn attach_host_runtime(",
        "error: Some(resync_error)",
        "error: Some(error),",
    ] {
        assert!(
            !source.contains(forbidden),
            "production native shell must not expose {forbidden}"
        );
    }
}

#[test]
fn virtual_task_source_rejects_cap_plus_one_before_duplicate_set_allocation() {
    let source = (0..=MAX_TASK_SOURCE_IDS)
        .map(task_id_index)
        .collect::<Vec<_>>();
    let overflow = TaskList::from_virtual_task_ids(source).expect_err("source cap");
    assert_eq!(overflow.limit, MAX_TASK_SOURCE_IDS);
    assert_eq!(overflow.retained_count, 0);
}

#[test]
fn keyboard_dispatch_commits_only_the_captured_action_and_epoch() {
    let mut interaction = NativeInteraction::new(None);
    let model = devmanager::ui::actions::KeyboardModel::default();
    let (focus_epoch, request_generation, action) = interaction
        .keyboard(&model, KeyboardShortcut::ctrl(ShortcutKey::Character('k')))
        .expect("ctrl-k should resolve to the palette action");

    assert!(interaction.commit_keyboard_action(focus_epoch, request_generation, action));
    assert!(interaction.keyboard_state().palette_open);
    assert!(!interaction.commit_keyboard_action(focus_epoch, request_generation, action));
    let (new_focus_epoch, new_request_generation, new_action) = interaction
        .keyboard(&model, KeyboardShortcut::escape())
        .expect("escape should resolve to dismiss");
    assert!(!interaction.commit_keyboard_action(focus_epoch, request_generation, action));
    assert!(interaction.commit_keyboard_action(
        new_focus_epoch,
        new_request_generation,
        new_action
    ));
}

#[test]
fn pointer_capture_invalidates_a_pending_keyboard_intent() {
    let mut interaction = NativeInteraction::new(None);
    let model = devmanager::ui::actions::KeyboardModel::default();
    let (focus_epoch, request_generation, action) = interaction
        .keyboard(&model, KeyboardShortcut::ctrl(ShortcutKey::Character('k')))
        .expect("ctrl-k should resolve to the palette action");

    let navigation = interaction.navigation_mouse_down(task_id(8), &TaskList::empty());
    assert!(navigation.propagation_stopped);
    assert!(!interaction.commit_keyboard_action(focus_epoch, request_generation, action));
}

#[test]
fn native_action_dispatch_is_selection_and_interaction_fenced() {
    let selected = task_id(3);
    let foreign = task_id(4);
    let mut interaction = NativeInteraction::new(Some(selected));

    assert!(interaction
        .action(ActionRequest::TaskShow { task_id: foreign })
        .is_none());
    assert!(interaction
        .action(ActionRequest::TaskRename(
            devmanager::client::action::TaskRenameArguments {
                task_id: foreign,
                title: "forged rename".to_string(),
            },
        ))
        .is_none());

    interaction.set_disabled(true);
    assert!(interaction.action(ActionRequest::TaskList).is_none());

    interaction.set_disabled(false);
    let pointer_action = interaction
        .action_from_source(
            ActionRequest::HostStatus,
            ActivationSource::Pointer { pointer_id: 17 },
        )
        .expect("enabled host action should produce one typed event");
    assert!(matches!(
        pointer_action.event.source,
        ActivationSource::Pointer { pointer_id: 17 }
    ));
    assert!(matches!(
        pointer_action.command,
        devmanager::ui::native_shell::NativeHostCommand::Hold {
            action_id: "host.status",
            ..
        }
    ));

    let show = interaction
        .action(ActionRequest::TaskShow { task_id: selected })
        .expect("selected task show should remain dispatchable without a mutation revision");
    assert!(interaction.accepts_action_record(&show));
    assert!(matches!(
        show.command,
        devmanager::ui::native_shell::NativeHostCommand::Hold {
            action_id: "task.show",
            ..
        }
    ));
}

#[test]
fn native_mutating_action_captures_model_revision_and_epochs() {
    let selected = task_id(31);
    let mut interaction = NativeInteraction::new(Some(selected));
    interaction.set_client_model(Some(Arc::new(model_with_tasks(&[selected]))));
    let record = interaction
        .action(ActionRequest::TaskRename(
            devmanager::client::action::TaskRenameArguments {
                task_id: selected,
                title: "captured rename".to_string(),
            },
        ))
        .expect("model-backed rename should be accepted");

    assert_eq!(record.task_id, Some(selected));
    assert_eq!(record.expected_task_revision, Some(4));
    assert_eq!(record.captured_task_action_epoch, Some(3));
    assert!(record.action_epoch > 0);
    assert!(record.disabled_reason.is_none());
    assert!(matches!(record.capability, None));
    assert!(matches!(
        &record.command,
        devmanager::ui::native_shell::NativeHostCommand::TaskRename {
            command_id: _,
            issued_at_ms: _,
            expected_task_revision: 4,
            ..
        }
    ));

    let retry = record.clone();
    let (command_id, issued_at_ms, captured_task_id) = match &record.command {
        devmanager::ui::native_shell::NativeHostCommand::TaskRename {
            command_id,
            issued_at_ms,
            arguments,
            ..
        } => (*command_id, *issued_at_ms, arguments.task_id),
        _ => panic!("rename must carry a typed retry identity"),
    };
    match &retry.command {
        devmanager::ui::native_shell::NativeHostCommand::TaskRename {
            command_id: retry_command_id,
            issued_at_ms: retry_issued_at_ms,
            arguments,
            ..
        } => {
            assert_eq!(
                (*retry_command_id, *retry_issued_at_ms, arguments.task_id),
                (command_id, issued_at_ms, captured_task_id)
            );
        }
        _ => panic!("retry must preserve the captured command identity"),
    }

    let create_task = task_id(35);
    let create = interaction
        .action(ActionRequest::TaskCreate(
            devmanager::client::action::TaskCreateArguments {
                task_id: create_task,
                environment_id: EnvironmentId::new(),
                title: "captured create".to_string(),
                description: None,
                project_id: ProjectId::new(),
                workspace: WorkspaceRef::Main,
            },
        ))
        .expect("task create should capture a retry identity");
    match &create.command {
        devmanager::ui::native_shell::NativeHostCommand::TaskCreate {
            arguments,
            command_id,
            issued_at_ms,
        } => {
            assert_eq!(arguments.task_id, create_task);
            assert_ne!(*command_id, devmanager::domain::id::CommandId::default());
            assert!(*issued_at_ms > 0);
        }
        _ => panic!("create must carry a typed retry identity"),
    }

    let newer = interaction
        .action(ActionRequest::HostStatus)
        .expect("new action should be accepted");
    assert!(!interaction.accepts_action_record(&record));
    assert!(interaction.accepts_action_record(&newer));
}

#[test]
fn native_action_rejects_foreign_connection_and_runtime_epochs() {
    let selected = task_id(32);
    let mut interaction = NativeInteraction::new(Some(selected));
    let record = interaction
        .action(ActionRequest::HostStatus)
        .expect("host status action should be captured");

    interaction.set_connection_epoch(7);
    assert!(!interaction.accepts_action_record(&record));

    let replacement = interaction
        .action(ActionRequest::HostStatus)
        .expect("new host status action should be captured");
    interaction.set_runtime_generation(4);
    assert!(!interaction.accepts_action_record(&replacement));
}

#[test]
fn native_pointer_release_requires_the_captured_pointer_and_button() {
    let selected = task_id(33);
    let mut interaction = NativeInteraction::new(Some(selected));
    let down = interaction.terminal_mouse_down(7, selected, PointerButton::Primary, Some(selected));
    assert!(down.capture.is_ok());

    let wrong_button = interaction.terminal_mouse_up_for(7, PointerButton::Secondary);
    assert_eq!(
        wrong_button.release,
        devmanager::ui::shell::TerminalRelease::Rejected(
            devmanager::ui::shell::ReleaseRejection::MismatchedOwner
        )
    );
    let release = interaction.terminal_mouse_up_for(7, PointerButton::Primary);
    assert_eq!(
        release.release,
        devmanager::ui::shell::TerminalRelease::Authorized
    );
}

#[test]
fn native_pointer_release_rejects_unmapped_button_without_releasing_capture() {
    let selected = task_id(34);
    let mut interaction = NativeInteraction::new(Some(selected));
    let down = interaction.terminal_mouse_down(8, selected, PointerButton::Primary, Some(selected));
    assert!(down.capture.is_ok());

    let unsupported = interaction.terminal_mouse_up_unmapped(8);
    assert_eq!(
        unsupported.release,
        devmanager::ui::shell::TerminalRelease::Rejected(
            devmanager::ui::shell::ReleaseRejection::MismatchedOwner
        )
    );

    let release = interaction.terminal_mouse_up_for(8, PointerButton::Primary);
    assert_eq!(
        release.release,
        devmanager::ui::shell::TerminalRelease::Authorized
    );
}

#[test]
fn isolated_profile_exposes_one_explicit_native_host_client_config() {
    let workspace = tempdir().expect("workspace tempdir");
    let profile = isolated_dev_profile(workspace.path()).expect("isolated profile");
    let config = profile.host_client_config();

    assert_ne!(profile.named_profile(), "native-next-dev");
    assert!(profile.named_profile().starts_with("native-next-"));
    assert_eq!(config.named_profile, profile.named_profile());
    assert!(config.client_build.starts_with("devmanager/"));
    assert!(!config.client_build.contains("devmanager-next"));
}

#[test]
fn native_host_launch_is_pinned_and_has_no_path_fallback() {
    let workspace = tempdir().expect("workspace tempdir");
    let profile = isolated_dev_profile(workspace.path()).expect("isolated profile");
    assert_eq!(
        profile.host_client_config().named_profile,
        profile.named_profile()
    );
    assert!(!profile
        .host_connection()
        .endpoint()
        .contains("native-next-dev"));
    let source = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/src/ui/native_shell.rs"
    ))
    .expect("native shell source");
    assert!(source.contains("current_exe"));
    assert!(!source.contains("PathBuf::from(if cfg!(windows)"));
    assert!(source.contains("try_attach_existing_host"));
    assert!(source.contains("DetachOnClientClose"));
    assert!(source.contains("for_production"));
    assert!(source.contains("sanitize_spawned_host_environment"));
    assert!(source.contains("authorize_full_host_quit"));
    assert!(source.contains("return Err(IpcError::Timeout)"));
    assert!(!source.contains("\"devmanager-next/"));
}

#[test]
fn virtual_shell_uses_full_source_count_and_stable_task_keys() {
    let source = (0..100_000).map(task_id_index).collect::<Vec<_>>();
    let task_list = TaskList::from_virtual_task_ids(source).expect("bounded virtual source");
    assert_eq!(task_list.total_count(), 100_000);
    assert!(task_list.rendered_task_ids().len() <= 104);
    assert_ne!(task_list.stable_key_for(0), task_list.stable_key_for(1));
    assert!(task_list.uses_gpui_uniform_list());
}

#[test]
fn native_shell_projects_typed_inbox_and_header_from_client_model() {
    let first = task_id(21);
    let second = task_id(22);
    let model = Arc::new(model_with_tasks(&[first, second]));
    let workspace = tempdir().expect("workspace tempdir");
    let profile = isolated_dev_profile(workspace.path()).expect("isolated profile");
    let report_slot = std::rc::Rc::new(std::cell::RefCell::new(None));
    let report_slot_for_app = std::rc::Rc::clone(&report_slot);
    gpui::Application::headless().run(move |cx| {
        devmanager::ui::init(cx);
        let entity = cx.new(|cx| NativeShell::new_for_headless(profile, cx));
        let report = entity.update(cx, |shell, _cx| {
            shell
                .apply_client_model(Arc::clone(&model))
                .expect("client model projection");
            let before = matches!(
                shell.header_attachment(),
                NativeHeaderAttachment::Unavailable { .. }
            );
            let selected = shell.select_projected_task(first);
            assert!(selected.consumed);
            assert!(selected.propagation_stopped);
            let inbox = shell.inbox_render_model(InboxPresentationWidth::Regular);
            let titles = inbox
                .items
                .iter()
                .filter_map(|item| match item {
                    InboxRenderItem::Row(row) => Some(row.title.clone()),
                    _ => None,
                })
                .collect::<Vec<_>>();
            let header_title = match shell.header_attachment() {
                NativeHeaderAttachment::Projection { title, .. } => title.clone(),
                NativeHeaderAttachment::Unavailable { reason } => {
                    format!("unavailable:{reason}")
                }
            };
            (
                before,
                header_title,
                titles,
                shell.cockpit().selected_task(),
            )
        });
        *report_slot_for_app.borrow_mut() = Some(report);
        drop(entity);
        cx.quit();
    });
    let (header_was_unavailable, header_title, titles, selected) = report_slot
        .borrow_mut()
        .take()
        .expect("typed cockpit report");
    assert!(header_was_unavailable);
    assert_eq!(header_title, "Task 0");
    assert!(titles.iter().any(|title| title == "Task 0"));
    assert!(titles.iter().any(|title| title == "Task 1"));
    assert_eq!(selected, Some(first));
}

#[test]
fn platform_accessibility_tree_exposes_the_focused_task_node() {
    let task = task_id(12);
    let task_list = TaskList::from_virtual_task_ids(vec![task]).expect("bounded task source");
    let tree = AccessibilityTree::for_task_list(&task_list, Some(task));
    let update = tree.platform_update_for_test();

    assert_ne!(update.focus, accesskit::NodeId::from(0));
    let focused = update
        .nodes
        .iter()
        .find(|(node_id, _)| *node_id == update.focus)
        .map(|(_, node)| node)
        .expect("focused platform node");
    assert_eq!(focused.is_selected(), Some(true));
    assert_eq!(focused.label(), Some(format!("Task {task}").as_str()));
    assert!(focused.supports_action(accesskit::Action::Click));
    assert!(focused.supports_action(accesskit::Action::Focus));
}
