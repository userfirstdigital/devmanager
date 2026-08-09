use devmanager::domain::id::TaskId;
use devmanager::ui::shell::{
    InvalidationReason, NavigationResult, PointerButton, ReleaseRejection, Shell, TerminalRelease,
    TransientPriority,
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
fn navigation_mouse_down_commits_once_and_both_mouse_events_are_consumed() {
    let first = task_id(1);
    let second = task_id(2);
    let mut shell = Shell::new(Some(first));
    let epoch = shell.navigation_epoch();

    let navigation = shell.navigation_mouse_down(second, epoch);
    assert!(navigation.consumed());
    assert_eq!(
        navigation,
        NavigationResult::Committed {
            task_id: second,
            navigation_epoch: epoch + 1,
        }
    );
    assert_eq!(shell.selected_task(), Some(second));
    assert_eq!(shell.navigation_mouse_up(), true);
    assert_eq!(shell.navigation_mouse_up(), true);

    assert_eq!(
        shell.navigation_mouse_down(first, epoch),
        NavigationResult::Rejected {
            reason: devmanager::ui::shell::NavigationRejection::StaleEpoch,
        }
    );
    assert_eq!(shell.selected_task(), Some(second));
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
    let other_task = task_id(5);
    let mut shell = Shell::new(Some(task));
    let owner = shell
        .terminal_mouse_down(41, task, PointerButton::Primary)
        .expect("selected terminal can own one pointer");

    let release = shell.terminal_mouse_up(owner);
    assert!(release.consumed());
    assert_eq!(release, TerminalRelease::Authorized);
    assert_eq!(shell.pointer_owner(), None);

    let owner = shell
        .terminal_mouse_down(41, task, PointerButton::Primary)
        .expect("pointer can be captured again");
    let mut mismatched = owner;
    mismatched.task_id = other_task;
    assert_eq!(
        shell.terminal_mouse_up(mismatched),
        TerminalRelease::Rejected(ReleaseRejection::MismatchedOwner)
    );
    assert_eq!(shell.pointer_owner(), None);

    let owner = shell
        .terminal_mouse_down(41, task, PointerButton::Primary)
        .expect("pointer can be captured after rejection");
    let mut mismatched = owner;
    mismatched.generation += 1;
    assert_eq!(
        shell.terminal_mouse_up(mismatched),
        TerminalRelease::Rejected(ReleaseRejection::MismatchedOwner)
    );

    let owner = shell
        .terminal_mouse_down(41, task, PointerButton::Primary)
        .expect("pointer can be captured after generation mismatch");
    let mut mismatched = owner;
    mismatched.button = PointerButton::Secondary;
    assert_eq!(
        shell.terminal_mouse_up(mismatched),
        TerminalRelease::Rejected(ReleaseRejection::MismatchedOwner)
    );

    let owner = shell
        .terminal_mouse_down(41, task, PointerButton::Primary)
        .expect("pointer can be captured after button mismatch");
    let mut mismatched = owner;
    mismatched.navigation_epoch += 1;
    assert_eq!(
        shell.terminal_mouse_up(mismatched),
        TerminalRelease::Rejected(ReleaseRejection::MismatchedOwner)
    );
}

#[test]
fn view_focus_deactivate_and_resync_invalidate_capture_without_synthesizing_release() {
    let task = task_id(6);
    for reason in [
        InvalidationReason::ViewSwitch,
        InvalidationReason::FocusLoss,
        InvalidationReason::Deactivate,
        InvalidationReason::Resync,
    ] {
        let mut shell = Shell::new(Some(task));
        let owner = shell
            .terminal_mouse_down(9, task, PointerButton::Primary)
            .expect("selected terminal can own one pointer");
        assert!(shell.invalidate(reason));
        assert_eq!(shell.pointer_owner(), None);
        assert_eq!(
            shell.terminal_mouse_up(owner),
            TerminalRelease::Rejected(ReleaseRejection::NoOwner)
        );
    }
}
