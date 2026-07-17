use devmanager::browser::{
    BrowserError, BrowserRecipeAction, BrowserRecipeAssertion, BrowserRecipeInput,
    BrowserRecipeInputKind, BrowserRecipeLocator, BrowserRecipeValue, BrowserRecipeViewport,
    BrowserRecipeWait, BrowserRecordingAction, BrowserRecordingActor, BrowserRecordingCommit,
    BrowserRecordingError, BrowserRecordingInstance, BrowserRecordingMetadata,
    BrowserRecordingStatus, BrowserRisk, BrowserWorkflowRecorder, BrowserWorkspaceKey,
};
use static_assertions::assert_not_impl_any;

assert_not_impl_any!(BrowserRecordingAction: std::fmt::Debug, serde::Serialize);
assert_not_impl_any!(BrowserRecordingMetadata: std::fmt::Debug, serde::Serialize);
assert_not_impl_any!(devmanager::browser::BrowserRecordingReview: std::fmt::Debug, serde::Serialize);
assert_not_impl_any!(BrowserWorkflowRecorder: std::fmt::Debug, serde::Serialize);

fn workspace(project: &str, tab: &str) -> BrowserWorkspaceKey {
    BrowserWorkspaceKey::new(project, tab).expect("valid workspace")
}

#[test]
fn cancellation_capacity_and_late_completion_preserve_the_exact_instance() {
    let workspace = workspace("project-a", "ai-a");
    let mut recorder = BrowserWorkflowRecorder::with_capacity(2);
    let instance = recorder.start(workspace.clone()).expect("start");
    let failed = recorder
        .reserve(&instance, BrowserRecordingActor::Agent)
        .expect("reserve failed action");
    let success = recorder
        .reserve(&instance, BrowserRecordingActor::User)
        .expect("reserve successful action");

    assert_eq!(
        recorder
            .reserve(&instance, BrowserRecordingActor::Agent)
            .unwrap_err(),
        BrowserRecordingError::CapacityExceeded
    );
    assert_eq!(
        recorder
            .commit(success, navigate("https://example.test/success"))
            .expect("buffer success"),
        BrowserRecordingCommit::Buffered
    );
    assert_eq!(
        recorder.cancel(failed).expect("cancel failure"),
        BrowserRecordingCommit::Recorded
    );

    let late = recorder
        .reserve(&instance, BrowserRecordingActor::Agent)
        .expect("reserve late action");
    let review = recorder.stop(&instance).expect("stop");
    assert_eq!(review.recipe().steps.len(), 1);
    assert_eq!(
        recorder
            .commit(late, navigate("https://example.test/late"))
            .expect("late completion is ignored"),
        BrowserRecordingCommit::Ignored
    );

    recorder.discard(&instance).expect("discard review");
    let replacement = recorder.start(workspace).expect("restart");
    assert_ne!(replacement.id(), instance.id());
    assert_eq!(recorder.active_step_count(&replacement).unwrap(), 0);
}

#[test]
fn stop_cancels_unresolved_slots_but_keeps_later_successes_completed_in_time() {
    let workspace = workspace("project-a", "ai-a");
    let mut recorder = BrowserWorkflowRecorder::default();
    let instance = recorder.start(workspace).expect("start");
    let unresolved = recorder
        .reserve(&instance, BrowserRecordingActor::User)
        .expect("reserve unresolved action");
    let completed = recorder
        .reserve(&instance, BrowserRecordingActor::Agent)
        .expect("reserve completed action");
    assert_eq!(
        recorder
            .commit(completed, navigate("https://example.test/completed"))
            .expect("complete later action"),
        BrowserRecordingCommit::Buffered
    );

    let review = recorder.stop(&instance).expect("stop");
    assert_eq!(review.recipe().steps.len(), 1);
    assert!(matches!(
        &review.recipe().steps[0].action,
        BrowserRecipeAction::Navigate {
            url: BrowserRecipeValue::Literal { value },
        } if value == "https://example.test/completed"
    ));
    assert_eq!(
        recorder
            .commit(unresolved, navigate("https://example.test/late"))
            .expect("late unresolved action is ignored"),
        BrowserRecordingCommit::Ignored
    );
}

