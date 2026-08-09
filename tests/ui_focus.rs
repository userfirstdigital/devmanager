use devmanager::domain::id::TaskId;
use devmanager::ui::shell::{
    InvalidationReason, PointerButton, ReleaseRejection, Shell, TerminalPressRejection,
    TerminalRelease, TransientPriority,
};

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
    let mut shell = Shell::new(Some(task));

    assert_eq!(shell.transient_priority(), None);
    shell.set_transient_priority(Some(TransientPriority::High));
    assert_eq!(shell.transient_priority(), Some(TransientPriority::High));
    shell.set_transient_priority(None);
    assert_eq!(shell.transient_priority(), None);
}

#[test]
fn terminal_release_requires_the_exact_pointer_task_generation_button_and_epoch_owner() {
    let task = task_id(4);
    let mut shell = Shell::new(Some(task));
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
    let mut shell = Shell::new(Some(task));
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
fn stale_terminal_down_after_focus_loss_view_switch_or_resync_is_rejected() {
    let task = task_id(6);
    for reason in [
        InvalidationReason::FocusLoss,
        InvalidationReason::ViewSwitch,
        InvalidationReason::Resync,
    ] {
        let mut shell = Shell::new(Some(task));
        let captured_epoch = shell.navigation_epoch();
        assert!(shell.invalidate(reason));
        assert!(shell.navigation_epoch() > captured_epoch);
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
    let mut receiver = Shell::new(Some(task));
    let mut foreign_shell = Shell::new(Some(task));
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
