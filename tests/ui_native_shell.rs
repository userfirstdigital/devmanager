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
    headless_render_smoke, isolated_dev_profile, AccessibilityTree, NativeHeaderAttachment,
    NativeHostLaunchSpec, NativeHostProjection, NativeHostProjectionKind, NativeHostRuntimeStub,
    NativeHostState, NativeInteraction, NativeShell, NativeShellError, TerminalDockState,
};
use devmanager::ui::shell::{NavigationResult, PointerButton, TerminalPressRejection};
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

#[test]
fn native_headless_render_smoke_constructs_the_real_gpui_shell() {
    let workspace = tempdir().expect("workspace tempdir");
    let report = headless_render_smoke(workspace.path()).expect("headless native shell render");

    assert!(report.root_constructed);
    assert!(report.semantic_nodes > 0);
    assert!(report.rendered_task_rows <= 104);
    assert_eq!(report.host_profile, report.profile_root);
    assert_eq!(report.host_state, NativeHostState::Disconnected);
    assert!(report
        .gpui_accessibility_nodes
        .iter()
        .any(|node| node.element_id == "native-shell-root" && node.focusable));
    assert!(report
        .gpui_accessibility_nodes
        .iter()
        .any(|node| node.element_id == "native-task-inbox" && node.label == "Task inbox"));
}

#[test]
fn native_shell_header_uses_typed_attachment_and_explicit_unavailable_state() {
    let workspace = tempdir().expect("workspace tempdir");
    let profile = isolated_dev_profile(workspace.path()).expect("isolated profile");
    let report_slot = std::rc::Rc::new(std::cell::RefCell::new(None));
    let report_slot_for_app = std::rc::Rc::clone(&report_slot);
    gpui::Application::headless().run(move |cx| {
        devmanager::ui::init(cx);
        let entity = cx.new(|cx| NativeShell::new_for_headless(profile, cx));
        let result = entity.update(cx, |shell, _cx| {
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
            shell.header_attachment().clone()
        });
        *report_slot_for_app.borrow_mut() = Some(result);
        drop(entity);
        cx.quit();
    });
    assert!(matches!(
        report_slot.borrow_mut().take().expect("header attachment"),
        NativeHeaderAttachment::Projection { .. }
    ));
}

#[test]
fn native_shell_accepts_one_injected_runtime_seam_without_opening_a_second_client() {
    let workspace = tempdir().expect("workspace tempdir");
    let profile = isolated_dev_profile(workspace.path()).expect("isolated profile");
    let fake = NativeHostRuntimeStub::new(
        "phase2-test://isolated",
        NativeHostState::Connected {
            endpoint: "phase2-test://isolated".to_string(),
        },
    );
    let report_slot = std::rc::Rc::new(std::cell::RefCell::new(None));
    let report_slot_for_app = std::rc::Rc::clone(&report_slot);
    gpui::Application::headless().run(move |cx| {
        devmanager::ui::init(cx);
        let entity = cx.new(|cx| {
            NativeShell::new_with_host_runtime_port(
                profile,
                Box::new(fake),
                RuntimePreferencesSnapshot::new(
                    ThemeMode::Dark,
                    Density::Comfortable,
                    Scale::Scale100,
                ),
                cx,
            )
        });
        let result = entity.update(cx, |shell, _cx| {
            (
                shell.host_endpoint().to_string(),
                shell.host_state().clone(),
                shell.host_runtime().is_none(),
            )
        });
        *report_slot_for_app.borrow_mut() = Some(result);
        drop(entity);
        cx.quit();
    });
    let (endpoint, state, concrete_runtime_absent) = report_slot
        .borrow_mut()
        .take()
        .expect("injected shell report");
    assert_eq!(endpoint, "phase2-test://isolated");
    assert!(matches!(state, NativeHostState::Connected { .. }));
    assert!(concrete_runtime_absent);
}

