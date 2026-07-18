use devmanager::browser::{
    browser_replay_secret_mask, compile_browser_replay, BrowserPaneContext, BrowserPaneModel,
    BrowserPaneSurface, BrowserPaneTransient, BrowserRecipeAction, BrowserRecipeInput,
    BrowserRecipeInputKind, BrowserRecipeLocator, BrowserRecipeStep, BrowserRecipeV1,
    BrowserRecipeValue, BrowserRecipeViewport, BrowserReplayCoordinator, BrowserReplaySecretError,
    BrowserReplaySecretPromptEvent, BrowserReplaySecretPromptOperation,
    BrowserReplaySecretPromptProjection, BrowserReplaySecretPromptVault, BrowserReplayStatus,
    BrowserWorkspaceKey, BrowserWorkspaceSnapshot, BROWSER_RECIPE_SCHEMA_VERSION,
    MAX_BROWSER_REPLAY_SECRET_INPUTS, MAX_BROWSER_REPLAY_SECRET_VALUE_BYTES,
};
use static_assertions::{assert_impl_all, assert_not_impl_any};

assert_impl_all!(
    BrowserReplaySecretPromptEvent:
        Clone,
        std::fmt::Debug,
        serde::Serialize,
        Send,
        Sync
);
assert_impl_all!(
    BrowserReplaySecretPromptProjection:
        Clone,
        std::fmt::Debug,
        serde::Serialize,
        Send,
        Sync
);
assert_impl_all!(BrowserReplaySecretPromptOperation: Copy, Clone, std::fmt::Debug, Send, Sync);
assert_not_impl_any!(
    BrowserReplaySecretPromptVault:
        Clone,
        std::fmt::Debug,
        serde::Serialize,
        serde::de::DeserializeOwned
);

const SECRET_SENTINEL: &str = "DM_PROMPT_SECRET_SENTINEL_5B1D";

fn workspace(conversation: &str) -> BrowserWorkspaceKey {
    BrowserWorkspaceKey::new("replay-secret-prompt", conversation).unwrap()
}

fn locator(test_id: &str) -> BrowserRecipeLocator {
    BrowserRecipeLocator {
        test_id: Some(test_id.to_string()),
        ..BrowserRecipeLocator::default()
    }
}

fn secret_recipe() -> BrowserRecipeV1 {
    let names = ["credential", "one-time-code"];
    BrowserRecipeV1 {
        schema_version: BROWSER_RECIPE_SCHEMA_VERSION,
        id: "secret-prompt".to_string(),
        name: "Secret prompt".to_string(),
        description: "Multiple replay secrets".to_string(),
        start_url: "https://example.test/sign-in".to_string(),
        viewport: BrowserRecipeViewport::default(),
        inputs: names
            .iter()
            .map(|name| BrowserRecipeInput {
                name: (*name).to_string(),
                kind: BrowserRecipeInputKind::Secret,
                default_value: None,
            })
            .collect(),
        steps: names
            .iter()
            .enumerate()
            .map(|(index, name)| BrowserRecipeStep {
                id: format!("secret-step-{}", index + 1),
                action: BrowserRecipeAction::Type {
                    locator: locator(name),
                    value: BrowserRecipeValue::Input {
                        name: (*name).to_string(),
                    },
                },
                wait: None,
                assertions: Vec::new(),
            })
            .collect(),
    }
}

