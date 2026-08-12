use devmanager::client::model::ClientModelBuilder;
use devmanager::domain::{
    AgentRole, AgentSessionFacts, AgentSessionLifecycle, EnvironmentId, OwnerKind, ProjectId,
    RequestId, ResourceFacts, ResourceKind, ResourceLifecycle, ResourceRecipe, ReviewReadiness,
    SnapshotId, SnapshotItem, SnapshotPage, SnapshotSection, TaskActivity, TaskAssignment,
    TaskAttention, TaskConnectivity, TaskFacts, TaskLifecycle, TaskSnapshotItem, WorkspaceRef,
};
use devmanager::ui::task_cockpit::dock::{
    ContextDock, DockEdge, DockPointerSurface, DockShortcut, DockTool, PointerButton, PointerPhase,
    PointerPress,
};

fn uuid(tail: u8) -> [u8; 16] {
    [
        0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        tail,
    ]
}

fn task_id(tail: u8) -> devmanager::domain::TaskId {
    devmanager::domain::TaskId::from_bytes(uuid(tail)).expect("task")
}

fn focus_model(
    tail: u8,
    runtime_generation: u64,
    resource_generation: u64,
) -> devmanager::client::model::ClientModel {
    let task_id = task_id(tail);
    let agent_id = devmanager::domain::AgentSessionId::from_bytes(uuid(tail)).expect("agent");
    let resource_id = devmanager::domain::ResourceId::from_bytes(uuid(tail)).expect("resource");
    let snap = SnapshotId::from_bytes(uuid(0x10)).expect("snapshot");
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
            vec![SnapshotItem::Task(TaskSnapshotItem {
                task: TaskFacts {
                    id: task_id,
                    environment_id: EnvironmentId::from_bytes(uuid(0x01)).expect("env"),
                    title: "Focus dock".into(),
                    description: None,
                    project_id: ProjectId::from_bytes(uuid(0x02)).expect("project"),
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
            })],
        ))
        .expect("tasks");
    builder
        .ingest_page(page(
            SnapshotSection::AgentSessions,
            vec![SnapshotItem::AgentSession(AgentSessionFacts {
                id: agent_id,
                task_id,
                role: AgentRole::Primary,
                provider_kind: "claude".into(),
                provider_session_id: None,
                lifecycle: AgentSessionLifecycle::Open,
                runtime_generation,
                revision: 0,
            })],
        ))
        .expect("agents");
    builder
        .ingest_page(page(SnapshotSection::Artifacts, Vec::new()))
        .expect("artifacts");
    builder
        .ingest_page(page(
            SnapshotSection::Resources,
            vec![SnapshotItem::Resource(ResourceFacts {
                id: resource_id,
                task_id: Some(task_id),
                owner_kind: OwnerKind::Task,
                resource_kind: ResourceKind::Terminal,
                recipe: ResourceRecipe::Terminal { cols: 40, rows: 8 },
                lifecycle: ResourceLifecycle::Active,
                runtime_generation: resource_generation,
                updated_at_ms: 1,
            })],
        ))
        .expect("resources");
    builder
        .ingest_page(page(SnapshotSection::Operations, Vec::new()))
        .expect("operations");
    builder.finish().expect("client model")
}

fn bind(dock: &mut ContextDock, model: &devmanager::client::model::ClientModel, tail: u8) {
    dock.follow_task(task_id(tail));
    dock.bind_from_model(model).expect("bind");
    dock.dispatch_shortcut(DockShortcut::AltTool(3), RequestId::new(), model)
        .expect("terminal");
    dock.dispatch_shortcut(DockShortcut::ToggleRawTerminal, RequestId::new(), model)
        .expect("raw");
    dock.focus_terminal();
}

fn press(surface: DockPointerSurface, pointer_id: u64) -> PointerPress {
    PointerPress {
        pointer_id,
        button: PointerButton::Left,
        surface,
    }
}

