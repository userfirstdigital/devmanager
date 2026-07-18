use devmanager::browser::{
    compile_browser_replay, BrowserRecipeAction, BrowserRecipeInput, BrowserRecipeInputKind,
    BrowserRecipeLocator, BrowserRecipeStep, BrowserRecipeV1, BrowserRecipeValue,
    BrowserRecipeViewport, BrowserReplayCancellationLease, BrowserReplayCoordinator,
    BrowserReplayError, BrowserReplayExecutionHandle, BrowserReplayFailureCode, BrowserReplayPlan,
    BrowserReplayProjection, BrowserReplayPublicInput, BrowserReplayStatus, BrowserWorkspaceKey,
    BROWSER_RECIPE_SCHEMA_VERSION, MAX_BROWSER_REPLAY_SECRET_INPUTS, MAX_BROWSER_REPLAY_TEXT_BYTES,
    MAX_BROWSER_REPLAY_URL_BYTES,
};
use static_assertions::{assert_impl_all, assert_not_impl_any};

assert_impl_all!(BrowserReplayPublicInput: Send, Sync);
assert_impl_all!(BrowserReplayPlan: Send, Sync);
assert_impl_all!(BrowserReplayCoordinator: Clone, Send, Sync);
assert_impl_all!(BrowserReplayProjection: Clone, Send, Sync, std::fmt::Debug, serde::Serialize);
assert_impl_all!(BrowserReplayCancellationLease: Clone, Send, Sync);
assert_impl_all!(BrowserReplayExecutionHandle: Send, Sync);
assert_not_impl_any!(BrowserReplayPublicInput: std::fmt::Debug, serde::Serialize);
assert_not_impl_any!(BrowserReplayPlan: std::fmt::Debug, serde::Serialize);
assert_not_impl_any!(BrowserReplayCancellationLease: std::fmt::Debug, serde::Serialize);
assert_not_impl_any!(BrowserReplayExecutionHandle: Clone, std::fmt::Debug, serde::Serialize);

fn locator(test_id: &str) -> BrowserRecipeLocator {
    BrowserRecipeLocator {
        test_id: Some(test_id.to_string()),
        ..BrowserRecipeLocator::default()
    }
}

fn replay_recipe() -> BrowserRecipeV1 {
    BrowserRecipeV1 {
        schema_version: BROWSER_RECIPE_SCHEMA_VERSION,
        id: "replay-checkout".to_string(),
        name: "Replay checkout".to_string(),
        description: "Checkpoint seven fixture".to_string(),
        start_url: "https://example.test/start".to_string(),
        viewport: BrowserRecipeViewport {
            width: 1440,
            height: 900,
            scale_percent: 100,
        },
        inputs: vec![
            BrowserRecipeInput {
                name: "query".to_string(),
                kind: BrowserRecipeInputKind::Text,
                default_value: None,
            },
            BrowserRecipeInput {
                name: "destination".to_string(),
                kind: BrowserRecipeInputKind::Url,
                default_value: Some("https://example.test/default".to_string()),
            },
            BrowserRecipeInput {
                name: "upload".to_string(),
                kind: BrowserRecipeInputKind::File,
                default_value: None,
            },
            BrowserRecipeInput {
                name: "password".to_string(),
                kind: BrowserRecipeInputKind::Secret,
                default_value: None,
            },
        ],
        steps: vec![
            BrowserRecipeStep {
                id: "type-query".to_string(),
                action: BrowserRecipeAction::Type {
                    locator: locator("query"),
                    value: BrowserRecipeValue::Input {
                        name: "query".to_string(),
                    },
                },
                wait: None,
                assertions: Vec::new(),
            },
            BrowserRecipeStep {
                id: "navigate".to_string(),
                action: BrowserRecipeAction::Navigate {
                    url: BrowserRecipeValue::Input {
                        name: "destination".to_string(),
                    },
                },
                wait: None,
                assertions: Vec::new(),
            },
            BrowserRecipeStep {
                id: "upload".to_string(),
                action: BrowserRecipeAction::Upload {
                    locator: locator("upload"),
                    file: BrowserRecipeValue::Input {
                        name: "upload".to_string(),
                    },
                },
                wait: None,
                assertions: Vec::new(),
            },
            BrowserRecipeStep {
                id: "type-secret".to_string(),
                action: BrowserRecipeAction::Type {
                    locator: locator("password"),
                    value: BrowserRecipeValue::Input {
                        name: "password".to_string(),
                    },
                },
                wait: None,
                assertions: Vec::new(),
            },
        ],
    }
}