#[test]
fn secret_prompt_supports_multiple_inputs_with_only_fixed_masked_safe_projections() {
    let coordinator = BrowserReplayCoordinator::default();
    let started = coordinator
        .start(
            workspace("multiple"),
            compile_browser_replay(&secret_recipe(), Vec::new()).unwrap(),
        )
        .unwrap();
    assert_eq!(
        started.projection.status,
        BrowserReplayStatus::NeedsUserSecret
    );
    let instance = started.instance.clone();
    let (mut vault, installed) = BrowserReplaySecretPromptVault::install(
        instance.clone(),
        started.projection.unresolved_secret_inputs.clone(),
    )
    .expect("install prompt for exact replay");

    assert_eq!(
        installed.operation,
        BrowserReplaySecretPromptOperation::Installed
    );
    assert_eq!(installed.input_name, None);
    assert_eq!(installed.focused_input.as_deref(), Some("credential"));
    assert_eq!(installed.is_set, None);
    let initial = vault.projection();
    assert_eq!(initial.workspace_key, workspace("multiple"));
    assert_eq!(initial.instance_id, instance.id());
    assert_eq!(initial.input_names, vec!["credential", "one-time-code"]);
    assert_eq!(initial.focused_input.as_deref(), Some("credential"));
    assert_eq!(initial.is_set, vec![false, false]);
    assert_eq!(browser_replay_secret_mask(false), "");
    assert_eq!(browser_replay_secret_mask(true), "••••••••");
    assert_eq!(browser_replay_secret_mask(true).chars().count(), 8);

    let first_edit = vault
        .edit(&instance, "credential", SECRET_SENTINEL)
        .expect("edit first secret");
    assert_eq!(
        first_edit.operation,
        BrowserReplaySecretPromptOperation::Edited
    );
    assert_eq!(first_edit.input_name.as_deref(), Some("credential"));
    assert_eq!(first_edit.focused_input.as_deref(), Some("credential"));
    assert_eq!(first_edit.is_set, Some(true));

    let focus = vault
        .focus(&instance, "one-time-code")
        .expect("focus second secret");
    assert_eq!(focus.operation, BrowserReplaySecretPromptOperation::Focused);
    vault.edit(&instance, "one-time-code", "93").unwrap();
    let backspace = vault
        .backspace(&instance, "one-time-code")
        .expect("backspace second secret");
    assert_eq!(
        backspace.operation,
        BrowserReplaySecretPromptOperation::Backspaced
    );
    assert_eq!(backspace.is_set, Some(true));
    let backspace = vault.backspace(&instance, "one-time-code").unwrap();
    assert_eq!(backspace.is_set, Some(false));
    vault.edit(&instance, "one-time-code", "4").unwrap();

    let projection = vault.projection();
    assert_eq!(projection.focused_input.as_deref(), Some("one-time-code"));
    assert_eq!(projection.is_set, vec![true, true]);
    assert_eq!(projection.mask_for("credential"), Some("••••••••"));
    assert_eq!(projection.mask_for("one-time-code"), Some("••••••••"));
    let safe_event_json = serde_json::to_string(&first_edit).unwrap();
    let safe_projection_json = serde_json::to_string(&projection).unwrap();
    let safe_projection_debug = format!("{projection:?}");
    for safe in [
        safe_event_json.as_str(),
        safe_projection_json.as_str(),
        safe_projection_debug.as_str(),
    ] {
        assert!(!safe.contains(SECRET_SENTINEL));
        assert!(!safe.contains("93"));
    }

    let model = BrowserPaneModel::new(
        workspace("multiple"),
        &BrowserPaneContext {
            browser_enabled: true,
            platform_supported: true,
            active_surface: Some(BrowserPaneSurface::Codex),
            editor_open: false,
            modal_open: false,
        },
        &BrowserWorkspaceSnapshot::default(),
        BrowserPaneTransient {
            replay_secret_prompt: Some(projection),
            ..BrowserPaneTransient::default()
        },
    );
    let pane_debug = format!("{model:?}");
    let persisted_snapshot = serde_json::to_string(&BrowserWorkspaceSnapshot::default()).unwrap();
    let remote_snapshot =
        serde_json::to_string(&devmanager::remote::RemoteWorkspaceSnapshot::default()).unwrap();
    assert!(!pane_debug.contains(SECRET_SENTINEL));
    assert!(!persisted_snapshot.contains(SECRET_SENTINEL));
    assert!(!remote_snapshot.contains(SECRET_SENTINEL));

    let (submission, submitted) = vault.submit(&instance).expect("consume prompt vault");
    assert_eq!(
        submitted.operation,
        BrowserReplaySecretPromptOperation::Submitted
    );
    assert!(!format!("{submitted:?}").contains(SECRET_SENTINEL));
    let running = coordinator
        .submit_secrets(&instance, submission)
        .expect("submit multiple exact secrets");
    assert_eq!(running.status, BrowserReplayStatus::Running);
    assert!(running.unresolved_secret_inputs.is_empty());
}

