use devmanager::client::action::ActionRequest;
use devmanager::domain::id::TaskId;
use devmanager::ui::components::interaction::{
    ActivationSource, InteractionStateModel, KeyboardKey,
};
use devmanager::ui::native_shell::NativeInteraction;
use devmanager::ui::shell::{
    HostEpochSnapshot, InvalidationReason, PointerButton, ReleaseRejection, Shell,
    TerminalPressRejection, TerminalRelease, TransientPriority,
};
use devmanager::ui::task_cockpit::TaskList;

fn attached_shell(task: devmanager::domain::id::TaskId) -> Shell {
    Shell::new(
        Some(task),
        HostEpochSnapshot::try_from_host(3, 5, 7, 11, 13).expect("test host epochs"),
    )
}

fn task_id(tail: u8) -> TaskId {
    let mut bytes = [
        0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00,
    ];
    bytes[15] = tail;
    TaskId::from_bytes(bytes).expect("fixed UUIDv7 task id")
}

#[test]
fn transient_priority_is_shell_local_and_has_no_host_effect_surface() {
    let task = task_id(3);
    let mut shell = attached_shell(task);

    assert_eq!(shell.transient_priority(), None);
    shell.set_transient_priority(Some(TransientPriority::High));
    assert_eq!(shell.transient_priority(), Some(TransientPriority::High));
    shell.set_transient_priority(None);
    assert_eq!(shell.transient_priority(), None);
}

#[test]
fn terminal_release_requires_the_exact_pointer_task_generation_button_and_epoch_owner() {
    let task = task_id(4);
    let mut shell = attached_shell(task);
    let epoch = shell.navigation_epoch();
    let owner = shell
        .terminal_mouse_down(41, task, PointerButton::Primary, epoch, Some(task))
        .expect("selected terminal can own one pointer");

    let release = shell.terminal_mouse_up(Some(owner));
    assert!(release.consumed());
    assert_eq!(release, TerminalRelease::Authorized);
    assert_eq!(
        shell.terminal_mouse_up(None),
        TerminalRelease::Rejected(ReleaseRejection::NoOwner)
    );
}

#[test]
fn foreign_task_and_projected_selection_cannot_capture_terminal_pointer() {
    let task = task_id(6);
    let foreign = task_id(9);
    let mut shell = attached_shell(task);
    let epoch = shell.navigation_epoch();

    assert_eq!(
        shell.terminal_mouse_down(9, foreign, PointerButton::Primary, epoch, Some(foreign)),
        Err(TerminalPressRejection::TaskNotSelected)
    );
    assert_eq!(
        shell.terminal_mouse_down(9, task, PointerButton::Primary, epoch, Some(foreign)),
        Err(TerminalPressRejection::TaskNotSelected)
    );
    assert_eq!(
        shell.terminal_mouse_up(None),
        TerminalRelease::Rejected(ReleaseRejection::NoOwner)
    );
}

#[test]
fn lifecycle_boundaries_reject_stale_release_and_input_after_invalidating_active_owner() {
    let task = task_id(6);
    for reason in [
        InvalidationReason::FocusLoss,
        InvalidationReason::ViewSwitch,
        InvalidationReason::Resync,
        InvalidationReason::Deactivate,
    ] {
        let mut shell = attached_shell(task);
        let captured_epoch = shell.navigation_epoch();
        let owner = shell
            .terminal_mouse_down(9, task, PointerButton::Primary, captured_epoch, Some(task))
            .expect("lifecycle test must start with a real active terminal owner");
        assert!(shell.invalidate(reason));
        assert!(shell.navigation_epoch() > captured_epoch);
        assert_eq!(
            shell.terminal_mouse_up(Some(owner)),
            TerminalRelease::Rejected(ReleaseRejection::NoOwner)
        );
        assert_eq!(
            shell.terminal_mouse_down(9, task, PointerButton::Primary, captured_epoch, Some(task)),
            Err(TerminalPressRejection::StaleEpoch)
        );
        assert_eq!(
            shell.terminal_mouse_up(None),
            TerminalRelease::Rejected(ReleaseRejection::NoOwner)
        );
    }
}