fn navigate(url: &str) -> BrowserRecordingAction {
    BrowserRecordingAction::navigate(url).expect("safe navigation")
}

fn locator(test_id: &str) -> BrowserRecipeLocator {
    BrowserRecipeLocator {
        test_id: Some(test_id.to_string()),
        ..BrowserRecipeLocator::default()
    }
}

fn commit_text(
    recorder: &mut BrowserWorkflowRecorder,
    instance: &BrowserRecordingInstance,
    actor: BrowserRecordingActor,
    tab_id: &str,
    risk: BrowserRisk,
    target: BrowserRecipeLocator,
    value: &str,
    wait: bool,
    assertion: bool,
) {
    let ticket = recorder
        .reserve_on(instance, actor, tab_id, risk)
        .expect("reserve text");
    let mut action = BrowserRecordingAction::type_text(target, value).expect("record text");
    if wait {
        action = action
            .with_wait(BrowserRecipeWait::Duration { duration_ms: 10 })
            .expect("safe wait");
    }
    if assertion {
        action = action
            .with_assertions(vec![BrowserRecipeAssertion::Title {
                value: BrowserRecipeValue::Literal {
                    value: "Ready".to_string(),
                },
                exact: true,
            }])
            .expect("safe assertion");
    }
    recorder.commit(ticket, action).expect("commit text");
}

#[test]
fn recorder_is_explicit_orders_async_commits_and_fences_workspace_instances() {
    let workspace_a = workspace("project-a", "ai-a");
    let workspace_b = workspace("project-b", "ai-b");
    let mut recorder = BrowserWorkflowRecorder::default();

    assert_eq!(
        recorder.status(&workspace_a),
        BrowserRecordingStatus::Inactive
    );

    let instance_a = recorder.start(workspace_a.clone()).expect("start A");
    let first = recorder
        .reserve(&instance_a, BrowserRecordingActor::User)
        .expect("reserve first action");
    let second = recorder
        .reserve(&instance_a, BrowserRecordingActor::Agent)
        .expect("reserve second action");
    recorder
        .commit(second, navigate("https://example.test/second"))
        .expect("commit second completion first");
    recorder
        .commit(first, navigate("https://example.test/first"))
        .expect("commit first completion last");

    let instance_b = recorder.start(workspace_b.clone()).expect("start B");
    let only_b = recorder
        .reserve(&instance_b, BrowserRecordingActor::Agent)
        .expect("reserve B");
    recorder
        .commit(only_b, navigate("https://other.test/only-b"))
        .expect("commit B");

    let review_a = recorder.stop(&instance_a).expect("review A");
    let urls = review_a
        .recipe()
        .steps
        .iter()
        .map(|step| match &step.action {
            BrowserRecipeAction::Navigate {
                url: BrowserRecipeValue::Literal { value },
            } => value.as_str(),
            action => panic!("unexpected action: {action:?}"),
        })
        .collect::<Vec<_>>();
    assert_eq!(
        urls,
        vec!["https://example.test/first", "https://example.test/second"]
    );
    assert_eq!(recorder.active_step_count(&instance_b).unwrap(), 1);

    recorder.discard(&instance_a).expect("discard A review");
    let replacement_a = recorder.start(workspace_a).expect("restart A");
    assert_ne!(replacement_a.id(), instance_a.id());
    assert_eq!(
        recorder
            .reserve(&instance_a, BrowserRecordingActor::User)
            .unwrap_err(),
        BrowserRecordingError::StaleInstance
    );
}

