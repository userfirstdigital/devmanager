//! Runtime RED for the dependency-safe production catalog/command union.
//!
//! Uses only `client::action::catalog()` and `TaskComposer::bind`. Integration
//! tests cannot see `bind_with_catalog` / `FIXTURE_CATALOG` (`#[cfg(test)]` on
//! the lib). Do not green this file by registering fixture actions. Save-draft
//! and upload stay typed HOLDs.
//!
//! Task 4.7 union sequence and canonical shell are documented at the bottom of
//! this module and in the worker handoff. Compile gate:
//! `cargo test --doc composer_host_command_union_gate`

use devmanager::client::action::{
    catalog, ACTION_TASK_ANSWER_QUESTION, ACTION_TASK_QUEUE_FOLLOW_UP,
    ACTION_TASK_REMOVE_COMPOSER_ATTACHMENT, ACTION_TASK_RESOLVE_APPROVAL,
    ACTION_TASK_SAVE_COMPOSER_DRAFT, ACTION_TASK_SEND_NOW, ACTION_TASK_STAGE_COMPOSER_ATTACHMENT,
    ACTION_TASK_STEER_CURRENT_TURN, ACTION_TASK_STOP_TURN,
};
use devmanager::domain::{AgentSessionId, ArtifactId, RequestId, TaskId};
use devmanager::ui::task_cockpit::composer::{
    ApprovalProjection, ComposerControl, ComposerDraftProjection, ComposerFence,
    ComposerHostProjection, QuestionProjection, TaskComposer, TurnId,
};

fn fence() -> ComposerFence {
    ComposerFence {
        task_id: TaskId::new(),
        agent_session_id: AgentSessionId::new(),
        runtime_generation: 4,
        action_epoch: 11,
        turn_id: Some(TurnId::from_raw(21)),
    }
}

fn production_projection() -> ComposerHostProjection {
    let owned = ArtifactId::new();
    ComposerHostProjection {
        fence: fence(),
        draft: ComposerDraftProjection {
            text: "ship this turn".into(),
            attachments: Vec::new(),
            prompt: None,
        },
        owned_artifacts: vec![owned],
        question: Some(QuestionProjection {
            request_id: RequestId::new(),
            state_revision: 3,
            options: vec!["Ship it".into()],
        }),
        approval: Some(ApprovalProjection {
            request_id: RequestId::new(),
            state_revision: 8,
        }),
        disabled_reasons: Vec::new(),
    }
}

fn catalog_ids() -> Vec<&'static str> {
    let production = devmanager::client::action::catalog();
    assert!(
        std::ptr::eq(catalog(), production),
        "union tests must use the shared production catalog slice"
    );
    production.iter().map(|descriptor| descriptor.id).collect()
}

#[test]
fn production_catalog_registers_canonical_turn_actions() {
    let ids = catalog_ids();
    for required in [
        ACTION_TASK_SEND_NOW,
        ACTION_TASK_STEER_CURRENT_TURN,
        ACTION_TASK_QUEUE_FOLLOW_UP,
        ACTION_TASK_ANSWER_QUESTION,
        ACTION_TASK_RESOLVE_APPROVAL,
        ACTION_TASK_STOP_TURN,
    ] {
        assert!(
            ids.contains(&required),
            "{required} must be a canonical ActionCatalog entry before bind can enable it"
        );
    }
}

#[test]
fn production_bind_enables_send_steer_queue_stop_via_catalog_host_union() {
    let composer = TaskComposer::bind(production_projection()).expect("host projection");
    for control in [
        ComposerControl::SendNow,
        ComposerControl::Steer,
        ComposerControl::QueueFollowUp,
        ComposerControl::StopTurn,
    ] {
        assert!(
            composer
                .availability(control)
                .expect("bounded availability")
                .is_available(),
            "{control:?} enables only after catalog ∩ host Command union"
        );
    }
}

#[test]
fn production_bind_enables_answer_and_approval_only_with_host_projected_identity() {
    let composer = TaskComposer::bind(production_projection()).expect("host projection");
    assert!(
        composer
            .availability(ComposerControl::Answer)
            .expect("answer")
            .is_available(),
        "Answer requires catalog union plus host-projected request_id/revision"
    );
    assert!(
        composer
            .availability(ComposerControl::Approval)
            .expect("approval")
            .is_available(),
        "Approval requires catalog union plus host-projected request_id/revision"
    );
}

#[test]
fn production_bind_does_not_enable_answer_or_approval_without_host_identity() {
    let mut projection = production_projection();
    projection.question = None;
    projection.approval = None;
    let composer = TaskComposer::bind(projection).expect("host projection");
    assert!(!composer
        .availability(ComposerControl::Answer)
        .expect("answer")
        .is_available());
    assert!(!composer
        .availability(ComposerControl::Approval)
        .expect("approval")
        .is_available());
}

#[test]
fn draft_and_upload_remain_typed_holds() {
    let ids = catalog_ids();
    assert!(
        !ids.contains(&ACTION_TASK_SAVE_COMPOSER_DRAFT),
        "save-draft must stay unregistered until its host Command exists"
    );
    assert!(
        !ids.contains(&ACTION_TASK_STAGE_COMPOSER_ATTACHMENT),
        "stage-attachment must stay unregistered until upload Command exists"
    );
    assert!(
        !ids.contains(&ACTION_TASK_REMOVE_COMPOSER_ATTACHMENT),
        "remove-attachment must stay unregistered until upload Command exists"
    );

    let composer = TaskComposer::bind(production_projection()).expect("host projection");
    for control in [
        ComposerControl::SaveDraft,
        ComposerControl::StageAttachment,
        ComposerControl::RemoveAttachment,
    ] {
        let availability = composer
            .availability(control)
            .expect("bounded hold availability");
        assert!(
            !availability.is_available(),
            "{control:?} stays a typed HOLD without a host Command"
        );
        assert!(availability
            .reason()
            .is_some_and(|reason| reason.contains("action catalog does not expose")));
    }
}

#[test]
fn owned_artifacts_remain_the_only_host_ownership_proof() {
    let mut projection = production_projection();
    projection.owned_artifacts.clear();
    let composer = TaskComposer::bind(projection).expect("host projection");
    assert!(
        composer.attachments().is_empty(),
        "composer must not invent staged artifacts"
    );
    assert!(
        !composer
            .availability(ComposerControl::StageAttachment)
            .expect("stage hold")
            .is_available(),
        "Stage stays HOLD; UI cannot prove ArtifactId ownership beyond host owned_artifacts"
    );
}

// Task 4.7 union sequence (same PR, no fixture catalog):
// 1. Add Command::{SendNow,SteerCurrentTurn,QueueFollowUp,AnswerQuestion,
//    ResolveApproval,StopTurn}(_) and wire `decide`.
// 2. Register the six reserved ACTION_TASK_* turn ids in `ACTIONS` only.
//    Leave SAVE_COMPOSER_DRAFT / STAGE_COMPOSER_ATTACHMENT /
//    REMOVE_COMPOSER_ATTACHMENT unregistered.
// 3. Host projects ComposerHostProjection (fence, turn, question/approval
//    identity, owned_artifacts).
// 4. Flip `composer_shared_catalog_disables_missing_turn_actions_*`.
// Canonical isolated shell:
//   CARGO_TARGET_DIR="$PWD/target" cargo test --test ui_composer_production_union -- --test-threads=1
//   CARGO_TARGET_DIR="$PWD/target" cargo test --doc composer_host_command_union_gate
//   CARGO_TARGET_DIR="$PWD/target" cargo test --lib ui::task_cockpit::composer -- --test-threads=1
