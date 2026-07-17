use devmanager::browser::{
    BrowserError, BrowserRecipeAction, BrowserRecipeAssertion, BrowserRecipeInput,
    BrowserRecipeInputKind, BrowserRecipeLocator, BrowserRecipeValue, BrowserRecipeViewport,
    BrowserRecipeWait, BrowserRecordingAction, BrowserRecordingActor, BrowserRecordingCommit,
    BrowserRecordingError, BrowserRecordingInstance, BrowserRecordingMetadata,
    BrowserRecordingStatus, BrowserRisk, BrowserWorkflowRecorder, BrowserWorkspaceKey,
    MAX_BROWSER_RECORDING_ASSERTIONS, MAX_BROWSER_RECORDING_ASSERTIONS_PER_ACTION,
    MAX_BROWSER_RECORDING_INPUTS,
};
use static_assertions::assert_not_impl_any;

assert_not_impl_any!(BrowserRecordingAction: std::fmt::Debug, serde::Serialize);
assert_not_impl_any!(BrowserRecordingMetadata: std::fmt::Debug, serde::Serialize);
assert_not_impl_any!(devmanager::browser::BrowserRecordingReview: std::fmt::Debug, serde::Serialize);
assert_not_impl_any!(BrowserWorkflowRecorder: std::fmt::Debug, serde::Serialize);

fn workspace(project: &str, tab: &str) -> BrowserWorkspaceKey {
    BrowserWorkspaceKey::new(project, tab).expect("valid workspace")
}