fn replay_recipe_with_secret_count(secret_count: usize) -> BrowserRecipeV1 {
    BrowserRecipeV1 {
        schema_version: BROWSER_RECIPE_SCHEMA_VERSION,
        id: format!("secret-capacity-{secret_count}"),
        name: "Secret capacity fixture".to_string(),
        description: "Public compiled-recipe boundary fixture".to_string(),
        start_url: "https://example.test/start".to_string(),
        viewport: BrowserRecipeViewport::default(),
        inputs: (0..secret_count)
            .map(|index| BrowserRecipeInput {
                name: format!("secret_{index}"),
                kind: BrowserRecipeInputKind::Secret,
                default_value: None,
            })
            .collect(),
        steps: vec![BrowserRecipeStep {
            id: "type-secret".to_string(),
            action: BrowserRecipeAction::Type {
                locator: locator("password"),
                value: BrowserRecipeValue::Input {
                    name: "secret_0".to_string(),
                },
            },
            wait: None,
            assertions: Vec::new(),
        }],
    }
}

fn input(name: &str, kind: BrowserRecipeInputKind, value: &str) -> BrowserReplayPublicInput {
    BrowserReplayPublicInput::new(name, kind, value)
}

fn compile_fixture(file: &str) -> Result<BrowserReplayPlan, BrowserReplayError> {
    compile_browser_replay(
        &replay_recipe(),
        vec![
            input("upload", BrowserRecipeInputKind::File, file),
            input("query", BrowserRecipeInputKind::Text, "rust replay"),
        ],
    )
}

fn compile_error(result: Result<BrowserReplayPlan, BrowserReplayError>) -> BrowserReplayError {
    match result {
        Ok(_) => panic!("replay compilation unexpectedly succeeded"),
        Err(error) => error,
    }
}

fn replay_error<T>(result: Result<T, BrowserReplayError>) -> BrowserReplayError {
    match result {
        Ok(_) => panic!("replay operation unexpectedly succeeded"),
        Err(error) => error,
    }
}

fn workspace(project: &str, conversation: &str) -> BrowserWorkspaceKey {
    BrowserWorkspaceKey::new(project, conversation).unwrap()
}

fn plan_without_secrets() -> BrowserReplayPlan {
    let mut recipe = replay_recipe();
    recipe.name = "projection-recipe-name-sentinel".to_string();
    recipe.description = "projection-recipe-description-sentinel".to_string();
    recipe
        .inputs
        .retain(|input| input.kind != BrowserRecipeInputKind::Secret);
    recipe.steps.retain(|step| step.id != "type-secret");
    if let BrowserRecipeAction::Type { locator, value } = &mut recipe.steps[0].action {
        locator.test_id = Some("projection-locator-sentinel".to_string());
        *value = BrowserRecipeValue::Literal {
            value: "projection-recipe-literal-sentinel".to_string(),
        };
    }
    compile_browser_replay(
        &recipe,
        vec![
            input(
                "query",
                BrowserRecipeInputKind::Text,
                "state-query-value-sentinel",
            ),
            input(
                "upload",
                BrowserRecipeInputKind::File,
                "state-file-path-sentinel.txt",
            ),
        ],
    )
    .unwrap()
}

fn replay_recipe_without_secret_gate() -> BrowserRecipeV1 {
    let mut recipe = replay_recipe();
    recipe
        .inputs
        .retain(|input| input.kind != BrowserRecipeInputKind::Secret);
    recipe.steps[3].action = BrowserRecipeAction::Reload;
    recipe
}

fn tab_alias_recipe(actions: Vec<BrowserRecipeAction>) -> BrowserRecipeV1 {
    BrowserRecipeV1 {
        schema_version: BROWSER_RECIPE_SCHEMA_VERSION,
        id: "tab-alias-lifecycle".to_string(),
        name: "Tab alias lifecycle".to_string(),
        description: "Portable replay tab aliases".to_string(),
        start_url: "https://example.test/start".to_string(),
        viewport: BrowserRecipeViewport::default(),
        inputs: Vec::new(),
        steps: actions
            .into_iter()
            .enumerate()
            .map(|(index, action)| BrowserRecipeStep {
                id: format!("step-{}", index + 1),
                action,
                wait: None,
                assertions: Vec::new(),
            })
            .collect(),
    }
}

fn assert_credential_identifier_never_reaches_replay_history(
    recipe: BrowserRecipeV1,
    credential_id: &str,
    target_step_index: Option<usize>,
    case_index: usize,
) {
    let coordinator = BrowserReplayCoordinator::with_terminal_capacity(4);
    let result = compile_browser_replay(
        &recipe,
        vec![
            input("query", BrowserRecipeInputKind::Text, "safe-query"),
            input("upload", BrowserRecipeInputKind::File, "safe-file.txt"),
        ],
    );

    match result {
        Err(error) => {
            assert_eq!(error, BrowserReplayError::InvalidRecipe);
            assert!(!format!("{error:?}").contains(credential_id));
            assert!(!error.to_string().contains(credential_id));
        }
        Ok(plan) => {
            let conversation = format!("credential-id-case-{case_index}");
            let started = coordinator
                .start(workspace("credential-id-project", &conversation), plan)
                .unwrap();
            let mut projection = started.projection;
            if let Some(target_step_index) = target_step_index {
                coordinator.begin(&started.instance).unwrap();
                for completed_step_index in 0..target_step_index {
                    projection = coordinator
                        .advance_step(&started.instance, completed_step_index)
                        .unwrap();
                }
            }
            let cancelled = coordinator.cancel(&started.instance).unwrap();
            let history = coordinator.status(&started.instance).unwrap();
            for surface in [
                serde_json::to_string(&projection).unwrap(),
                format!("{projection:?}"),
                serde_json::to_string(&cancelled).unwrap(),
                format!("{cancelled:?}"),
                serde_json::to_string(&history).unwrap(),
                format!("{history:?}"),
            ] {
                assert!(
                    !surface.contains(credential_id),
                    "credential-shaped identifier reached replay projection/history"
                );
            }
            panic!("credential-shaped identifier unexpectedly compiled");
        }
    }

    assert_eq!(coordinator.retained_terminal_count(), 0);
}

