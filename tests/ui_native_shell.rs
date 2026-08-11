use devmanager::domain::id::TaskId;
use devmanager::ui::components::{ActionRequest, ActivationSource};
use devmanager::ui::native_shell::{
    headless_render_smoke, isolated_dev_profile, AccessibilityTree, NativeInteraction,
    NativeShellError, TerminalDockState,
};
use devmanager::ui::shell::{NavigationResult, PointerButton, TerminalPressRejection};
use devmanager::ui::task_cockpit::TaskList;
use tempfile::tempdir;

fn task_id(tail: u8) -> TaskId {
    let mut bytes = [
        0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00,
    ];
    bytes[15] = tail;
    TaskId::from_bytes(bytes).expect("fixed UUIDv7 task id")
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
fn isolated_profile_exposes_one_explicit_native_host_client_config() {
    let workspace = tempdir().expect("workspace tempdir");
    let profile = isolated_dev_profile(workspace.path()).expect("isolated profile");
    let config = profile.host_client_config();

    assert_eq!(profile.named_profile(), "native-next-dev");
    assert_eq!(config.named_profile, profile.named_profile());
    assert!(config.client_build.starts_with("devmanager-next/"));
}