fn review_input_names(review: &devmanager::browser::BrowserRecordingReview) -> Vec<&str> {
    review
        .recipe()
        .inputs
        .iter()
        .map(|input| input.name.as_str())
        .collect()
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

#[test]
fn review_hardening_rejects_encoded_secrets_or_unbounded_invalid_state() {
    let mut defects = Vec::new();

    let mut url_recorder = BrowserWorkflowRecorder::default();
    let url_instance = url_recorder
        .start(workspace("url-project", "url-tab"))
        .expect("start URL recording");
    for url in [
        "https://example.test/path?keep=ok&%74oken=query-token-sentinel&%2561uthorization=double-authorization-sentinel&%73ession=session-sentinel#route?%63ookie=fragment-cookie-sentinel&%2570assword=fragment-password-sentinel",
        "https://example.test/path?q=hello%20world#section-2",
    ] {
        let reservation = url_recorder
            .reserve(&url_instance, BrowserRecordingActor::User)
            .expect("reserve URL action");
        url_recorder
            .commit(
                reservation,
                BrowserRecordingAction::navigate(url).expect("construct encoded URL action"),
            )
            .expect("commit URL action");
    }
    let url_review = url_recorder
        .stop(&url_instance)
        .expect("stop URL recording");
    let url_json = serde_json::to_string(url_review.recipe()).expect("serialize safe URL review");
    let safe_url_preserved = matches!(
        &url_review.recipe().steps[1].action,
        BrowserRecipeAction::Navigate {
            url: BrowserRecipeValue::Literal { value },
        } if value == "https://example.test/path?q=hello%20world#section-2"
    );
    let invalid_encoding_rejected = matches!(
        BrowserRecordingAction::navigate("https://example.test/?%ZZoken=invalid-sentinel"),
        Err(BrowserRecordingError::InvalidAction)
    );
    if [
        "query-token-sentinel",
        "double-authorization-sentinel",
        "session-sentinel",
        "fragment-cookie-sentinel",
        "fragment-password-sentinel",
        "%74oken",
        "%2561uthorization",
        "%73ession",
        "%63ookie",
        "%2570assword",
    ]
    .iter()
    .any(|sentinel| url_json.contains(sentinel))
        || !safe_url_preserved
        || !invalid_encoding_rejected
    {
        defects.push("encoded URL credentials");
    }

    let mut delete_recorder = BrowserWorkflowRecorder::default();
    let delete_instance = delete_recorder
        .start(workspace("delete-project", "delete-tab"))
        .expect("start delete recording");
    let actions = [
        BrowserRecordingAction::type_password(locator("password")).expect("password marker"),
        BrowserRecordingAction::upload(locator("upload")).expect("file marker"),
        BrowserRecordingAction::type_text(locator("query"), "alpha").expect("literal text"),
        navigate("https://example.test/destination"),
        BrowserRecordingAction::recipe(BrowserRecipeAction::Click {
            locator: locator("keeper"),
        })
        .expect("keeper click"),
    ];
    for action in actions {
        let reservation = delete_recorder
            .reserve(&delete_instance, BrowserRecordingActor::User)
            .expect("reserve generated-input action");
        delete_recorder
            .commit(reservation, action)
            .expect("commit generated-input action");
    }
    delete_recorder
        .stop(&delete_instance)
        .expect("stop delete recording");
    delete_recorder
        .convert_action_value_to_input(
            &delete_instance,
            "step-3",
            "generated_text",
            BrowserRecipeInputKind::Text,
        )
        .expect("generate text input");
    delete_recorder
        .convert_action_value_to_input(
            &delete_instance,
            "step-4",
            "generated_url",
            BrowserRecipeInputKind::Url,
        )
        .expect("generate URL input");
    delete_recorder
        .set_step_wait(
            &delete_instance,
            "step-5",
            Some(BrowserRecipeWait::Url {
                value: BrowserRecipeValue::Input {
                    name: "generated_url".to_string(),
                },
                exact: true,
                timeout_ms: 1_000,
            }),
        )
        .expect("share generated URL input");
    delete_recorder
        .add_input(
            &delete_instance,
            BrowserRecipeInput {
                name: "manual_input".to_string(),
                kind: BrowserRecipeInputKind::Text,
                default_value: Some("manual".to_string()),
            },
        )
        .expect("add explicit review input");
    for step_id in ["step-1", "step-2", "step-3", "step-4"] {
        delete_recorder
            .delete_step(&delete_instance, step_id)
            .expect("delete generated-input step");
    }
    let delete_review = delete_recorder
        .review(&delete_instance)
        .expect("review generated-input collection");
    let remaining_names = delete_review
        .recipe()
        .inputs
        .iter()
        .map(|input| input.name.as_str())
        .collect::<Vec<_>>();
    if remaining_names != ["generated_url", "manual_input"]
        || delete_recorder.recipe_for_save(&delete_instance).is_err()
    {
        defects.push("generated input garbage collection");
    }

    let unresolved_generic = BrowserRecordingAction::recipe(BrowserRecipeAction::Navigate {
        url: BrowserRecipeValue::Input {
            name: "missing_url".to_string(),
        },
    });
    if !matches!(
        unresolved_generic,
        Err(BrowserRecordingError::InvalidAction)
    ) || BrowserRecordingAction::navigate("https://example.test/literal").is_err()
        || BrowserRecordingAction::type_password(locator("normal-password-marker")).is_err()
        || BrowserRecordingAction::upload(locator("normal-upload-marker")).is_err()
    {
        defects.push("unresolved generic input capture");
    }

    let title_assertion = || BrowserRecipeAssertion::Title {
        value: BrowserRecipeValue::Literal {
            value: "Ready".to_string(),
        },
        exact: true,
    };
    let action_assertion_overflow = BrowserRecordingAction::recipe(BrowserRecipeAction::Click {
        locator: locator("bounded-action"),
    })
    .expect("bounded action")
    .with_assertions(vec![
        title_assertion();
        MAX_BROWSER_RECORDING_ASSERTIONS_PER_ACTION + 1
    ]);
    let mut bounds_failed = !matches!(
        action_assertion_overflow,
        Err(BrowserRecordingError::CapacityExceeded)
    );

    let mut input_recorder = BrowserWorkflowRecorder::default();
    let input_instance = input_recorder
        .start(workspace("input-cap-project", "input-cap-tab"))
        .expect("start input-cap recording");
    let input_step = input_recorder
        .reserve(&input_instance, BrowserRecordingActor::User)
        .expect("reserve input-cap step");
    input_recorder
        .commit(input_step, navigate("https://example.test/input-cap"))
        .expect("commit input-cap step");
    input_recorder
        .stop(&input_instance)
        .expect("stop input-cap recording");
    for index in 0..MAX_BROWSER_RECORDING_INPUTS {
        input_recorder
            .add_input(
                &input_instance,
                BrowserRecipeInput {
                    name: format!("manual_{index}"),
                    kind: BrowserRecipeInputKind::Text,
                    default_value: None,
                },
            )
            .expect("fill review input capacity");
    }
    let input_overflow = input_recorder.add_input(
        &input_instance,
        BrowserRecipeInput {
            name: "manual_overflow".to_string(),
            kind: BrowserRecipeInputKind::Text,
            default_value: None,
        },
    );
    let conversion_overflow = input_recorder.convert_action_value_to_input(
        &input_instance,
        "step-1",
        "converted_overflow",
        BrowserRecipeInputKind::Url,
    );
    let bounded_input_review = input_recorder
        .review(&input_instance)
        .expect("review atomic input rejection");
    bounds_failed |= !matches!(input_overflow, Err(BrowserRecordingError::CapacityExceeded))
        || !matches!(
            conversion_overflow,
            Err(BrowserRecordingError::CapacityExceeded)
        )
        || bounded_input_review.recipe().inputs.len() != MAX_BROWSER_RECORDING_INPUTS
        || !matches!(
            &bounded_input_review.recipe().steps[0].action,
            BrowserRecipeAction::Navigate {
                url: BrowserRecipeValue::Literal { .. }
            }
        );

    let mut assertion_recorder = BrowserWorkflowRecorder::default();
    let assertion_instance = assertion_recorder
        .start(workspace("assertion-cap-project", "assertion-cap-tab"))
        .expect("start assertion-cap recording");
    for index in
        0..=(MAX_BROWSER_RECORDING_ASSERTIONS / MAX_BROWSER_RECORDING_ASSERTIONS_PER_ACTION)
    {
        let reservation = assertion_recorder
            .reserve(&assertion_instance, BrowserRecordingActor::User)
            .expect("reserve assertion-cap step");
        assertion_recorder
            .commit(
                reservation,
                BrowserRecordingAction::recipe(BrowserRecipeAction::Click {
                    locator: locator(&format!("assertion-step-{index}")),
                })
                .expect("assertion-cap click"),
            )
            .expect("commit assertion-cap step");
    }
    assertion_recorder
        .stop(&assertion_instance)
        .expect("stop assertion-cap recording");
    for step_index in
        0..(MAX_BROWSER_RECORDING_ASSERTIONS / MAX_BROWSER_RECORDING_ASSERTIONS_PER_ACTION)
    {
        for _ in 0..MAX_BROWSER_RECORDING_ASSERTIONS_PER_ACTION {
            assertion_recorder
                .add_step_assertion(
                    &assertion_instance,
                    &format!("step-{}", step_index + 1),
                    title_assertion(),
                )
                .expect("fill total assertion capacity");
        }
    }
    let assertion_overflow = assertion_recorder.add_step_assertion(
        &assertion_instance,
        &format!(
            "step-{}",
            (MAX_BROWSER_RECORDING_ASSERTIONS / MAX_BROWSER_RECORDING_ASSERTIONS_PER_ACTION) + 1
        ),
        title_assertion(),
    );
    let bounded_assertion_review = assertion_recorder
        .review(&assertion_instance)
        .expect("review atomic assertion rejection");
    bounds_failed |= !matches!(
        assertion_overflow,
        Err(BrowserRecordingError::CapacityExceeded)
    ) || bounded_assertion_review
        .recipe()
        .steps
        .iter()
        .map(|step| step.assertions.len())
        .sum::<usize>()
        != MAX_BROWSER_RECORDING_ASSERTIONS;

    let mut generated_recorder =
        BrowserWorkflowRecorder::with_capacity(MAX_BROWSER_RECORDING_INPUTS + 1);
    let generated_instance = generated_recorder
        .start(workspace("generated-cap-project", "generated-cap-tab"))
        .expect("start generated-cap recording");
    for index in 0..MAX_BROWSER_RECORDING_INPUTS {
        let reservation = generated_recorder
            .reserve(&generated_instance, BrowserRecordingActor::User)
            .expect("reserve generated input");
        generated_recorder
            .commit(
                reservation,
                BrowserRecordingAction::type_password(locator(&format!("password-{index}")))
                    .expect("generated secret marker"),
            )
            .expect("fill generated input capacity");
    }
    let generated_overflow_reservation = generated_recorder
        .reserve(&generated_instance, BrowserRecordingActor::User)
        .expect("reserve generated overflow");
    let generated_overflow = generated_recorder.commit(
        generated_overflow_reservation,
        BrowserRecordingAction::type_password(locator("password-overflow"))
            .expect("generated overflow marker"),
    );
    let generated_review = generated_recorder
        .stop(&generated_instance)
        .expect("stop generated-cap recording");
    bounds_failed |= !matches!(
        generated_overflow,
        Err(BrowserRecordingError::CapacityExceeded)
    ) || generated_review.recipe().inputs.len() != MAX_BROWSER_RECORDING_INPUTS
        || generated_review.recipe().steps.len() != MAX_BROWSER_RECORDING_INPUTS;

    if bounds_failed {
        defects.push("retained collection capacity and atomicity");
    }

    assert!(
        defects.is_empty(),
        "unfixed recording review findings: {}",
        defects.join(", ")
    );
}

#[test]
fn generated_input_gc_follows_successful_reference_mutations_atomically() {
    let mut recorder = BrowserWorkflowRecorder::default();
    let instance = recorder
        .start(workspace("gc-project", "gc-tab"))
        .expect("start GC recording");
    let actions = [
        navigate("https://example.test/remove-wait"),
        BrowserRecordingAction::recipe(BrowserRecipeAction::Click {
            locator: locator("remove-wait-keeper"),
        })
        .expect("remove-wait keeper"),
        navigate("https://example.test/replace-wait"),
        BrowserRecordingAction::recipe(BrowserRecipeAction::Click {
            locator: locator("replace-wait-keeper"),
        })
        .expect("replace-wait keeper"),
        BrowserRecordingAction::type_text(locator("assertion-source"), "expected title")
            .expect("assertion source"),
        BrowserRecordingAction::recipe(BrowserRecipeAction::Click {
            locator: locator("assertion-keeper"),
        })
        .expect("assertion keeper"),
    ];
    for action in actions {
        let reservation = recorder
            .reserve(&instance, BrowserRecordingActor::User)
            .expect("reserve GC action");
        recorder
            .commit(reservation, action)
            .expect("commit GC action");
    }
    recorder.stop(&instance).expect("stop GC recording");

    for (step_id, input_name, kind) in [
        ("step-1", "wait_remove", BrowserRecipeInputKind::Url),
        ("step-3", "wait_replace", BrowserRecipeInputKind::Url),
        ("step-5", "assertion_text", BrowserRecipeInputKind::Text),
    ] {
        recorder
            .convert_action_value_to_input(&instance, step_id, input_name, kind)
            .expect("generate shared input");
    }
    recorder
        .set_step_wait(
            &instance,
            "step-2",
            Some(BrowserRecipeWait::Url {
                value: BrowserRecipeValue::Input {
                    name: "wait_remove".to_string(),
                },
                exact: true,
                timeout_ms: 1_000,
            }),
        )
        .expect("share removal wait input");
    recorder
        .set_step_wait(
            &instance,
            "step-4",
            Some(BrowserRecipeWait::Url {
                value: BrowserRecipeValue::Input {
                    name: "wait_replace".to_string(),
                },
                exact: true,
                timeout_ms: 1_000,
            }),
        )
        .expect("share replacement wait input");
    recorder
        .add_step_assertion(
            &instance,
            "step-6",
            BrowserRecipeAssertion::Title {
                value: BrowserRecipeValue::Input {
                    name: "assertion_text".to_string(),
                },
                exact: true,
            },
        )
        .expect("share assertion input");
    recorder
        .add_input(
            &instance,
            BrowserRecipeInput {
                name: "manual_input".to_string(),
                kind: BrowserRecipeInputKind::Text,
                default_value: Some("manual".to_string()),
            },
        )
        .expect("add explicit review input");

    for step_id in ["step-1", "step-3", "step-5"] {
        recorder
            .delete_step(&instance, step_id)
            .expect("delete generated-input source step");
    }

    let mut defects = Vec::new();
    let shared = recorder.review(&instance).expect("shared review");
    if review_input_names(&shared)
        != [
            "wait_remove",
            "wait_replace",
            "assertion_text",
            "manual_input",
        ]
    {
        defects.push("shared generated inputs were not preserved");
    }

    let before_invalid_wait = recorder.review(&instance).expect("before invalid wait");
    let invalid_wait = recorder.set_step_wait(
        &instance,
        "step-4",
        Some(BrowserRecipeWait::Duration { duration_ms: 0 }),
    );
    let after_invalid_wait = recorder.review(&instance).expect("after invalid wait");
    if !matches!(invalid_wait, Err(BrowserRecordingError::InvalidMutation))
        || before_invalid_wait != after_invalid_wait
    {
        defects.push("failed wait mutation was not atomic");
    }

    let before_invalid_assertion = recorder
        .review(&instance)
        .expect("before invalid assertion removal");
    let invalid_assertion = recorder.remove_step_assertion(&instance, "step-6", 1);
    let after_invalid_assertion = recorder
        .review(&instance)
        .expect("after invalid assertion removal");
    if !matches!(
        invalid_assertion,
        Err(BrowserRecordingError::InvalidMutation)
    ) || before_invalid_assertion != after_invalid_assertion
    {
        defects.push("failed assertion mutation was not atomic");
    }

    let after_wait_removal = recorder
        .set_step_wait(&instance, "step-2", None)
        .expect("remove final wait reference");
    if review_input_names(&after_wait_removal) != ["wait_replace", "assertion_text", "manual_input"]
    {
        defects.push("wait removal left a generated input orphan");
    }

    let after_wait_replacement = recorder
        .set_step_wait(
            &instance,
            "step-4",
            Some(BrowserRecipeWait::Load { timeout_ms: 1_000 }),
        )
        .expect("replace final wait reference");
    if review_input_names(&after_wait_replacement) != ["assertion_text", "manual_input"] {
        defects.push("wait replacement left a generated input orphan");
    }

    let after_assertion_removal = recorder
        .remove_step_assertion(&instance, "step-6", 0)
        .expect("remove final assertion reference");
    if review_input_names(&after_assertion_removal) != ["manual_input"]
        || recorder.recipe_for_save(&instance).is_err()
    {
        defects.push("assertion removal left a generated input orphan");
    }

    assert!(
        defects.is_empty(),
        "unfixed generated-input lifecycle findings: {}",
        defects.join(", ")
    );
}
