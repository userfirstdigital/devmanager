use devmanager::client::action::catalog;
use devmanager::domain::{AgentSessionId, ArtifactId, TaskId};
use devmanager::ui::components::interaction::FocusEpochSource;
use devmanager::ui::components::text_field::TextFieldKey;
use devmanager::ui::task_cockpit::composer::{
    ComposerAttachmentProjection, ComposerControl, ComposerDraftProjection, ComposerError,
    ComposerFence, ComposerHostProjection, EnterPreference, PromptVersionRef, TaskComposer, TurnId,
    EXPECTED_ACTION_QUEUE, EXPECTED_ACTION_SAVE_DRAFT, EXPECTED_ACTION_SEND_NOW,
    EXPECTED_ACTION_STEER,
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

fn projection_with(fence: ComposerFence, text: &str) -> ComposerHostProjection {
    ComposerHostProjection {
        fence,
        draft: ComposerDraftProjection {
            text: text.to_string(),
            attachments: Vec::new(),
            prompt: None,
        },
        owned_artifacts: Vec::new(),
        question: None,
        approval: None,
        disabled_reasons: Vec::new(),
    }
}

fn bind(text: &str) -> TaskComposer {
    TaskComposer::bind(projection_with(fence(), text)).expect("composer binds a host projection")
}

fn focus(composer: &mut TaskComposer, epochs: &mut FocusEpochSource) {
    let epoch = epochs.current();
    composer.set_focus_epoch(epoch).expect("focus epoch");
    composer.focus_input(epoch).expect("focus input");
}

// Current HOLD: production catalog has no turn actions. Flip this when the
// real catalog/command union lands (`tests/ui_composer_production_union.rs`).
#[test]
fn composer_shared_catalog_disables_missing_turn_actions_and_cannot_mint() {
    let mut composer = bind("ship this turn");
    let mut epochs = FocusEpochSource::new();
    focus(&mut composer, &mut epochs);
    let epoch = epochs.current();

    for control in [
        ComposerControl::SendNow,
        ComposerControl::Steer,
        ComposerControl::QueueFollowUp,
        ComposerControl::Answer,
        ComposerControl::Approval,
        ComposerControl::StopTurn,
        ComposerControl::SaveDraft,
        ComposerControl::StageAttachment,
        ComposerControl::RemoveAttachment,
    ] {
        let availability = composer
            .availability(control)
            .expect("bounded availability");
        assert!(
            !availability.is_available(),
            "{control:?} must stay disabled without Task 4.7 catalog entries"
        );
        assert!(availability
            .reason()
            .is_some_and(|reason: &str| reason.contains("action catalog does not expose")));
        let error = composer
            .activate(control, epoch)
            .expect_err("missing catalog actions cannot mint intents");
        assert!(matches!(error, ComposerError::Unavailable { .. }));
    }
    assert!(composer.pending_intent().is_none());
    assert!(catalog().iter().all(|descriptor| {
        descriptor.id != EXPECTED_ACTION_SEND_NOW && descriptor.id != EXPECTED_ACTION_SAVE_DRAFT
    }));
}

#[test]
fn composer_multiline_input_is_ephemeral_and_rejects_oversize_text() {
    let mut composer = bind("");
    let mut epochs = FocusEpochSource::new();
    focus(&mut composer, &mut epochs);
    let epoch = epochs.current();

    composer
        .handle_key(TextFieldKey::Character('a'), epoch)
        .expect("type a");
    composer
        .insert_newline(epoch)
        .expect("shift-enter inserts a newline");
    composer
        .handle_key(TextFieldKey::Character('b'), epoch)
        .expect("type b");
    assert_eq!(composer.draft_text(), "a\nb");

    let too_long = "x".repeat(composer.text_limits().max_scalars + 1);
    let error = composer
        .replace_draft(&too_long, epoch)
        .expect_err("composer text must be bounded");
    assert!(matches!(error, ComposerError::TextBoundExceeded { .. }));
    assert_eq!(composer.draft_text(), "a\nb");
}

#[test]
fn composer_host_projection_is_the_only_draft_source() {
    let first_fence = fence();
    let mut composer =
        TaskComposer::bind(projection_with(first_fence, "keep me")).expect("host draft projection");
    let mut epochs = FocusEpochSource::new();
    focus(&mut composer, &mut epochs);
    assert_eq!(composer.draft_text(), "keep me");

    composer
        .replace_draft("ephemeral overlay", epochs.current())
        .expect("dirty edit");
    assert_eq!(composer.draft_text(), "ephemeral overlay");

    let mut same = first_fence;
    same.runtime_generation = first_fence.runtime_generation;
    composer
        .apply_projection(
            projection_with(same, "host still says keep me"),
            epochs.current(),
        )
        .expect("same-task refresh keeps the ephemeral overlay");
    assert_eq!(composer.draft_text(), "ephemeral overlay");

    let mut next = first_fence;
    next.task_id = TaskId::new();
    next.agent_session_id = AgentSessionId::new();
    let rejected = composer
        .apply_projection(projection_with(next, "other task"), epochs.current())
        .expect_err("cross-task apply cannot overwrite a scoped draft");
    assert!(matches!(rejected, ComposerError::StaleFence { .. }));
    assert_eq!(composer.draft_text(), "ephemeral overlay");
    assert_eq!(composer.fence().task_id, first_fence.task_id);
}

#[test]
fn composer_slash_action_search_uses_catalog_and_bounds_query() {
    let mut composer = bind("");
    let mut epochs = FocusEpochSource::new();
    focus(&mut composer, &mut epochs);
    let epoch = epochs.current();

    composer.replace_draft("/task", epoch).expect("slash query");
    let hits = composer.action_search().expect("bounded search");
    assert!(hits.iter().any(|hit| hit.id == "task.list"));
    assert!(hits.iter().any(|hit| hit.id == "task.show"));
    assert!(hits.iter().all(|hit| {
        hit.id != EXPECTED_ACTION_SEND_NOW
            && hit.id != EXPECTED_ACTION_STEER
            && hit.id != EXPECTED_ACTION_QUEUE
    }));
    assert!(hits
        .iter()
        .all(|hit| catalog().iter().any(|item| item.id == hit.id)));

    let overflow = format!("/{}", "q".repeat(composer.search_query_limit() + 1));
    let error = composer
        .replace_draft(&overflow, epoch)
        .and_then(|_| composer.action_search())
        .expect_err("search query is cap+1 bounded");
    assert!(matches!(error, ComposerError::TextBoundExceeded { .. }));
}

#[test]
fn composer_attachments_are_host_artifact_projections() {
    let artifact_id = ArtifactId::new();
    let mut projection = projection_with(fence(), "see file");
    projection.owned_artifacts.push(artifact_id);
    projection
        .draft
        .attachments
        .push(ComposerAttachmentProjection {
            artifact_id,
            kind: devmanager::ui::task_cockpit::composer::AttachmentKind::File,
            label: "notes.md".into(),
        });
    let composer = TaskComposer::bind(projection).expect("artifact projection");
    let shown = composer.attachments();
    assert_eq!(shown.len(), 1);
    assert_eq!(shown[0].artifact_id, artifact_id);
    assert_eq!(shown[0].label, "notes.md");
    assert!(!format!("{shown:?}").contains('\\'));
    assert!(!format!("{shown:?}").contains("/home"));
    assert!(!format!("{composer:?}").contains("composer-drafts.json"));
}

#[test]
fn composer_paste_is_bounded_and_returns_typed_stale_errors() {
    let mut composer = bind("");
    let mut epochs = FocusEpochSource::new();
    focus(&mut composer, &mut epochs);
    let epoch = epochs.current();

    assert!(composer.paste_text("alpha", epoch).expect("paste"));
    assert_eq!(composer.draft_text(), "alpha");

    let stale = epoch;
    let next = epochs.advance();
    composer.set_focus_epoch(next).expect("advance");
    composer.focus_input(next).expect("refocus");
    let stale_paste = composer
        .paste_text("beta", stale)
        .expect_err("stale paste is a typed error");
    assert!(matches!(stale_paste, ComposerError::StaleFocusEpoch { .. }));
    assert_eq!(composer.draft_text(), "alpha");

    let overflow = "z".repeat(composer.text_limits().max_scalars);
    let error = composer
        .paste_text(&overflow, next)
        .expect_err("paste must preflight the bound");
    assert!(matches!(error, ComposerError::TextBoundExceeded { .. }));
    assert_eq!(composer.draft_text(), "alpha");
}

#[test]
fn composer_enter_shift_enter_and_ime_never_send_during_composition() {
    let mut composer = bind("ready");
    let mut epochs = FocusEpochSource::new();
    focus(&mut composer, &mut epochs);
    let epoch = epochs.current();
    composer
        .set_enter_preference(EnterPreference::EnterSends)
        .expect("pref");

    composer.begin_ime(epoch).expect("ime start");
    assert!(composer
        .handle_enter(epoch)
        .expect("ime enter is ignored")
        .is_none());
    assert!(composer.pending_intent().is_none());
    composer.end_ime(epoch).expect("ime end");

    composer.handle_shift_enter(epoch).expect("newline");
    assert_eq!(composer.draft_text(), "ready\n");
    assert!(composer.pending_intent().is_none());

    let denied = composer
        .handle_enter(epoch)
        .expect_err("Enter cannot mint without a catalog action");
    assert!(matches!(
        denied,
        ComposerError::Unavailable { .. } | ComposerError::StalePointer { .. }
    ));
    assert!(composer.pending_intent().is_none());
}

#[test]
fn composer_prompt_library_inserts_immutable_version_without_sending() {
    let mut composer = bind("");
    let mut epochs = FocusEpochSource::new();
    focus(&mut composer, &mut epochs);
    let epoch = epochs.current();

    let inserted = composer
        .insert_prompt_version(
            PromptVersionRef {
                prompt_id: "prompt-review".into(),
                version: 7,
                body: "Review the bounded diff only.".into(),
            },
            epoch,
        )
        .expect("insert");
    assert_eq!(inserted, "Review the bounded diff only.");
    assert_eq!(composer.draft_text(), "Review the bounded diff only.");
    assert_eq!(
        composer.inserted_prompt_version(),
        Some(("prompt-review", 7))
    );
    assert!(composer.pending_intent().is_none());
    assert!(!composer.auto_sent());
}

#[test]
fn composer_stale_pointer_release_cannot_submit_replacement_state() {
    let mut composer = bind("do not fire");
    let mut epochs = FocusEpochSource::new();
    focus(&mut composer, &mut epochs);
    let first = epochs.current();
    assert!(matches!(
        composer
            .pointer_down(ComposerControl::SendNow, 9, first)
            .expect_err("disabled press is not armed"),
        ComposerError::StalePointer { .. }
    ));

    let mut next = composer.fence();
    next.task_id = TaskId::new();
    next.agent_session_id = AgentSessionId::new();
    let second = epochs.advance();
    composer
        .retarget(projection_with(next, "fresh target"), second)
        .expect("explicit navigation replaces the host projection");
    let rejected = composer
        .pointer_up(ComposerControl::SendNow, 9, first)
        .expect_err("stale release is typed and non-writing");
    assert!(matches!(rejected, ComposerError::StalePointer { .. }));
    assert!(composer.pending_intent().is_none());
    assert_eq!(composer.draft_text(), "fresh target");

    composer.focus_input(second).expect("focus after nav");
    let stale_fence = composer
        .fence()
        .with_runtime_generation(composer.fence().runtime_generation.saturating_add(1));
    let stale_fence_error = composer
        .activate_with_fence(ComposerControl::SendNow, stale_fence, second)
        .expect_err("captured fence must match");
    assert!(matches!(
        stale_fence_error,
        ComposerError::StaleFence { .. }
    ));
}

#[test]
fn composer_enter_after_same_epoch_retarget_cannot_submit_replacement() {
    let mut composer = bind("original task");
    let mut epochs = FocusEpochSource::new();
    focus(&mut composer, &mut epochs);
    let epoch = epochs.current();
    composer
        .set_enter_preference(EnterPreference::EnterSends)
        .expect("pref");

    let mut next = composer.fence();
    next.task_id = TaskId::new();
    next.agent_session_id = AgentSessionId::new();
    composer
        .retarget(projection_with(next, "replacement task"), epoch)
        .expect("same-epoch retarget");
    let rejected = composer
        .handle_enter(epoch)
        .expect_err("unarmed Enter cannot target the replacement");
    assert!(matches!(
        rejected,
        ComposerError::StalePointer { .. }
            | ComposerError::StaleFence { .. }
            | ComposerError::Unavailable { .. }
    ));
    assert!(composer.pending_intent().is_none());
    assert_eq!(composer.draft_text(), "replacement task");
}

#[test]
fn composer_stale_runtime_projection_is_rejected_atomically() {
    let mut composer = bind("keep overlay");
    let mut epochs = FocusEpochSource::new();
    focus(&mut composer, &mut epochs);
    let epoch = epochs.current();
    composer
        .replace_draft("ephemeral", epoch)
        .expect("dirty overlay");

    let mut stale = projection_with(composer.fence(), "older host draft");
    stale.fence.runtime_generation = composer.fence().runtime_generation.saturating_sub(1);
    let error = composer
        .apply_projection(stale, epoch)
        .expect_err("older runtime generation cannot mutate");
    assert!(matches!(error, ComposerError::StaleFence { .. }));
    assert_eq!(composer.draft_text(), "ephemeral");
    assert_eq!(composer.fence().runtime_generation, 4);
}

#[test]
fn composer_foreign_artifact_cannot_cross_target_a_scoped_draft() {
    let owned = ArtifactId::new();
    let foreign = ArtifactId::new();
    let mut projection = projection_with(fence(), "scoped");
    projection.owned_artifacts.push(owned);
    let mut composer = TaskComposer::bind(projection).expect("bind");
    let rejected = TaskComposer::bind({
        let mut bad = projection_with(composer.fence(), "scoped");
        bad.draft.attachments.push(ComposerAttachmentProjection {
            artifact_id: foreign,
            kind: devmanager::ui::task_cockpit::composer::AttachmentKind::File,
            label: "notes.md".into(),
        });
        bad
    })
    .expect_err("unowned ArtifactId cannot bind");
    assert!(matches!(rejected, ComposerError::AttachmentRejected { .. }));

    let mut epochs = FocusEpochSource::new();
    focus(&mut composer, &mut epochs);
    let epoch = epochs.current();
    let stage = composer
        .activate_stage(foreign, epoch)
        .expect_err("foreign stage stays typed unavailable");
    assert!(matches!(
        stage,
        ComposerError::Unavailable { .. } | ComposerError::AttachmentRejected { .. }
    ));
    let remove = composer
        .activate_remove(owned, epoch)
        .expect_err("remove without a staged owned artifact stays unavailable");
    assert!(matches!(
        remove,
        ComposerError::Unavailable { .. } | ComposerError::AttachmentRejected { .. }
    ));
}

#[test]
fn composer_labels_and_reasons_redact_paths_secrets_ansi_and_bidi() {
    let mut projection = projection_with(fence(), "safe");
    projection.disabled_reasons.push((
        ComposerControl::SendNow,
        "C:\\Users\\secret\\.ssh\\id_rsa api_key=UI_COMPOSER_SECRET_SENTINEL \u{1b}[31m\u{202e}leak"
            .into(),
    ));
    projection.owned_artifacts.push(ArtifactId::new());
    let artifact = projection.owned_artifacts[0];
    projection
        .draft
        .attachments
        .push(ComposerAttachmentProjection {
            artifact_id: artifact,
            kind: devmanager::ui::task_cockpit::composer::AttachmentKind::File,
            label: "/home/secret/.ssh/id_rsa".into(),
        });
    let composer = TaskComposer::bind(projection).expect("sanitized bind");
    let send = composer
        .control_accessibility(ComposerControl::SendNow)
        .expect("bounded accessibility");
    let shown = format!(
        "{}{}{:?}",
        send.description,
        composer.attachments()[0].label,
        composer.attachments()
    );
    assert!(!shown.contains("UI_COMPOSER_SECRET_SENTINEL"));
    assert!(!shown.contains("id_rsa"));
    assert!(!shown.contains("C:\\Users"));
    assert!(!shown.contains("/home/secret"));
    assert!(!shown.contains('\u{1b}'));
    assert!(!shown.contains('\u{202e}'));
    assert!(send.disabled);
}
