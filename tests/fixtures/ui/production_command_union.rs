//! Source fixture mirrored by `composer_host_command_union_gate`.
//! The executable compile gate is
//! `cargo test --doc composer_host_command_union_gate`.
//!
//! Fully qualified `devmanager::domain::command::Command` and
//! `devmanager::client::action::catalog()` — a local `Command` alias,
//! `FIXTURE_CATALOG`, or `bind_with_catalog` cannot satisfy the rustdoc gate.
//! Save-draft and upload stay unmatched until their host commands exist.

fn accept_production_host_commands(command: devmanager::domain::command::Command) {
    match command {
        devmanager::domain::command::Command::CreateTask(_)
        | devmanager::domain::command::Command::RenameTask(_)
        | devmanager::domain::command::Command::SetTaskAttention(_)
        | devmanager::domain::command::Command::BeginCloseTask
        | devmanager::domain::command::Command::ReopenTask
        | devmanager::domain::command::Command::RegisterAgentSession { .. }
        | devmanager::domain::command::Command::SetPrimaryAgent { .. }
        | devmanager::domain::command::Command::RegisterArtifact { .. }
        | devmanager::domain::command::Command::RegisterResource { .. }
        | devmanager::domain::command::Command::ReleaseResource { .. }
        | devmanager::domain::command::Command::ConfirmHostQuit(_)
        | devmanager::domain::command::Command::SendNow(_)
        | devmanager::domain::command::Command::SteerCurrentTurn(_)
        | devmanager::domain::command::Command::QueueFollowUp(_)
        | devmanager::domain::command::Command::AnswerQuestion(_)
        | devmanager::domain::command::Command::ResolveApproval(_)
        | devmanager::domain::command::Command::StopTurn(_) => {}
    }
}

fn production_catalog_registers_turn_ids_not_draft_upload() {
    let catalog = devmanager::client::action::catalog;
    assert!(std::ptr::eq(
        catalog(),
        devmanager::client::action::catalog()
    ));
    let ids: Vec<_> = catalog().iter().map(|descriptor| descriptor.id).collect();
    assert!(ids.contains(&devmanager::client::action::ACTION_TASK_SEND_NOW));
    assert!(ids.contains(&devmanager::client::action::ACTION_TASK_STEER_CURRENT_TURN));
    assert!(ids.contains(&devmanager::client::action::ACTION_TASK_QUEUE_FOLLOW_UP));
    assert!(ids.contains(&devmanager::client::action::ACTION_TASK_ANSWER_QUESTION));
    assert!(ids.contains(&devmanager::client::action::ACTION_TASK_RESOLVE_APPROVAL));
    assert!(ids.contains(&devmanager::client::action::ACTION_TASK_STOP_TURN));
    assert!(!ids.contains(&devmanager::client::action::ACTION_TASK_SAVE_COMPOSER_DRAFT));
    assert!(!ids.contains(&devmanager::client::action::ACTION_TASK_STAGE_COMPOSER_ATTACHMENT));
    assert!(!ids.contains(&devmanager::client::action::ACTION_TASK_REMOVE_COMPOSER_ATTACHMENT));
    let _ = accept_production_host_commands;
}

fn main() {
    production_catalog_registers_turn_ids_not_draft_upload();
}