#[test]
fn replay_compiler_applies_safe_defaults_and_preserves_ordered_opaque_bindings() {
    let plan = compile_fixture("fixtures/upload.txt").expect("compile replay plan");

    assert_eq!(plan.recipe_id(), "replay-checkout");
    assert_eq!(plan.start_url(), "https://example.test/start");
    assert_eq!(plan.viewport().width, 1440);
    assert_eq!(plan.resolve_input("query"), Some("rust replay"));
    assert_eq!(
        plan.resolve_input("destination"),
        Some("https://example.test/default")
    );
    assert_eq!(plan.resolve_input("upload"), Some("fixtures/upload.txt"));
    assert_eq!(plan.resolve_input("password"), None);
    assert_eq!(plan.unresolved_secret_input_names(), &["password"]);
    assert_eq!(
        plan.bound_input_names().collect::<Vec<_>>(),
        vec!["query", "destination", "upload"]
    );
    assert_eq!(
        plan.steps()
            .iter()
            .map(|step| step.id.as_str())
            .collect::<Vec<_>>(),
        vec!["type-query", "navigate", "upload", "type-secret"]
    );

    let absolute_candidate = if cfg!(windows) {
        r"C:\outside-workspace\upload.txt"
    } else {
        "/outside-workspace/upload.txt"
    };
    let absolute =
        compile_fixture(absolute_candidate).expect("absolute remains an opaque candidate");
    assert_eq!(absolute.resolve_input("upload"), Some(absolute_candidate));
}

#[test]
fn replay_compiler_rejects_invalid_recipe_and_exact_public_input_contract_violations() {
    let mut invalid = replay_recipe();
    invalid.steps.clear();
    assert_eq!(
        compile_error(compile_browser_replay(&invalid, Vec::new())),
        BrowserReplayError::InvalidRecipe
    );

    assert_eq!(
        compile_error(compile_browser_replay(
            &replay_recipe(),
            vec![
                input("query", BrowserRecipeInputKind::Text, "first"),
                input("query", BrowserRecipeInputKind::Text, "second"),
                input("upload", BrowserRecipeInputKind::File, "fixture.txt"),
            ],
        )),
        BrowserReplayError::DuplicatePublicInput
    );
    assert_eq!(
        compile_error(compile_browser_replay(
            &replay_recipe(),
            vec![
                input("query", BrowserRecipeInputKind::Text, "rust"),
                input("upload", BrowserRecipeInputKind::File, "fixture.txt"),
                input("extra", BrowserRecipeInputKind::Text, "unknown"),
            ],
        )),
        BrowserReplayError::UnknownPublicInput
    );
    assert_eq!(
        compile_error(compile_browser_replay(
            &replay_recipe(),
            vec![input("upload", BrowserRecipeInputKind::File, "fixture.txt")],
        )),
        BrowserReplayError::MissingPublicInput
    );
    assert_eq!(
        compile_error(compile_browser_replay(
            &replay_recipe(),
            vec![
                input("query", BrowserRecipeInputKind::Url, "https://example.test"),
                input("upload", BrowserRecipeInputKind::File, "fixture.txt"),
            ],
        )),
        BrowserReplayError::InputKindMismatch
    );
}

