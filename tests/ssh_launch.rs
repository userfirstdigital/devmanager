use devmanager::ssh::{ssh_runtime_outcome, SshRuntimeOutcome};

#[test]
fn public_ssh_runtime_fails_closed_without_task_supervisor_adapter() {
    assert!(matches!(
        ssh_runtime_outcome(),
        SshRuntimeOutcome::Unavailable { .. }
    ));
}