#[test]
fn secret_prompt_is_bounded_exact_instance_owned_and_closes_on_every_dismissal() {
    let left = BrowserReplayCoordinator::default();
    let right = BrowserReplayCoordinator::default();
    let left_started = left
        .start(
            workspace("exact"),
            compile_browser_replay(&secret_recipe(), Vec::new()).unwrap(),
        )
        .unwrap();
    let right_started = right
        .start(
            workspace("exact"),
            compile_browser_replay(&secret_recipe(), Vec::new()).unwrap(),
        )
        .unwrap();
    assert_eq!(left_started.instance.id(), right_started.instance.id());
    let (mut vault, _) = BrowserReplaySecretPromptVault::install(
        left_started.instance.clone(),
        left_started.projection.unresolved_secret_inputs.clone(),
    )
    .unwrap();
    assert!(matches!(
        vault.edit(&right_started.instance, "credential", SECRET_SENTINEL),
        Err(BrowserReplaySecretError::StaleAuthority)
    ));
    assert_eq!(vault.projection().is_set, vec![false, false]);
    vault
        .edit(&left_started.instance, "credential", SECRET_SENTINEL)
        .unwrap();
    let cancelled = vault.cancel(&left_started.instance).unwrap();
    assert_eq!(
        cancelled.operation,
        BrowserReplaySecretPromptOperation::Cancelled
    );
    assert!(!serde_json::to_string(&cancelled)
        .unwrap()
        .contains(SECRET_SENTINEL));

    let (mut route_vault, _) = BrowserReplaySecretPromptVault::install(
        left_started.instance.clone(),
        left_started.projection.unresolved_secret_inputs.clone(),
    )
    .unwrap();
    route_vault
        .edit(&left_started.instance, "credential", SECRET_SENTINEL)
        .unwrap();
    let route = route_vault.route_switch(&left_started.instance).unwrap();
    assert_eq!(
        route.operation,
        BrowserReplaySecretPromptOperation::RouteSwitched
    );

    let (mut replacement_vault, _) = BrowserReplaySecretPromptVault::install(
        left_started.instance.clone(),
        left_started.projection.unresolved_secret_inputs.clone(),
    )
    .unwrap();
    replacement_vault
        .edit(&left_started.instance, "credential", SECRET_SENTINEL)
        .unwrap();
    let replaced = replacement_vault
        .replay_replaced(&left_started.instance)
        .unwrap();
    assert_eq!(
        replaced.operation,
        BrowserReplaySecretPromptOperation::ReplayReplaced
    );

    let too_many = (0..=MAX_BROWSER_REPLAY_SECRET_INPUTS)
        .map(|index| format!("input-{index}"))
        .collect();
    assert!(matches!(
        BrowserReplaySecretPromptVault::install(left_started.instance.clone(), too_many),
        Err(BrowserReplaySecretError::InvalidSubmission)
    ));
    assert!(matches!(
        BrowserReplaySecretPromptVault::install(
            left_started.instance.clone(),
            vec!["duplicate".to_string(), "duplicate".to_string()],
        ),
        Err(BrowserReplaySecretError::InvalidSubmission)
    ));
    let (mut bounded, _) = BrowserReplaySecretPromptVault::install(
        left_started.instance.clone(),
        vec!["bounded".to_string()],
    )
    .unwrap();
    bounded
        .edit(
            &left_started.instance,
            "bounded",
            &"x".repeat(MAX_BROWSER_REPLAY_SECRET_VALUE_BYTES),
        )
        .unwrap();
    assert!(matches!(
        bounded.edit(&left_started.instance, "bounded", "x"),
        Err(BrowserReplaySecretError::InvalidSubmission)
    ));
    assert_eq!(bounded.projection().is_set, vec![true]);
}