#[test]
fn replay_compile_rejects_invalid_tab_alias_lifecycle() {
    let invalid_actions = [
        vec![BrowserRecipeAction::SelectTab {
            tab: "ambient-tab".to_string(),
        }],
        vec![BrowserRecipeAction::CloseTab {
            tab: "ambient-tab".to_string(),
        }],
        vec![
            BrowserRecipeAction::CreateTab {
                tab: "created-tab".to_string(),
                url: None,
            },
            BrowserRecipeAction::CreateTab {
                tab: "created-tab".to_string(),
                url: None,
            },
        ],
        vec![
            BrowserRecipeAction::CloseTab {
                tab: "tab-1".to_string(),
            },
            BrowserRecipeAction::SelectTab {
                tab: "tab-1".to_string(),
            },
        ],
        vec![
            BrowserRecipeAction::CreateTab {
                tab: "tab-1".to_string(),
                url: None,
            },
            BrowserRecipeAction::CloseTab {
                tab: "tab-1".to_string(),
            },
            BrowserRecipeAction::CreateTab {
                tab: "tab-1".to_string(),
                url: None,
            },
        ],
    ];

    for actions in invalid_actions {
        assert_eq!(
            compile_error(compile_browser_replay(
                &tab_alias_recipe(actions),
                Vec::new()
            )),
            BrowserReplayError::InvalidRecipe
        );
    }

    let portable_initial_tab = tab_alias_recipe(vec![
        BrowserRecipeAction::SelectTab {
            tab: "tab-1".to_string(),
        },
        BrowserRecipeAction::CreateTab {
            tab: "tab-2".to_string(),
            url: None,
        },
        BrowserRecipeAction::SelectTab {
            tab: "tab-1".to_string(),
        },
    ]);
    assert!(compile_browser_replay(&portable_initial_tab, Vec::new()).is_ok());

    let legacy_create_tab_one = tab_alias_recipe(vec![
        BrowserRecipeAction::CreateTab {
            tab: "tab-1".to_string(),
            url: None,
        },
        BrowserRecipeAction::SelectTab {
            tab: "tab-1".to_string(),
        },
    ]);
    assert!(compile_browser_replay(&legacy_create_tab_one, Vec::new()).is_ok());
}

#[test]
fn replay_compiler_rejects_every_public_secret_submission_without_echoing_values() {
    let secret = "authorization=Bearer replay-public-secret-sentinel";
    for supplied in [
        input("password", BrowserRecipeInputKind::Secret, secret),
        input("password", BrowserRecipeInputKind::Text, secret),
        input("unknown", BrowserRecipeInputKind::Secret, secret),
    ] {
        let error = compile_error(compile_browser_replay(
            &replay_recipe(),
            vec![
                input("query", BrowserRecipeInputKind::Text, "rust"),
                input("upload", BrowserRecipeInputKind::File, "fixture.txt"),
                supplied,
            ],
        ));
        assert_eq!(error, BrowserReplayError::PublicSecretRejected);
        assert!(!format!("{error:?}").contains(secret));
        assert!(!error.to_string().contains(secret));
    }
}

#[test]
fn replay_compiler_rejects_secret_input_count_above_store_capacity_before_start() {
    let accepted = compile_browser_replay(
        &replay_recipe_with_secret_count(MAX_BROWSER_REPLAY_SECRET_INPUTS),
        Vec::new(),
    )
    .unwrap();
    assert_eq!(
        accepted.unresolved_secret_input_names().len(),
        MAX_BROWSER_REPLAY_SECRET_INPUTS
    );
    let coordinator = BrowserReplayCoordinator::default();
    let started = coordinator
        .start(workspace("secret-capacity", "accepted"), accepted)
        .unwrap();
    assert_eq!(
        started.projection.status,
        BrowserReplayStatus::NeedsUserSecret
    );

    assert_eq!(
        compile_error(compile_browser_replay(
            &replay_recipe_with_secret_count(MAX_BROWSER_REPLAY_SECRET_INPUTS + 1),
            Vec::new(),
        )),
        BrowserReplayError::CapacityExceeded
    );
}

#[test]
fn replay_compiler_validates_bounded_text_url_and_opaque_file_candidates() {
    let cases = [
        (
            input(
                "query",
                BrowserRecipeInputKind::Text,
                "authorization=Bearer text-value-sentinel",
            ),
            BrowserReplayError::InvalidTextInput,
        ),
        (
            input("query", BrowserRecipeInputKind::Text, "nul\0text"),
            BrowserReplayError::InvalidTextInput,
        ),
        (
            input("query", BrowserRecipeInputKind::Text, &"x".repeat(65_537)),
            BrowserReplayError::InvalidTextInput,
        ),
        (
            input(
                "destination",
                BrowserRecipeInputKind::Url,
                "javascript:alert(1)",
            ),
            BrowserReplayError::InvalidUrlInput,
        ),
        (
            input(
                "destination",
                BrowserRecipeInputKind::Url,
                "https://example.test/?token=url-value-sentinel",
            ),
            BrowserReplayError::InvalidUrlInput,
        ),
        (
            input(
                "destination",
                BrowserRecipeInputKind::Url,
                &format!("https://example.test/{}", "u".repeat(8_193)),
            ),
            BrowserReplayError::InvalidUrlInput,
        ),
        (
            input("upload", BrowserRecipeInputKind::File, "   "),
            BrowserReplayError::InvalidFileInput,
        ),
        (
            input("upload", BrowserRecipeInputKind::File, "folder\nfile.txt"),
            BrowserReplayError::InvalidFileInput,
        ),
        (
            input("upload", BrowserRecipeInputKind::File, &"f".repeat(32_769)),
            BrowserReplayError::InvalidFileInput,
        ),
    ];

    for (supplied, expected) in cases {
        let supplied_name = supplied.name().to_string();
        let mut inputs = vec![
            input("query", BrowserRecipeInputKind::Text, "rust"),
            input("upload", BrowserRecipeInputKind::File, "fixture.txt"),
        ];
        inputs.retain(|candidate| candidate.name() != supplied_name);
        inputs.push(supplied);
        assert_eq!(
            compile_error(compile_browser_replay(&replay_recipe(), inputs)),
            expected
        );
    }
}