#[test]
fn forged_foreign_release_token_is_rejected_and_invalid_mouse_up_is_consumed() {
    let task = task_id(10);
    let mut receiver = attached_shell(task);
    let mut foreign_shell = attached_shell(task);
    let epoch = receiver.navigation_epoch();

    receiver
        .terminal_mouse_down(1, task, PointerButton::Primary, epoch, Some(task))
        .expect("receiver captures its pointer");
    let foreign_token = foreign_shell
        .terminal_mouse_down(
            1,
            task,
            PointerButton::Primary,
            foreign_shell.navigation_epoch(),
            Some(task),
        )
        .expect("foreign shell issues its own token");

    assert_eq!(
        receiver.terminal_mouse_up(Some(foreign_token)),
        TerminalRelease::Rejected(ReleaseRejection::MismatchedOwner)
    );
    assert_eq!(
        receiver.terminal_mouse_up(None),
        TerminalRelease::Rejected(ReleaseRejection::NoOwner)
    );

    receiver
        .terminal_mouse_down(2, task, PointerButton::Primary, epoch, Some(task))
        .expect("mismatched release consumed the prior capture");
    assert_eq!(
        receiver.terminal_mouse_up(None),
        TerminalRelease::Rejected(ReleaseRejection::MismatchedOwner)
    );
    assert_eq!(
        receiver.terminal_mouse_up(None),
        TerminalRelease::Rejected(ReleaseRejection::NoOwner)
    );
}

#[test]
fn task_selection_click_does_not_activate_an_overlapping_control() {
    let task = task_id(12);
    let task_list = TaskList::from_virtual_task_ids(vec![task]).expect("task source");
    let mut interaction = NativeInteraction::new(None);
    let mut overlapping = InteractionStateModel::default();
    let down_epoch = interaction.current_focus_epoch();
    overlapping.set_focus_epoch(down_epoch);
    assert!(overlapping.pointer_down(7, down_epoch));

    let outcome = interaction.navigation_mouse_down_for(7, task, &task_list);
    assert!(outcome.consumed);
    assert!(outcome.propagation_stopped);
    assert_eq!(
        outcome.navigation,
        devmanager::ui::shell::NavigationResult::Committed {
            task_id: task,
            navigation_epoch: interaction.action_epochs().navigation_epoch,
        }
    );

    assert!(
        !interaction.overlapping_control_pointer_up(&mut overlapping, 7, down_epoch),
        "a task-selecting pointer must not also activate an overlapping control"
    );
    assert!(
        interaction
            .action_from_source(
                ActionRequest::HostStatus,
                ActivationSource::Pointer { pointer_id: 7 },
            )
            .is_none(),
        "the consumed pointer must not dispatch a second catalog action"
    );

    interaction.release_pointer(7);
    interaction.begin_control_pointer(7);
    let next = interaction
        .action_from_source(
            ActionRequest::HostStatus,
            ActivationSource::Pointer { pointer_id: 7 },
        )
        .expect("a later exclusive control gesture may activate");
    assert!(matches!(
        next.event.source,
        ActivationSource::Pointer { pointer_id: 7 }
    ));
}

#[test]
fn terminal_click_does_not_activate_an_overlapping_control() {
    let task = task_id(14);
    let mut interaction = NativeInteraction::new(Some(task));
    let mut overlapping = InteractionStateModel::default();
    let down_epoch = interaction.current_focus_epoch();
    overlapping.set_focus_epoch(down_epoch);
    overlapping.focus();
    assert!(overlapping.pointer_down(3, down_epoch));

    let outcome = interaction.terminal_mouse_down(3, task, PointerButton::Primary, Some(task));
    assert!(outcome.consumed);
    assert!(outcome.propagation_stopped);
    assert!(outcome.capture.is_ok());

    assert!(
        !interaction.overlapping_control_pointer_up(&mut overlapping, 3, down_epoch),
        "a terminal-owning pointer must not also activate an overlapping control"
    );
    assert!(!overlapping.key_activate(KeyboardKey::Enter, down_epoch));
    assert!(interaction
        .action_from_source(
            ActionRequest::HostStatus,
            ActivationSource::Pointer { pointer_id: 3 },
        )
        .is_none());
}