#[test]
fn coalescing_and_redaction_produce_only_safe_unset_inputs() {
    let workspace_a = workspace("project-a", "ai-a");
    let mut recorder = BrowserWorkflowRecorder::default();
    let instance = recorder.start(workspace_a).expect("start");
    let user_name = locator("user-name");

    for value in ["hel", "hello"] {
        let ticket = recorder
            .reserve_on(
                &instance,
                BrowserRecordingActor::User,
                "page-a",
                BrowserRisk::Normal,
            )
            .expect("reserve typing");
        recorder
            .commit(
                ticket,
                BrowserRecordingAction::type_text(user_name.clone(), value).expect("safe text"),
            )
            .expect("commit typing");
    }

    let password = recorder
        .reserve_on(
            &instance,
            BrowserRecordingActor::User,
            "page-a",
            BrowserRisk::AccountSecurity,
        )
        .expect("reserve password");
    recorder
        .commit(
            password,
            BrowserRecordingAction::type_password(locator("password")).expect("password marker"),
        )
        .expect("commit password marker");

    let token = recorder
        .reserve_on(
            &instance,
            BrowserRecordingActor::Agent,
            "page-a",
            BrowserRisk::AccountSecurity,
        )
        .expect("reserve token-like text");
    recorder
        .commit(
            token,
            BrowserRecordingAction::type_text(
                locator("api-value"),
                "authorization=Bearer recorder-token-sentinel",
            )
            .expect("sensitive text is promoted"),
        )
        .expect("commit promoted secret");

    let upload = recorder
        .reserve_on(
            &instance,
            BrowserRecordingActor::User,
            "page-a",
            BrowserRisk::OutsideWorkspaceFile,
        )
        .expect("reserve upload");
    recorder
        .commit(
            upload,
            BrowserRecordingAction::upload(locator("resume-upload")).expect("file marker"),
        )
        .expect("commit file marker");

    let navigation = recorder
        .reserve_on(
            &instance,
            BrowserRecordingActor::User,
            "page-a",
            BrowserRisk::Normal,
        )
        .expect("reserve navigation");
    recorder
        .commit(
            navigation,
            BrowserRecordingAction::navigate(
                "https://example.test/results?token=url-token-sentinel&view=compact",
            )
            .expect("sanitize navigation"),
        )
        .expect("commit navigation");

    let review = recorder.stop(&instance).expect("review");
    let recipe = review.recipe();
    assert_eq!(recipe.steps.len(), 5, "two adjacent input events coalesce");
    assert_eq!(
        recipe
            .inputs
            .iter()
            .map(|input| input.kind)
            .collect::<Vec<_>>(),
        vec![
            BrowserRecipeInputKind::Secret,
            BrowserRecipeInputKind::Secret,
            BrowserRecipeInputKind::File,
        ]
    );
    assert!(recipe
        .inputs
        .iter()
        .all(|input| input.default_value.is_none()));
    assert!(matches!(
        &recipe.steps[0].action,
        BrowserRecipeAction::Type {
            value: BrowserRecipeValue::Literal { value },
            ..
        } if value == "hello"
    ));
    assert!(matches!(
        &recipe.steps[1].action,
        BrowserRecipeAction::Type {
            value: BrowserRecipeValue::Input { .. },
            ..
        }
    ));
    assert!(matches!(
        &recipe.steps[3].action,
        BrowserRecipeAction::Upload {
            file: BrowserRecipeValue::Input { .. },
            ..
        }
    ));
    assert!(matches!(
        &recipe.steps[4].action,
        BrowserRecipeAction::Navigate {
            url: BrowserRecipeValue::Literal { value },
        } if value == "https://example.test/results?view=compact"
    ));

    recipe.validate().expect("recorded draft validates as v1");
    let json = serde_json::to_string(recipe).expect("serialize safe preview");
    for forbidden in [
        "recorder-token-sentinel",
        "url-token-sentinel",
        "authorization=Bearer",
        "passwordValue",
        "fileContents",
    ] {
        assert!(!json.contains(forbidden), "preview leaked {forbidden}");
    }
}