#[test]
fn replay_compiler_accepts_safe_values_through_the_documented_bounds() {
    let safe_text = "x".repeat(MAX_BROWSER_REPLAY_TEXT_BYTES);
    let url_prefix = "https://example.test/";
    let safe_url = format!(
        "{url_prefix}{}",
        "u".repeat(MAX_BROWSER_REPLAY_URL_BYTES - url_prefix.len())
    );
    let plan = compile_browser_replay(
        &replay_recipe(),
        vec![
            input("query", BrowserRecipeInputKind::Text, &safe_text),
            input("destination", BrowserRecipeInputKind::Url, &safe_url),
            input("upload", BrowserRecipeInputKind::File, "fixture.txt"),
        ],
    )
    .expect("safe values at the declared replay bounds compile");

    assert_eq!(plan.resolve_input("query"), Some(safe_text.as_str()));
    assert_eq!(plan.resolve_input("destination"), Some(safe_url.as_str()));
}

#[test]
fn replay_compiler_rejects_credential_bearing_secret_names_before_projection() {
    for unsafe_name in [
        "password=secret-name-value-sentinel",
        "sk-proj-abcdefghijklmnopqrstuvwxyz0123456789ABCDEFG",
    ] {
        let mut recipe = replay_recipe();
        recipe.inputs.push(BrowserRecipeInput {
            name: unsafe_name.to_string(),
            kind: BrowserRecipeInputKind::Secret,
            default_value: None,
        });

        assert_eq!(
            compile_error(compile_browser_replay(
                &recipe,
                vec![
                    input("query", BrowserRecipeInputKind::Text, "rust"),
                    input("upload", BrowserRecipeInputKind::File, "fixture.txt"),
                ],
            )),
            BrowserReplayError::InvalidRecipe
        );
    }

    let mut ordinary_names = replay_recipe();
    for name in ["api_token", "login_secret"] {
        ordinary_names.inputs.push(BrowserRecipeInput {
            name: name.to_string(),
            kind: BrowserRecipeInputKind::Secret,
            default_value: None,
        });
    }
    let plan = compile_browser_replay(
        &ordinary_names,
        vec![
            input("query", BrowserRecipeInputKind::Text, "rust"),
            input("upload", BrowserRecipeInputKind::File, "fixture.txt"),
        ],
    )
    .unwrap();
    assert!(plan
        .unresolved_secret_input_names()
        .iter()
        .any(|name| name == "api_token"));
    assert!(plan
        .unresolved_secret_input_names()
        .iter()
        .any(|name| name == "login_secret"));
}

#[test]
fn replay_compiler_rejects_credential_shaped_recipe_and_every_step_id_before_history() {
    let mut case_index = 0;
    for credential_id in [
        "sk-proj-abcdefghijklmnopqrstuvwxyz0123456789ABCDEFG",
        "ghp_abcdefghijklmnopqrstuvwxyz0123456789",
    ] {
        let mut recipe = replay_recipe_without_secret_gate();
        recipe.id = credential_id.to_string();
        assert_credential_identifier_never_reaches_replay_history(
            recipe,
            credential_id,
            None,
            case_index,
        );
        case_index += 1;

        for step_index in 0..replay_recipe_without_secret_gate().steps.len() {
            let mut recipe = replay_recipe_without_secret_gate();
            recipe.steps[step_index].id = credential_id.to_string();
            assert_credential_identifier_never_reaches_replay_history(
                recipe,
                credential_id,
                Some(step_index),
                case_index,
            );
            case_index += 1;
        }
    }
}

#[test]
fn replay_errors_have_only_fixed_value_free_messages() {
    let errors = [
        BrowserReplayError::InvalidRecipe,
        BrowserReplayError::CapacityExceeded,
        BrowserReplayError::InvalidPublicInputName,
        BrowserReplayError::DuplicatePublicInput,
        BrowserReplayError::UnknownPublicInput,
        BrowserReplayError::MissingPublicInput,
        BrowserReplayError::PublicSecretRejected,
        BrowserReplayError::InputKindMismatch,
        BrowserReplayError::InvalidTextInput,
        BrowserReplayError::InvalidUrlInput,
        BrowserReplayError::InvalidFileInput,
        BrowserReplayError::AlreadyActive,
        BrowserReplayError::StaleInstance,
        BrowserReplayError::InvalidTransition,
        BrowserReplayError::StepOutOfOrder,
        BrowserReplayError::IncompleteReplay,
        BrowserReplayError::TerminalState,
        BrowserReplayError::InstanceIdExhausted,
        BrowserReplayError::RepairInstanceIdExhausted,
        BrowserReplayError::InvalidRepairSlot,
        BrowserReplayError::InvalidRepairEvidence,
        BrowserReplayError::RepairEvidenceUnavailable,
    ];
    for error in errors {
        let display = error.to_string();
        assert!(display.starts_with("browser replay "));
        assert!(!display.contains("value-sentinel"));
        assert!(!format!("{error:?}").contains("value-sentinel"));
    }
}