#[test]
fn native_shell_owns_plaintext_outside_pane_persisted_and_remote_models_and_blocks_key_fallthrough()
{
    let app = include_str!("../src/app/mod.rs").replace("\r\n", "\n");
    let pane = include_str!("../src/browser/pane.rs").replace("\r\n", "\n");

    for (surface, source) in [
        (
            "browser snapshot and journal",
            include_str!("../src/browser/model.rs"),
        ),
        (
            "browser resources",
            include_str!("../src/browser/resources.rs"),
        ),
        ("persistence", include_str!("../src/persistence/mod.rs")),
        ("remote snapshot", include_str!("../src/remote/mod.rs")),
        (
            "persisted app state",
            include_str!("../src/state/app_state.rs"),
        ),
        (
            "runtime state",
            include_str!("../src/state/runtime_state.rs"),
        ),
    ] {
        assert!(
            !source.contains("BrowserReplaySecretPromptVault")
                && !source.contains("browser_replay_secret_prompt"),
            "{surface} must not retain the replay secret prompt vault"
        );
    }

    let shell_start = app.find("struct NativeShell {").unwrap();
    let shell_end = app[shell_start..].find("\n}").unwrap() + shell_start;
    let shell = &app[shell_start..shell_end];
    assert!(shell.contains("browser_replay_secret_prompt: Option<BrowserReplaySecretPromptVault>"));

    let ui_start = app.find("struct BrowserWorkspaceUiState {").unwrap();
    let ui_end = app[ui_start..].find("\n}").unwrap() + ui_start;
    let ui = &app[ui_start..ui_end];
    assert!(!ui.contains("BrowserReplaySecretPromptVault"));
    assert!(!ui.contains("secret_value"));

    let transient_start = pane.find("pub struct BrowserPaneTransient {").unwrap();
    let transient_end = pane[transient_start..].find("\n}").unwrap() + transient_start;
    let transient = &pane[transient_start..transient_end];
    assert!(transient.contains("Option<BrowserReplaySecretPromptProjection>"));
    assert!(!transient.contains("BrowserReplaySecretPromptVault"));

    let terminal_key_start = app.find("fn handle_terminal_key(").unwrap();
    let terminal_key_end = app[terminal_key_start..]
        .find("fn handle_terminal_scroll(")
        .unwrap()
        + terminal_key_start;
    let terminal_key = &app[terminal_key_start..terminal_key_end];
    assert!(terminal_key.contains("browser_replay_secret_prompt.is_some()"));
    assert!(terminal_key.contains("window.prevent_default()"));
    assert!(
        terminal_key
            .find("browser_replay_secret_prompt.is_some()")
            .unwrap()
            < terminal_key.find("write_user_text_to_session").unwrap()
    );

    let route_start = app.find("fn sync_browser_host_visibility(").unwrap();
    let route_end = app[route_start..].find("fn active_browser_model(").unwrap() + route_start;
    assert!(app[route_start..route_end].contains("close_browser_replay_secret_prompt_for_route"));
    for (entry, next) in [
        ("fn open_editor(", "fn open_editor_with_field("),
        (
            "fn open_add_project_action(",
            "fn open_process_monitor_action(",
        ),
        (
            "fn open_process_monitor_action(",
            "fn close_process_monitor_action(",
        ),
    ] {
        let start = app.find(entry).unwrap();
        let end = app[start..].find(next).unwrap() + start;
        assert!(
            app[start..end].contains("close_browser_replay_secret_prompt_for_route(None)"),
            "{entry} must close the hidden replay secret prompt"
        );
    }
    for contract in [
        "fn install_browser_replay_secret_prompt(",
        "fn focus_browser_replay_secret_prompt(",
        "fn edit_browser_replay_secret_prompt(",
        "fn backspace_browser_replay_secret_prompt(",
        "fn submit_browser_replay_secret_prompt(",
        "fn cancel_browser_replay_secret_prompt(",
    ] {
        assert!(
            app.contains(contract),
            "missing NativeShell contract: {contract}"
        );
    }
    let install_start = app
        .find("fn install_browser_replay_secret_prompt(")
        .unwrap();
    let install_end = app[install_start..]
        .find("fn focus_browser_replay_secret_prompt(")
        .unwrap()
        + install_start;
    assert!(app[install_start..install_end].contains("replay_replaced"));

    let secret_key_start = app.find("fn handle_browser_replay_secret_key(").unwrap();
    let secret_key_end = app[secret_key_start..]
        .find("fn handle_browser_address_key(")
        .unwrap()
        + secret_key_start;
    let secret_key = &app[secret_key_start..secret_key_end];
    assert!(secret_key.contains("backspace_browser_replay_secret_prompt"));
    assert!(secret_key.contains("edit_browser_replay_secret_prompt"));
    assert!(secret_key.contains("event.keystroke.key_char"));
    assert!(secret_key.contains("window.prevent_default()"));
    assert!(!secret_key.contains("read_from_clipboard"));
    assert!(!secret_key.contains("write_user_text_to_session"));

    let pane_action_start = app.find("fn apply_browser_pane_action(").unwrap();
    let pane_action_end = app[pane_action_start..]
        .find("fn handle_browser_replay_secret_key(")
        .unwrap()
        + pane_action_start;
    let pane_action = &app[pane_action_start..pane_action_end];
    for action in ["CreateTab", "SelectTab", "CloseTab", "Collapse"] {
        assert!(
            pane_action.contains(&format!("BrowserPaneAction::{action}")),
            "{action} must close an active secret prompt"
        );
    }
    assert!(pane_action.contains("cancel_browser_replay_secret_prompt"));

    assert!(pane.contains("browser_replay_secret_mask(input.is_set)"));
    assert!(!pane.contains("secret_prompt.value"));
}