#[test]
fn coalescing_never_crosses_actor_tab_locator_risk_wait_or_assertion_boundaries() {
    let workspace_a = workspace("project-a", "ai-a");
    let mut recorder = BrowserWorkflowRecorder::default();
    let instance = recorder.start(workspace_a).expect("start");
    let first = locator("first");
    let second = locator("second");

    commit_text(
        &mut recorder,
        &instance,
        BrowserRecordingActor::User,
        "page-a",
        BrowserRisk::Normal,
        first.clone(),
        "one",
        false,
        false,
    );
    commit_text(
        &mut recorder,
        &instance,
        BrowserRecordingActor::Agent,
        "page-a",
        BrowserRisk::Normal,
        first.clone(),
        "two",
        false,
        false,
    );
    commit_text(
        &mut recorder,
        &instance,
        BrowserRecordingActor::Agent,
        "page-b",
        BrowserRisk::Normal,
        first,
        "three",
        false,
        false,
    );
    commit_text(
        &mut recorder,
        &instance,
        BrowserRecordingActor::Agent,
        "page-b",
        BrowserRisk::Normal,
        second.clone(),
        "four",
        false,
        false,
    );
    commit_text(
        &mut recorder,
        &instance,
        BrowserRecordingActor::Agent,
        "page-b",
        BrowserRisk::Financial,
        second.clone(),
        "five",
        false,
        false,
    );
    commit_text(
        &mut recorder,
        &instance,
        BrowserRecordingActor::Agent,
        "page-b",
        BrowserRisk::Financial,
        second.clone(),
        "six",
        true,
        false,
    );
    for (value, assertion) in [
        ("seven", false),
        ("eight", false),
        ("nine", true),
        ("ten", false),
        ("eleven", false),
    ] {
        commit_text(
            &mut recorder,
            &instance,
            BrowserRecordingActor::Agent,
            "page-b",
            BrowserRisk::Financial,
            second.clone(),
            value,
            false,
            assertion,
        );
    }

    let review = recorder.stop(&instance).expect("review boundaries");
    assert_eq!(review.recipe().steps.len(), 9);
    assert_eq!(
        review
            .recipe()
            .steps
            .iter()
            .map(|step| step.id.as_str())
            .collect::<Vec<_>>(),
        vec![
            "step-1", "step-2", "step-3", "step-4", "step-5", "step-6", "step-7", "step-9",
            "step-10",
        ]
    );

    let workspace_b = workspace("project-a", "ai-b");
    let instance = recorder.start(workspace_b).expect("start safe coalescing");
    for value in ["temporary", "", "final"] {
        commit_text(
            &mut recorder,
            &instance,
            BrowserRecordingActor::User,
            "page-a",
            BrowserRisk::Normal,
            locator("search"),
            value,
            false,
            false,
        );
    }
    for _ in 0..2 {
        let ticket = recorder
            .reserve_on(
                &instance,
                BrowserRecordingActor::User,
                "page-a",
                BrowserRisk::Normal,
            )
            .expect("reserve duplicate navigation");
        recorder
            .commit(ticket, navigate("https://example.test/final"))
            .expect("commit duplicate navigation");
    }
    let review = recorder.stop(&instance).expect("review safe coalescing");
    assert_eq!(review.recipe().steps.len(), 2);
    assert_eq!(review.recipe().steps[0].id, "step-1");
    assert!(matches!(
        &review.recipe().steps[0].action,
        BrowserRecipeAction::Type {
            value: BrowserRecipeValue::Literal { value },
            ..
        } if value == "final"
    ));
    assert_eq!(review.recipe().steps[1].id, "step-4");
}

