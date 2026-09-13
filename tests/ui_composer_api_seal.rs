//! Executable public-API seal: production callers only see `TaskComposer::bind`.
//! The compile-fail proof lives on `TaskComposer::bind`'s rustdoc
//! (`cargo test --doc TaskComposer`).
//! Catalog/command union RED: `tests/ui_composer_production_union.rs` and
//! `cargo test --doc composer_host_command_union_gate`.

use devmanager::ui::task_cockpit::composer::{
    ComposerDraftProjection, ComposerFence, ComposerHostProjection, TaskComposer,
};

#[test]
fn production_public_constructor_is_bind_only() {
    let _: fn(ComposerHostProjection) -> _ = TaskComposer::bind;
    let _ = ComposerFence {
        task_id: devmanager::domain::TaskId::new(),
        agent_session_id: devmanager::domain::AgentSessionId::new(),
        runtime_generation: 1,
        action_epoch: 1,
        turn_id: None,
    };
    let _ = ComposerDraftProjection {
        text: String::new(),
        attachments: Vec::new(),
        prompt: None,
    };
}