#[test]
fn injected_host_projection_drain_is_bounded_and_keeps_live_kinds_typed() {
    use devmanager::ui::native_shell::{NativeHostProjectionKind, NativeHostRuntimePort};
    let mut stub = NativeHostRuntimeStub::new(
        "phase2-test://isolated",
        NativeHostState::Connected {
            endpoint: "phase2-test://isolated".to_string(),
        },
    );
    for kind in [
        NativeHostProjectionKind::Snapshot,
        NativeHostProjectionKind::Replay,
        NativeHostProjectionKind::Live,
    ] {
        for _ in 0..40 {
            stub.push_projection(kind);
        }
    }
    let first = stub.drain_ready(32);
    let second = stub.drain_ready(32);
    assert_eq!(first.len(), 32);
    assert_eq!(second.len(), 32);
    assert!(first.iter().chain(second.iter()).all(|kind| matches!(
        kind,
        NativeHostProjectionKind::Snapshot
            | NativeHostProjectionKind::Replay
            | NativeHostProjectionKind::Live
    )));
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
        record.command,
        Some(
            devmanager::ui::native_shell::NativeHostCommand::TaskRename {
                expected_task_revision: 4,
                ..
            }
        )
    ));

    let newer = interaction
        .action(ActionRequest::HostStatus)
        .expect("new action should be accepted");
    assert!(!interaction.accepts_action_record(&record));
    assert!(interaction.accepts_action_record(&newer));
}

#[test]
fn isolated_profile_exposes_one_explicit_native_host_client_config() {
    let workspace = tempdir().expect("workspace tempdir");
    let profile = isolated_dev_profile(workspace.path()).expect("isolated profile");
    let config = profile.host_client_config();

    assert_eq!(profile.named_profile(), "native-next-dev");
    assert_eq!(config.named_profile, profile.named_profile());
    assert!(config.client_build.starts_with("devmanager-next/"));
}

#[test]
fn native_host_launch_spec_is_explicitly_isolated_and_single_owner() {
    let workspace = tempdir().expect("workspace tempdir");
    let profile = isolated_dev_profile(workspace.path()).expect("isolated profile");
    let spec = NativeHostLaunchSpec::for_profile(&profile, 42).expect("launch spec");

    assert_eq!(spec.profile, "native-next-dev");
    assert_eq!(spec.config_base, profile.root());
    assert_eq!(spec.parent_pid, 42);
    assert!(spec
        .arguments()
        .windows(2)
        .any(|pair| pair == ["--profile", "native-next-dev"]));
    assert!(!spec.arguments().iter().any(|arg| arg == "production"));
}

#[test]
fn controller_tick_consumes_a_fenced_action_once_and_applies_projection() {
    let workspace = tempdir().expect("workspace tempdir");
    let profile = isolated_dev_profile(workspace.path()).expect("isolated profile");
    let mut fake = NativeHostRuntimeStub::new(
        "phase2-test://isolated",
        NativeHostState::Connected {
            endpoint: "phase2-test://isolated".to_string(),
        },
    );
    fake.push_projection_message(NativeHostProjection::kind(NativeHostProjectionKind::Replay));
    let fake_handle = fake.handle();
    let report_slot = std::rc::Rc::new(std::cell::RefCell::new(None));
    let report_slot_for_app = std::rc::Rc::clone(&report_slot);
    gpui::Application::headless().run(move |cx| {
        devmanager::ui::init(cx);
        let entity = cx.new(|cx| {
            NativeShell::new_with_host_runtime_port(
                profile,
                Box::new(fake),
                RuntimePreferencesSnapshot::default(),
                cx,
            )
        });
        let result = entity.update(cx, |shell, _cx| {
            shell.dispatch_action_for_test(ActionRequest::TaskCreate(
                devmanager::client::action::TaskCreateArguments {
                    task_id: task_id(9),
                    environment_id: devmanager::domain::id::EnvironmentId::new(),
                    title: "created once".to_string(),
                    description: None,
                    project_id: devmanager::domain::id::ProjectId::new(),
                    workspace: devmanager::domain::task::WorkspaceRef::Main,
                },
            ));
            shell.controller_tick_for_test(32);
            shell.controller_tick_for_test(32);
            (shell.controller_tick_count(), shell.last_projection_kinds())
        });
        *report_slot_for_app.borrow_mut() = Some(result);
        drop(entity);
        cx.quit();
    });
    let (ticks, projections) = report_slot.borrow_mut().take().expect("controller report");
    assert!(ticks >= 2);
    assert_eq!(projections, vec![NativeHostProjectionKind::Replay]);
    assert_eq!(fake_handle.executed_count(), 1);
}