#[test]
fn replay_state_enforces_exact_progress_completion_and_terminal_immutability() {
    let coordinator = BrowserReplayCoordinator::with_terminal_capacity(4);
    let owner = workspace("project-a", "conversation-a");
    let started = coordinator.start(owner, plan_without_secrets()).unwrap();
    let instance = started.instance.clone();

    assert_eq!(started.projection.status, BrowserReplayStatus::Pending);
    assert_eq!(started.projection.current_step_index, 0);
    assert_eq!(
        started.projection.current_step_id.as_deref(),
        Some("type-query")
    );
    assert_eq!(
        replay_error(coordinator.complete(&instance)),
        BrowserReplayError::InvalidTransition
    );
    let running = coordinator.begin(&instance).unwrap();
    assert_eq!(running.status, BrowserReplayStatus::Running);
    assert_eq!(
        replay_error(coordinator.complete(&instance)),
        BrowserReplayError::IncompleteReplay
    );

    let after_first = coordinator.advance_step(&instance, 0).unwrap();
    assert_eq!(after_first.current_step_index, 1);
    assert_eq!(after_first.current_step_id.as_deref(), Some("navigate"));
    assert_eq!(
        replay_error(coordinator.advance_step(&instance, 0)),
        BrowserReplayError::StepOutOfOrder
    );
    coordinator.advance_step(&instance, 1).unwrap();
    coordinator.advance_step(&instance, 2).unwrap();
    let completed = coordinator.complete(&instance).unwrap();
    assert_eq!(completed.status, BrowserReplayStatus::Completed);
    assert_eq!(completed.current_step_index, 3);
    assert_eq!(completed.current_step_id, None);

    for error in [
        replay_error(coordinator.begin(&instance)),
        replay_error(coordinator.cancel(&instance)),
        replay_error(coordinator.fail(&instance, BrowserReplayFailureCode::StepFailed)),
    ] {
        assert_eq!(error, BrowserReplayError::TerminalState);
    }
    assert_eq!(
        coordinator.status(&instance).unwrap().status,
        BrowserReplayStatus::Completed
    );
}

#[test]
fn replay_state_projects_all_exact_statuses_and_typed_failures_without_values() {
    let coordinator = BrowserReplayCoordinator::with_terminal_capacity(8);

    let needs = coordinator
        .start(
            workspace("project-needs", "conversation-a"),
            compile_fixture("needs-secret-file-path-sentinel.txt").unwrap(),
        )
        .unwrap();
    assert_eq!(
        needs.projection.status,
        BrowserReplayStatus::NeedsUserSecret
    );
    assert_eq!(needs.projection.unresolved_secret_inputs, vec!["password"]);
    let cancelled = coordinator.cancel(&needs.instance).unwrap();
    assert_eq!(cancelled.status, BrowserReplayStatus::Cancelled);

    let failed = coordinator
        .start(
            workspace("project-failed", "conversation-a"),
            plan_without_secrets(),
        )
        .unwrap();
    coordinator.begin(&failed.instance).unwrap();
    let failed = coordinator
        .fail(&failed.instance, BrowserReplayFailureCode::AssertionFailed)
        .unwrap();
    assert_eq!(failed.status, BrowserReplayStatus::Failed);
    assert_eq!(
        failed.failure,
        Some(BrowserReplayFailureCode::AssertionFailed)
    );

    let encoded = serde_json::to_string(&failed).unwrap();
    let debug = format!("{failed:?}");
    for forbidden in [
        "state-query-value-sentinel",
        "state-file-path-sentinel",
        "needs-secret-file-path-sentinel",
        "https://example.test/start",
        "https://example.test/default",
        "projection-recipe-name-sentinel",
        "projection-recipe-description-sentinel",
        "projection-locator-sentinel",
        "projection-recipe-literal-sentinel",
    ] {
        assert!(
            !encoded.contains(forbidden),
            "projection serialized {forbidden}"
        );
        assert!(
            !debug.contains(forbidden),
            "projection debugged {forbidden}"
        );
    }
    assert_eq!(
        serde_json::to_value(BrowserReplayStatus::Pending).unwrap(),
        "pending"
    );
    assert_eq!(
        serde_json::to_value(BrowserReplayStatus::NeedsUserSecret).unwrap(),
        "needsUserSecret"
    );
    assert_eq!(
        serde_json::to_value(BrowserReplayStatus::PausedLocatorRepair).unwrap(),
        "pausedLocatorRepair"
    );
    assert_eq!(
        [
            BrowserReplayStatus::Pending,
            BrowserReplayStatus::Running,
            BrowserReplayStatus::NeedsUserSecret,
            BrowserReplayStatus::PausedLocatorRepair,
            BrowserReplayStatus::Completed,
            BrowserReplayStatus::Failed,
            BrowserReplayStatus::Cancelled,
        ]
        .map(|status| serde_json::to_value(status).unwrap()),
        [
            "pending",
            "running",
            "needsUserSecret",
            "pausedLocatorRepair",
            "completed",
            "failed",
            "cancelled",
        ]
    );
}