#[test]
fn review_mutations_are_immutable_validated_and_discardable_without_saving() {
    let workspace = workspace("project-a", "ai-a");
    let mut recorder = BrowserWorkflowRecorder::default();
    let instance = recorder.start(workspace.clone()).expect("start");

    commit_text(
        &mut recorder,
        &instance,
        BrowserRecordingActor::User,
        "page-a",
        BrowserRisk::Normal,
        locator("search"),
        "alpha",
        false,
        false,
    );
    let click = recorder
        .reserve(&instance, BrowserRecordingActor::Agent)
        .expect("reserve click");
    recorder
        .commit(
            click,
            BrowserRecordingAction::recipe(BrowserRecipeAction::Click {
                locator: locator("exploratory"),
            })
            .expect("safe click"),
        )
        .expect("commit click");
    let navigation = recorder
        .reserve(&instance, BrowserRecordingActor::Agent)
        .expect("reserve navigation");
    recorder
        .commit(navigation, navigate("https://example.test/results"))
        .expect("commit navigation");

    let original = recorder.stop(&instance).expect("stop");
    assert_eq!(original.recipe().steps.len(), 3);
    recorder
        .set_metadata(
            &instance,
            BrowserRecordingMetadata {
                id: "search-flow".to_string(),
                name: "Search flow".to_string(),
                description: "Search and verify results".to_string(),
                start_url: "https://example.test/start".to_string(),
                viewport: BrowserRecipeViewport {
                    width: 1440,
                    height: 900,
                    scale_percent: 100,
                },
            },
        )
        .expect("metadata");
    recorder
        .delete_step(&instance, "step-2")
        .expect("delete exploratory click");
    recorder
        .move_step(&instance, "step-3", 0)
        .expect("move navigation first");
    recorder
        .convert_action_value_to_input(
            &instance,
            "step-3",
            "destination",
            BrowserRecipeInputKind::Url,
        )
        .expect("convert URL literal");
    recorder
        .convert_action_value_to_input(&instance, "step-1", "query", BrowserRecipeInputKind::Text)
        .expect("convert text literal");
    recorder
        .rename_input(&instance, "query", "search_text")
        .expect("rename input and references");
    recorder
        .set_input_default(&instance, "search_text", Some("updated".to_string()))
        .expect("edit ordinary default");
    recorder
        .set_step_wait(
            &instance,
            "step-3",
            Some(BrowserRecipeWait::Load { timeout_ms: 2_000 }),
        )
        .expect("edit wait");
    recorder
        .add_step_assertion(
            &instance,
            "step-3",
            BrowserRecipeAssertion::Url {
                value: BrowserRecipeValue::Input {
                    name: "destination".to_string(),
                },
                exact: true,
            },
        )
        .expect("add URL assertion");
    recorder
        .add_step_assertion(
            &instance,
            "step-3",
            BrowserRecipeAssertion::Title {
                value: BrowserRecipeValue::Literal {
                    value: "Results".to_string(),
                },
                exact: false,
            },
        )
        .expect("add title assertion");
    recorder
        .remove_step_assertion(&instance, "step-3", 1)
        .expect("remove title assertion");

    let current = recorder.review(&instance).expect("fresh immutable preview");
    assert_eq!(original.recipe().id, "recording-1");
    assert_eq!(original.recipe().steps.len(), 3);
    assert_eq!(current.recipe().id, "search-flow");
    assert_eq!(current.recipe().steps.len(), 2);
    assert_eq!(current.recipe().steps[0].id, "step-3");
    assert_eq!(current.recipe().inputs.len(), 2);
    assert!(current
        .recipe()
        .inputs
        .iter()
        .any(|input| input.name == "search_text"
            && input.default_value.as_deref() == Some("updated")));

    let recipe = recorder
        .recipe_for_save(&instance)
        .expect("validated handoff");
    recipe.validate().expect("BrowserRecipeV1 validation");
    assert!(matches!(
        &recipe.steps[0].action,
        BrowserRecipeAction::Navigate {
            url: BrowserRecipeValue::Input { name },
        } if name == "destination"
    ));

    let invalid_secret = recorder.add_input(
        &instance,
        BrowserRecipeInput {
            name: "password".to_string(),
            kind: BrowserRecipeInputKind::Secret,
            default_value: Some("review-secret-sentinel".to_string()),
        },
    );
    assert!(matches!(
        invalid_secret,
        Err(BrowserRecordingError::InvalidMutation)
    ));
    assert!(matches!(
        recorder.remove_input(&instance, "destination"),
        Err(BrowserRecordingError::InvalidMutation)
    ));
    let json = serde_json::to_string(recorder.review(&instance).unwrap().recipe()).unwrap();
    assert!(!json.contains("review-secret-sentinel"));

    recorder.discard(&instance).expect("discard review");
    assert_eq!(
        recorder.status(&workspace),
        BrowserRecordingStatus::Inactive
    );
    assert!(matches!(
        recorder.recipe_for_save(&instance),
        Err(BrowserError::InvalidRecipe { .. })
    ));

    let invalid = recorder.start(workspace).expect("start invalid review");
    let ticket = recorder
        .reserve(&invalid, BrowserRecordingActor::User)
        .expect("reserve invalid review step");
    recorder
        .commit(ticket, navigate("https://example.test"))
        .expect("commit invalid review step");
    recorder.stop(&invalid).expect("stop invalid review");
    recorder
        .set_metadata(
            &invalid,
            BrowserRecordingMetadata {
                id: "../unsafe".to_string(),
                name: "Unsafe".to_string(),
                description: String::new(),
                start_url: "about:blank".to_string(),
                viewport: BrowserRecipeViewport::default(),
            },
        )
        .expect("editing can expose a validation error");
    assert!(matches!(
        recorder.recipe_for_save(&invalid),
        Err(BrowserError::InvalidRecipe { .. })
    ));
}

