use devmanager::ssh::SshRuntimeOutcome;

#[test]
fn runtime_contract_is_explicitly_unavailable_until_task_supervisor_exists() {
    let outcome = SshRuntimeOutcome::unavailable();
    assert!(matches!(outcome, SshRuntimeOutcome::Unavailable { .. }));
}