#[test]
fn replay_state_fences_one_active_instance_and_explicit_replacement_per_workspace() {
    let coordinator = BrowserReplayCoordinator::with_terminal_capacity(8);
    let owner = workspace("project-a", "conversation-a");
    let other = workspace("project-a", "conversation-b");
    let old = coordinator
        .start(owner.clone(), plan_without_secrets())
        .unwrap();
    assert_eq!(
        replay_error(coordinator.start(owner.clone(), plan_without_secrets())),
        BrowserReplayError::AlreadyActive
    );
    let isolated = coordinator
        .start(other.clone(), plan_without_secrets())
        .unwrap();

    let replacement = coordinator
        .replace(owner.clone(), plan_without_secrets())
        .unwrap();
    assert_ne!(old.instance.id(), replacement.instance.id());
    assert_eq!(
        coordinator.status(&old.instance).unwrap().status,
        BrowserReplayStatus::Cancelled
    );
    assert_eq!(
        replay_error(coordinator.advance_step(&old.instance, 0)),
        BrowserReplayError::TerminalState
    );
    assert_eq!(replacement.projection.status, BrowserReplayStatus::Pending);

    coordinator.interrupt_workspace(&owner);
    assert_eq!(
        coordinator.status(&replacement.instance).unwrap().status,
        BrowserReplayStatus::Cancelled
    );
    assert_eq!(
        coordinator.status(&isolated.instance).unwrap().status,
        BrowserReplayStatus::Pending
    );

    let unrelated = BrowserReplayCoordinator::default();
    assert_eq!(
        replay_error(unrelated.status(&isolated.instance)),
        BrowserReplayError::StaleInstance
    );
    assert!(coordinator.interrupt_workspace(&other).is_some());
}

#[test]
fn replay_state_rejects_colliding_local_ids_from_an_unrelated_coordinator() {
    let left = BrowserReplayCoordinator::with_terminal_capacity(2);
    let right = BrowserReplayCoordinator::with_terminal_capacity(2);
    let owner = workspace("same-project", "same-conversation");
    let left_started = left.start(owner.clone(), plan_without_secrets()).unwrap();
    let right_started = right.start(owner, plan_without_secrets()).unwrap();

    assert_eq!(left_started.instance.id(), right_started.instance.id());
    assert_eq!(
        replay_error(left.status(&right_started.instance)),
        BrowserReplayError::StaleInstance
    );
    assert_eq!(
        replay_error(left.cancel(&right_started.instance)),
        BrowserReplayError::StaleInstance
    );
    assert_eq!(
        left.status(&left_started.instance).unwrap().status,
        BrowserReplayStatus::Pending
    );
}

#[test]
fn replay_state_terminal_cleanup_is_bounded_and_evicts_oldest_identity() {
    let coordinator = BrowserReplayCoordinator::with_terminal_capacity(2);
    let owner = workspace("project-a", "conversation-a");
    let mut instances = Vec::new();
    for _ in 0..3 {
        let started = coordinator
            .start(owner.clone(), plan_without_secrets())
            .unwrap();
        instances.push(started.instance.clone());
        coordinator.cancel(&started.instance).unwrap();
    }

    assert_eq!(
        replay_error(coordinator.status(&instances[0])),
        BrowserReplayError::StaleInstance
    );
    for instance in &instances[1..] {
        assert_eq!(
            coordinator.status(instance).unwrap().status,
            BrowserReplayStatus::Cancelled
        );
    }
    assert_eq!(coordinator.retained_terminal_count(), 2);
}

#[test]
fn replay_cancellation_uses_one_authority_across_running_progress() {
    let coordinator = BrowserReplayCoordinator::with_terminal_capacity(8);
    let owner = workspace("project-running", "conversation-a");
    let started = coordinator
        .start(owner.clone(), plan_without_secrets())
        .unwrap();
    let lease_clone = started.lease.clone();
    let authority_id = started.lease.authority_id();

    assert!(started.lease.same_authority(&lease_clone));
    assert!(!started.lease.is_cancelled());
    coordinator.status(&started.instance).unwrap();
    coordinator.begin(&started.instance).unwrap();
    coordinator.advance_step(&started.instance, 0).unwrap();
    coordinator.status(&started.instance).unwrap();
    assert_eq!(started.lease.authority_id(), authority_id);
    assert!(started.lease.same_authority(&lease_clone));
    assert!(!lease_clone.is_cancelled());

    let cancelled = coordinator.interrupt_workspace(&owner).unwrap();
    assert_eq!(cancelled.status, BrowserReplayStatus::Cancelled);
    assert!(started.lease.is_cancelled());
    assert!(lease_clone.is_cancelled());
}