#[test]
fn dock_sidebar_then_terminal_up_is_click_through_and_does_not_report() {
    let mut dock = ContextDock::new(DockEdge::Right);
    bind(&mut dock, &focus_model(0x11, 1, 1), 0x11);
    assert!(
        !dock.terminal_mouse_reports_enabled(),
        "focus alone must not arm mouse reporting"
    );

    assert!(dock.handle_gpui_pointer(PointerPhase::Down, press(DockPointerSurface::Sidebar, 1)));
    assert!(!dock.terminal_mouse_reports_enabled());
    assert!(!dock.handle_gpui_pointer(PointerPhase::Up, press(DockPointerSurface::TerminalGrid, 1)));
    assert!(!dock.terminal_selection_changed());
    assert!(!dock.terminal_mouse_report_emitted());
}

#[test]
fn dock_tab_and_resize_presses_cannot_become_terminal_mouse_reports() {
    let model = focus_model(0x12, 1, 1);
    let mut dock = ContextDock::new(DockEdge::Right);
    bind(&mut dock, &model, 0x12);

    assert!(dock.handle_gpui_pointer(
        PointerPhase::Down,
        press(DockPointerSurface::Tab(DockTool::Files), 2)
    ));
    assert!(!dock.terminal_mouse_reports_enabled());
    assert!(!dock.handle_gpui_pointer(PointerPhase::Up, press(DockPointerSurface::TerminalGrid, 2)));

    dock.dispatch_shortcut(DockShortcut::AltTool(3), RequestId::new(), &model)
        .expect("terminal");
    dock.dispatch_shortcut(DockShortcut::ToggleRawTerminal, RequestId::new(), &model)
        .expect("raw");
    dock.focus_terminal();
    assert!(dock.handle_gpui_pointer(
        PointerPhase::Down,
        press(DockPointerSurface::ResizeHandle, 3)
    ));
    assert!(!dock.handle_gpui_pointer(
        PointerPhase::Move,
        press(DockPointerSurface::TerminalGrid, 3)
    ));
    assert!(!dock.handle_gpui_pointer(PointerPhase::Up, press(DockPointerSurface::TerminalGrid, 3)));
    assert!(!dock.terminal_mouse_report_emitted());
}

#[test]
fn dock_terminal_grid_capture_authorizes_its_own_drag_and_release() {
    let mut dock = ContextDock::new(DockEdge::Right);
    bind(&mut dock, &focus_model(0x13, 1, 1), 0x13);
    assert!(!dock.terminal_mouse_reports_enabled());
    assert!(dock.handle_gpui_pointer(
        PointerPhase::Down,
        press(DockPointerSurface::TerminalGrid, 4)
    ));
    assert!(
        !dock.terminal_mouse_reports_enabled(),
        "incomplete press must not report mouse"
    );
    assert!(dock.handle_gpui_pointer(
        PointerPhase::Move,
        press(DockPointerSurface::TerminalGrid, 4)
    ));
    assert!(!dock.terminal_mouse_report_emitted());
    assert!(dock.handle_gpui_pointer(PointerPhase::Up, press(DockPointerSurface::TerminalGrid, 4)));
    assert!(dock.terminal_mouse_report_emitted());
    assert!(dock.terminal_mouse_reports_enabled());
}

#[test]
fn dock_release_requires_exact_press_owner_fields() {
    let mut dock = ContextDock::new(DockEdge::Right);
    bind(&mut dock, &focus_model(0x14, 1, 1), 0x14);
    assert!(dock.handle_gpui_pointer(
        PointerPhase::Down,
        press(DockPointerSurface::TerminalGrid, 5)
    ));
    let owner = dock.press_owner().expect("captured");
    assert_eq!(owner.pointer_id(), 5);
    assert_eq!(owner.surface(), DockPointerSurface::TerminalGrid);

    assert!(!dock.handle_gpui_pointer(
        PointerPhase::Up,
        press(DockPointerSurface::TerminalGrid, 99)
    ));
    assert!(!dock.handle_gpui_pointer(
        PointerPhase::Up,
        PointerPress {
            pointer_id: 5,
            button: PointerButton::Right,
            surface: DockPointerSurface::TerminalGrid,
        }
    ));
    assert!(!dock.handle_gpui_pointer(PointerPhase::Up, press(DockPointerSurface::ResizeHandle, 5)));
    assert!(!dock.handle_gpui_pointer(PointerPhase::Cancel, press(DockPointerSurface::Sidebar, 5)));
    assert!(dock.press_owner().is_some());
    assert!(dock.release_press(owner));
    assert!(dock.press_owner().is_none());
}