#[test]
fn immutable_client_model_projection_drives_shell_rows_and_sequence() {
    let workspace = tempdir().expect("workspace tempdir");
    let profile = isolated_dev_profile(workspace.path()).expect("isolated profile");
    let model = Arc::new(model_with_tasks(&[task_id(21), task_id(22)]));
    let mut fake = NativeHostRuntimeStub::new(
        "phase2-test://isolated",
        NativeHostState::Connected {
            endpoint: "phase2-test://isolated".to_string(),
        },
    );
    fake.push_model_projection(Arc::clone(&model));
    let report_slot = std::rc::Rc::new(std::cell::RefCell::new(None));
    let report_slot_for_app = std::rc::Rc::clone(&report_slot);
    gpui::Application::headless().run(move |cx| {
        devmanager::ui::init(cx);
        let entity = cx.new(|cx| {
            NativeShell::new_with_host_runtime_port(
                profile,
                Box::new(fake),
                RuntimePreferencesSnapshot::default(),
                cx,
            )
        });
        let result = entity.update(cx, |shell, _cx| {
            shell.controller_tick_for_test(32);
            (
                shell
                    .client_model_snapshot()
                    .map(|model| model.last_applied_sequence()),
                shell.rendered_task_count(),
            )
        });
        *report_slot_for_app.borrow_mut() = Some(result);
        drop(entity);
        cx.quit();
    });
    let (sequence, rendered) = report_slot
        .borrow_mut()
        .take()
        .expect("model projection report");
    assert_eq!(sequence, Some(7));
    assert_eq!(rendered, 2);
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
fn appearance_and_scale_preferences_are_applied_by_controller_not_paint() {
    let workspace = tempdir().expect("workspace tempdir");
    let profile = isolated_dev_profile(workspace.path()).expect("isolated profile");
    let report_slot = std::rc::Rc::new(std::cell::RefCell::new(None));
    let report_slot_for_app = std::rc::Rc::clone(&report_slot);
    gpui::Application::headless().run(move |cx| {
        devmanager::ui::init(cx);
        let entity = cx.new(|cx| NativeShell::new_for_headless(profile, cx));
        let result = entity.update(cx, |shell, _cx| {
            let next = RuntimePreferencesSnapshot::new(
                ThemeMode::Light,
                Density::Compact,
                Scale::Scale150,
            );
            shell.queue_preferences_for_test(next);
            shell.controller_tick_for_test(32);
            shell.preferences()
        });
        *report_slot_for_app.borrow_mut() = Some(result);
        drop(entity);
        cx.quit();
    });
    assert_eq!(
        report_slot.borrow_mut().take().expect("preferences"),
        RuntimePreferencesSnapshot::new(ThemeMode::Light, Density::Compact, Scale::Scale150)
    );
}

#[test]
fn platform_accessibility_bridge_reports_actual_window_tree_contract() {
    let workspace = tempdir().expect("workspace tempdir");
    let report = headless_render_smoke(workspace.path()).expect("headless native shell render");
    // Headless GPUI has the same rendered AccessKit tree, but no OS window is
    // attached. The bridge must not claim availability until the real window
    // adapter is installed.
    assert!(!report.platform_accessibility_bridge);
    assert!(report.platform_accessibility_nodes >= report.semantic_nodes);
    assert!(report
        .platform_accessibility_roles
        .contains(&accesskit::Role::Region));
    assert!(report
        .platform_accessibility_roles
        .contains(&accesskit::Role::Status));
    assert!(report.platform_accessibility_focus_is_root);
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
}