#[test]
fn replay_execution_handle_shares_plan_and_cancellation_authority() {
    let coordinator = BrowserReplayCoordinator::with_terminal_capacity(2);
    let started = coordinator
        .start(
            workspace("execution-handle", "first"),
            plan_without_secrets(),
        )
        .expect("start replay");
    let lease_clone = started.lease.clone();

    assert!(started.lease.same_authority(&lease_clone));
    assert!(started.execution.same_instance(&started.instance));
    assert!(started.execution.same_authority(&started.lease));

    coordinator
        .cancel(&started.instance)
        .expect("cancel exact replay");
    assert!(started.lease.is_cancelled());
    assert!(lease_clone.is_cancelled());
    assert!(started.execution.is_cancelled());

    let replacement = coordinator
        .start(
            workspace("execution-handle", "replacement"),
            plan_without_secrets(),
        )
        .expect("start unrelated replay");
    assert!(!started.lease.same_authority(&replacement.lease));
    assert!(!started.execution.same_instance(&replacement.instance));
    assert!(!replacement.execution.same_instance(&started.instance));
}

#[test]
fn replay_cancellation_invalidates_pending_and_needs_secret_instances() {
    let coordinator = BrowserReplayCoordinator::with_terminal_capacity(8);
    let pending_owner = workspace("project-pending", "conversation-a");
    let pending = coordinator
        .start(pending_owner.clone(), plan_without_secrets())
        .unwrap();
    assert!(!pending.lease.is_cancelled());
    coordinator.interrupt_workspace(&pending_owner).unwrap();
    assert!(pending.lease.is_cancelled());

    let needs = coordinator
        .start(
            workspace("project-secret", "conversation-a"),
            compile_fixture("secret-gap-file-sentinel.txt").unwrap(),
        )
        .unwrap();
    assert_eq!(
        needs.projection.status,
        BrowserReplayStatus::NeedsUserSecret
    );
    let needs_clone = needs.lease.clone();
    coordinator.cancel(&needs.instance).unwrap();
    assert!(needs.lease.is_cancelled());
    assert!(needs_clone.is_cancelled());
}

#[test]
fn replay_cancellation_replacement_invalidates_only_the_old_authority() {
    let coordinator = BrowserReplayCoordinator::with_terminal_capacity(8);
    let owner = workspace("project-replaced", "conversation-a");
    let old = coordinator
        .start(owner.clone(), plan_without_secrets())
        .unwrap();
    let old_clone = old.lease.clone();
    let replacement = coordinator
        .replace(owner.clone(), plan_without_secrets())
        .unwrap();

    assert!(old.lease.is_cancelled());
    assert!(old_clone.is_cancelled());
    assert!(!replacement.lease.is_cancelled());
    assert!(!old.lease.same_authority(&replacement.lease));
    assert_ne!(old.lease.authority_id(), replacement.lease.authority_id());
    assert_eq!(
        replay_error(coordinator.complete(&old.instance)),
        BrowserReplayError::TerminalState
    );

    coordinator.cancel(&replacement.instance).unwrap();
    assert!(replacement.lease.is_cancelled());
}

#[test]
fn replay_cancellation_does_not_relabel_completed_or_failed_replays_as_cancelled() {
    let coordinator = BrowserReplayCoordinator::with_terminal_capacity(8);
    let completed = coordinator
        .start(
            workspace("project-completed", "conversation-a"),
            plan_without_secrets(),
        )
        .unwrap();
    coordinator.begin(&completed.instance).unwrap();
    for step_index in 0..3 {
        coordinator
            .advance_step(&completed.instance, step_index)
            .unwrap();
    }
    coordinator.complete(&completed.instance).unwrap();
    assert!(!completed.lease.is_cancelled());

    let failed = coordinator
        .start(
            workspace("project-failed-lease", "conversation-a"),
            plan_without_secrets(),
        )
        .unwrap();
    coordinator.begin(&failed.instance).unwrap();
    coordinator
        .fail(&failed.instance, BrowserReplayFailureCode::StepFailed)
        .unwrap();
    assert!(!failed.lease.is_cancelled());
}

#[test]
fn replay_scope_has_no_execution_or_platform_coupling() {
    let source = include_str!("../src/browser/replay.rs");
    let source = source
        .rsplit_once("mod tests {")
        .map(|(production, _)| production)
        .expect("replay source keeps one explicit test-module boundary");
    for forbidden in [
        "std::fs",
        "std::path",
        "BrowserHost",
        "BrowserController",
        "BrowserOperationQueue",
        "BrowserApproval",
        "BrowserJournal",
        "BrowserCommand",
        "recording_mcp",
        "BrowserPane",
        "zeroize",
    ] {
        assert!(
            !source.contains(forbidden),
            "replay domain unexpectedly couples to {forbidden}"
        );
    }
}