#[test]
fn cookie_token_and_clipboard_values_never_enter_recording_state() {
    let workspace = workspace("project-a", "ai-a");
    let mut recorder = BrowserWorkflowRecorder::default();
    let instance = recorder.start(workspace).expect("start");

    for (target, value) in [
        ("cookie-field", "cookie=session-cookie-sentinel"),
        ("token-field", "api_key=api-token-sentinel"),
    ] {
        let ticket = recorder
            .reserve(&instance, BrowserRecordingActor::User)
            .expect("reserve sensitive input");
        recorder
            .commit(
                ticket,
                BrowserRecordingAction::type_text(locator(target), value)
                    .expect("promote sensitive input"),
            )
            .expect("commit sensitive marker");
    }
    let clipboard = recorder
        .reserve(&instance, BrowserRecordingActor::User)
        .expect("reserve clipboard input");
    recorder
        .commit(
            clipboard,
            BrowserRecordingAction::type_clipboard(locator("paste-target"))
                .expect("clipboard marker accepts no contents"),
        )
        .expect("commit clipboard marker");

    let review = recorder.stop(&instance).expect("review");
    assert_eq!(review.recipe().inputs.len(), 3);
    assert!(
        review
            .recipe()
            .inputs
            .iter()
            .all(|input| input.kind == BrowserRecipeInputKind::Secret
                && input.default_value.is_none())
    );
    let json = serde_json::to_string(review.recipe()).expect("safe preview");
    for forbidden in [
        "session-cookie-sentinel",
        "api-token-sentinel",
        "cookie=session",
        "api_key=",
        "clipboardContents",
    ] {
        assert!(!json.contains(forbidden), "preview leaked {forbidden}");
    }
}

#[test]
fn sensitive_typing_coalesces_without_allocating_orphan_inputs() {
    let workspace = workspace("project-a", "ai-a");
    let mut recorder = BrowserWorkflowRecorder::default();
    let instance = recorder.start(workspace).expect("start");
    for actor in [
        BrowserRecordingActor::User,
        BrowserRecordingActor::User,
        BrowserRecordingActor::Agent,
    ] {
        let ticket = recorder
            .reserve_on(&instance, actor, "page-a", BrowserRisk::AccountSecurity)
            .expect("reserve password marker");
        recorder
            .commit(
                ticket,
                BrowserRecordingAction::type_password(locator("password"))
                    .expect("password marker"),
            )
            .expect("commit password marker");
    }

    let review = recorder.stop(&instance).expect("review");
    assert_eq!(review.recipe().steps.len(), 2, "actor change is a boundary");
    assert_eq!(review.recipe().inputs.len(), 2, "no orphan secret input");
    review.recipe().validate().expect("valid coalesced recipe");
}