#[test]
fn dock_reused_pointer_id_after_cancel_cannot_complete_old_surface() {
    let model = focus_model(0x15, 1, 1);
    let mut dock = ContextDock::new(DockEdge::Right);
    bind(&mut dock, &model, 0x15);
    assert!(dock.handle_gpui_pointer(
        PointerPhase::Down,
        press(DockPointerSurface::Tab(DockTool::Browser), 6)
    ));
    let first = dock.press_owner().expect("tab capture");
    assert!(dock.handle_gpui_pointer(
        PointerPhase::Cancel,
        press(DockPointerSurface::Tab(DockTool::Browser), 6)
    ));
    dock.dispatch_shortcut(DockShortcut::AltTool(3), RequestId::new(), &model)
        .expect("terminal");
    dock.focus_terminal();
    assert!(dock.handle_gpui_pointer(
        PointerPhase::Down,
        press(DockPointerSurface::TerminalGrid, 6)
    ));
    assert!(!dock.release_press(first));
    assert!(dock.handle_gpui_pointer(PointerPhase::Up, press(DockPointerSurface::TerminalGrid, 6)));
}

#[test]
fn dock_task_switch_and_generation_change_invalidate_press_owner() {
    let mut dock = ContextDock::new(DockEdge::Right);
    let first = focus_model(0x16, 1, 1);
    bind(&mut dock, &first, 0x16);
    assert!(dock.handle_gpui_pointer(
        PointerPhase::Down,
        press(DockPointerSurface::TerminalGrid, 7)
    ));
    let owner = dock.press_owner().expect("down");
    dock.follow_task(task_id(0x17));
    assert!(!dock.release_press(owner));

    bind(&mut dock, &first, 0x16);
    assert!(dock.handle_gpui_pointer(
        PointerPhase::Down,
        press(DockPointerSurface::ResizeHandle, 8)
    ));
    let resize_owner = dock.press_owner().expect("resize");
    dock.bind_from_model(&focus_model(0x16, 2, 2))
        .expect("generation change");
    assert!(!dock.release_press(resize_owner));
}

#[test]
fn dock_ui_actions_use_host_catalog_and_captured_identity() {
    use devmanager::client::action;
    use devmanager::ui::actions;
    use devmanager::ui::task_cockpit::shell::TaskCockpitShell;

    assert_eq!(action::catalog().len(), 6);
    let model = focus_model(0x18, 1, 1);
    let mut shell = TaskCockpitShell::new(DockEdge::Right);
    shell.follow_task(task_id(0x18));
    shell.follow_projection(model);
    let application = gpui::Application::headless();
    application.run(move |cx| {
        actions::register(cx);
        assert!(
            cx.all_action_names().contains(&"dock.tool.terminal"),
            "GPUI key token must register"
        );
        cx.quit();
    });
    shell
        .handle_tool_action(DockTool::Terminal, RequestId::new())
        .expect("dispatch");
    assert_eq!(shell.dock().active_tool(), DockTool::Terminal);
}

#[test]
fn dock_stale_action_node_is_rejected_after_epoch_change() {
    let mut dock = ContextDock::new(DockEdge::Right);
    let model = focus_model(0x19, 1, 1);
    bind(&mut dock, &model, 0x19);
    let node = dock
        .capture_action(DockTool::Files, RequestId::new())
        .expect("capture");
    dock.dispatch_shortcut(DockShortcut::AltTool(4), RequestId::new(), &model)
        .expect("browser");
    assert!(dock.dispatch_action(node, &model).is_err());
    let fresh = dock
        .capture_action(DockTool::Files, RequestId::new())
        .expect("fresh");
    assert!(dock.dispatch_action(fresh, &model).is_ok());
    assert_eq!(dock.active_tool(), DockTool::Files);
}
