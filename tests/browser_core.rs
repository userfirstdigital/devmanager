use devmanager::browser::{
    classify_upload_path, load_recipe, recipe_path, save_recipe, BrowserAnnotation,
    BrowserApprovalPolicy, BrowserBounds, BrowserElementRef, BrowserError, BrowserJournalActor,
    BrowserJournalEntry, BrowserLocator, BrowserRecipeAction, BrowserRecipeInput,
    BrowserRecipeInputKind, BrowserRecipeStep, BrowserRecipeV1, BrowserResourceId, BrowserRevision,
    BrowserRisk, BrowserStorageLayout, BrowserTabSnapshot, BrowserViewport, BrowserWorkspaceKey,
    BrowserWorkspaceSnapshot, BROWSER_RECIPE_SCHEMA_VERSION,
};
use devmanager::models::{SessionState, SessionTab, Settings, TabType};
use devmanager::state::AppState;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

struct TestDir(PathBuf);

impl TestDir {
    fn new(label: &str) -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "devmanager-browser-{label}-{}-{nanos:x}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path).expect("create test directory");
        Self(path)
    }

    fn path(&self) -> &std::path::Path {
        &self.0
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[cfg(windows)]
fn create_file_symlink(target: &std::path::Path, link: &std::path::Path) -> std::io::Result<()> {
    std::os::windows::fs::symlink_file(target, link)
}

#[cfg(unix)]
fn create_file_symlink(target: &std::path::Path, link: &std::path::Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}

fn sample_browser_recipe() -> BrowserRecipeV1 {
    BrowserRecipeV1 {
        schema_version: BROWSER_RECIPE_SCHEMA_VERSION,
        id: "checkout-review".to_string(),
        name: "Checkout review".to_string(),
        description: "Review checkout without submitting".to_string(),
        start_url: "https://example.test/checkout".to_string(),
        viewport: BrowserViewport::default(),
        inputs: vec![
            BrowserRecipeInput {
                name: "query".to_string(),
                kind: BrowserRecipeInputKind::Text,
                default_value: Some("books".to_string()),
            },
            BrowserRecipeInput {
                name: "password".to_string(),
                kind: BrowserRecipeInputKind::Secret,
                default_value: None,
            },
        ],
        steps: vec![BrowserRecipeStep {
            id: "open-checkout".to_string(),
            action: BrowserRecipeAction::Navigate,
            locator: Some(BrowserLocator {
                accessibility_role: Some("link".to_string()),
                accessibility_name: Some("Checkout".to_string()),
                test_id: None,
                css_selectors: vec!["a.checkout".to_string()],
            }),
            value_ref: Some("query".to_string()),
            wait_condition: Some("networkIdle".to_string()),
            assertions: vec!["urlContains:/checkout".to_string()],
        }],
    }
}

#[test]
fn browser_legacy_session_tab_json_omits_absent_workspace() {
    let json = r#"{
        "id": "claude-tab",
        "type": "claude",
        "projectId": "project-1",
        "ptySessionId": "pty-ephemeral"
    }"#;

    let tab: SessionTab = serde_json::from_str(json).expect("legacy session tab");

    assert!(tab.browser_workspace.is_none());
    let serialized = serde_json::to_value(tab).expect("serialize session tab");
    assert!(serialized.get("browserWorkspace").is_none());
}

#[test]
fn browser_session_normalization_preserves_only_ai_workspaces() {
    let json = r#"{
        "openTabs": [
            {"id":"server","type":"server","projectId":"project-1","browserWorkspace":{}},
            {"id":"claude","type":"claude","projectId":"project-1","browserWorkspace":{}},
            {"id":"codex","type":"codex","projectId":"project-1","browserWorkspace":{}},
            {"id":"ssh","type":"ssh","projectId":"project-1","browserWorkspace":{}}
        ],
        "activeTabId": "claude"
    }"#;

    let normalized: SessionState = serde_json::from_str::<SessionState>(json)
        .expect("session state")
        .normalize();

    assert!(normalized.open_tabs[0].browser_workspace.is_none());
    assert!(normalized.open_tabs[1].browser_workspace.is_some());
    assert!(normalized.open_tabs[2].browser_workspace.is_some());
    assert!(normalized.open_tabs[3].browser_workspace.is_none());

    let round_trip: SessionState = serde_json::from_str(
        &serde_json::to_string(&normalized).expect("serialize normalized session"),
    )
    .expect("round-trip normalized session");
    assert_eq!(round_trip, normalized);
}

#[test]
fn browser_enabled_uses_the_platform_default_for_legacy_settings() {
    let legacy: Settings = serde_json::from_str("{}").expect("legacy settings");

    assert_eq!(Settings::default().browser_enabled, cfg!(windows));
    assert_eq!(legacy.browser_enabled, cfg!(windows));
}

#[test]
fn browser_app_state_updates_workspaces_only_for_ai_tabs() {
    let mut state = AppState::default();
    state.open_ai_tab(
        "project-1",
        TabType::Claude,
        "claude-tab".to_string(),
        "claude-pty".to_string(),
        None,
    );
    state.open_ai_tab(
        "project-1",
        TabType::Codex,
        "codex-tab".to_string(),
        "codex-pty".to_string(),
        None,
    );
    state.ensure_server_tab("project-1", "server-tab", None);
    state.open_ssh_tab("project-1", "ssh-connection", None);
    let before = state.revision();

    assert!(state.update_browser_workspace("claude-tab", |_| {}));
    assert!(state.browser_workspace("claude-tab").is_some());
    assert!(state.revision() > before);

    let after_claude = state.revision();
    assert!(state.update_browser_workspace("codex-tab", |_| {}));
    assert!(state.browser_workspace("codex-tab").is_some());
    assert!(state.revision() > after_claude);

    let after_ai = state.revision();
    assert!(!state.update_browser_workspace("server-tab", |_| {}));
    assert!(!state.update_browser_workspace("ssh-connection-tab", |_| {}));
    assert!(!state.update_browser_workspace("missing-tab", |_| {}));
    assert_eq!(state.revision(), after_ai);
    assert!(state.browser_workspace("server-tab").is_none());
    assert!(state.browser_workspace("ssh-connection-tab").is_none());
}

#[test]
fn browser_workspace_keys_reject_blanks_and_ignore_pty_identity() {
    assert!(matches!(
        BrowserWorkspaceKey::new("", "ai-tab"),
        Err(BrowserError::InvalidWorkspaceKey { .. })
    ));
    assert!(matches!(
        BrowserWorkspaceKey::new("project-1", "  "),
        Err(BrowserError::InvalidWorkspaceKey { .. })
    ));

    let mut state = AppState::default();
    state.open_ai_tab(
        "project-stable",
        TabType::Claude,
        "tab-stable".to_string(),
        "pty-ephemeral".to_string(),
        None,
    );
    let key = state
        .browser_workspace_key("tab-stable")
        .expect("AI workspace key");

    assert_eq!(key.project_id, "project-stable");
    assert_eq!(key.ai_tab_id, "tab-stable");
    assert_ne!(key.ai_tab_id, "pty-ephemeral");
    assert!(state.browser_workspace_key("missing").is_none());
}

#[test]
fn browser_snapshot_defaults_clamps_revisions_and_rejects_stale_refs() {
    let mut snapshot = BrowserWorkspaceSnapshot::default();

    assert!(!snapshot.pane_open);
    assert_eq!(snapshot.split_percent, 50);
    assert_eq!(snapshot.revision, BrowserRevision(0));
    assert_eq!(
        BrowserViewport::default(),
        BrowserViewport {
            width: 1280,
            height: 720,
            scale_percent: 100,
        }
    );

    snapshot.set_split_percent(10);
    assert_eq!(snapshot.split_percent, 25);
    snapshot.set_split_percent(90);
    assert_eq!(snapshot.split_percent, 75);

    assert_eq!(snapshot.advance_revision(), BrowserRevision(1));
    snapshot.revision = BrowserRevision(u64::MAX);
    assert_eq!(snapshot.advance_revision(), BrowserRevision(u64::MAX));

    let stale = BrowserElementRef {
        revision: BrowserRevision(u64::MAX - 1),
        locator: BrowserLocator::default(),
        backend_node_id: Some(42),
    };
    assert!(matches!(
        snapshot.validate_element_ref(&stale),
        Err(BrowserError::StaleReference {
            expected: BrowserRevision(u64::MAX),
            actual: BrowserRevision(value),
        }) if value == u64::MAX - 1
    ));

    let current = BrowserElementRef {
        revision: snapshot.revision,
        locator: BrowserLocator::default(),
        backend_node_id: None,
    };
    assert_eq!(snapshot.validate_element_ref(&current), Ok(()));
}

#[test]
fn browser_persisted_model_payloads_are_camel_case_and_round_trip() {
    let locator = BrowserLocator {
        accessibility_role: Some("button".to_string()),
        accessibility_name: Some("Submit".to_string()),
        test_id: Some("submit".to_string()),
        css_selectors: vec!["button.primary".to_string(), "#submit".to_string()],
    };
    let annotation = BrowserAnnotation {
        id: "annotation-1".to_string(),
        comment: "Confirm before submitting".to_string(),
        url: "https://example.test/checkout".to_string(),
        locator: locator.clone(),
        bounds: BrowserBounds {
            x: 10,
            y: 20,
            width: 300,
            height: 40,
        },
        viewport: BrowserViewport::default(),
        screenshot_resource: BrowserResourceId("screenshot-1".to_string()),
        computed_styles: BTreeMap::from([("display".to_string(), "block".to_string())]),
        resolved: false,
    };
    let journal = BrowserJournalEntry {
        id: "journal-1".to_string(),
        actor: BrowserJournalActor::Agent,
        intent: "Inspect checkout".to_string(),
        url: "https://example.test/checkout".to_string(),
        started_at: "2026-07-16T12:00:00Z".to_string(),
        duration_ms: 250,
        result: "ok".to_string(),
        resource_ids: vec![BrowserResourceId("screenshot-1".to_string())],
    };
    let mut snapshot = BrowserWorkspaceSnapshot::default();
    snapshot.tabs.push(BrowserTabSnapshot {
        id: "page-1".to_string(),
        title: "Checkout".to_string(),
        url: "https://example.test/checkout".to_string(),
        viewport: BrowserViewport::default(),
    });
    snapshot.selected_tab_id = Some("page-1".to_string());
    snapshot.annotations.push(annotation);
    snapshot.journal_entries.push(journal);

    let value = serde_json::to_value(&snapshot).expect("serialize browser snapshot");
    assert!(value.get("paneOpen").is_some());
    assert!(value.get("splitPercent").is_some());
    assert!(value.get("selectedTabId").is_some());
    let annotation = &value["annotations"][0];
    assert!(annotation.get("screenshotResource").is_some());
    assert!(annotation.get("computedStyles").is_some());
    assert!(annotation["locator"].get("cssSelectors").is_some());
    let journal = &value["journalEntries"][0];
    assert_eq!(journal["actor"], "agent");
    assert!(journal.get("startedAt").is_some());
    assert!(journal.get("durationMs").is_some());
    assert!(journal.get("resourceIds").is_some());

    let round_trip: BrowserWorkspaceSnapshot =
        serde_json::from_value(value).expect("round-trip browser snapshot");
    assert_eq!(round_trip, snapshot);
}

#[test]
fn browser_error_taxonomy_is_serializable_and_displayable() {
    let errors = vec![
        BrowserError::MissingFile {
            path: PathBuf::from("missing.txt"),
        },
        BrowserError::OutsideWorkspace {
            path: PathBuf::from("outside.txt"),
        },
        BrowserError::InvalidRecipe {
            message: "invalid".to_string(),
        },
        BrowserError::UnsupportedRecipeVersion { version: 2 },
        BrowserError::Interrupted,
        BrowserError::Timeout {
            operation: "navigate".to_string(),
        },
        BrowserError::NavigationFailure {
            url: "https://example.test".to_string(),
            message: "offline".to_string(),
        },
        BrowserError::CrashedView {
            message: "renderer exited".to_string(),
        },
        BrowserError::BlockedPermission {
            permission: "camera".to_string(),
        },
        BrowserError::UnavailablePlatform {
            platform: "linux".to_string(),
        },
    ];

    for error in errors {
        assert!(!error.to_string().trim().is_empty());
        let json = serde_json::to_string(&error).expect("serialize browser error");
        let round_trip: BrowserError =
            serde_json::from_str(&json).expect("round-trip browser error");
        assert_eq!(round_trip, error);
    }
}

#[test]
fn browser_trust_project_policy_confirms_every_non_normal_risk() {
    let policy = BrowserApprovalPolicy::trust_project();

    assert!(!policy.requires_confirmation(BrowserRisk::Normal));
    for risk in [
        BrowserRisk::Financial,
        BrowserRisk::Destructive,
        BrowserRisk::AccountSecurity,
        BrowserRisk::PermissionChange,
        BrowserRisk::OutsideWorkspaceFile,
        BrowserRisk::OsPermission,
    ] {
        assert!(
            policy.requires_confirmation(risk),
            "{risk:?} must require confirmation"
        );
    }
}

#[test]
fn browser_upload_classification_canonicalizes_and_contains_paths() {
    let temp = TestDir::new("upload-policy");
    let workspace = temp.path().join("workspace");
    let outside_dir = temp.path().join("outside");
    std::fs::create_dir_all(&workspace).expect("workspace");
    std::fs::create_dir_all(&outside_dir).expect("outside directory");
    let inside = workspace.join("inside.txt");
    let outside = outside_dir.join("outside.txt");
    std::fs::write(&inside, "inside").expect("inside file");
    std::fs::write(&outside, "outside").expect("outside file");

    let (inside_path, inside_risk) =
        classify_upload_path(&workspace, &inside).expect("classify inside file");
    assert_eq!(
        inside_path,
        inside.canonicalize().expect("canonical inside")
    );
    assert_eq!(inside_risk, BrowserRisk::Normal);

    let (outside_path, outside_risk) =
        classify_upload_path(&workspace, &outside).expect("classify outside file");
    assert_eq!(
        outside_path,
        outside.canonicalize().expect("canonical outside")
    );
    assert_eq!(outside_risk, BrowserRisk::OutsideWorkspaceFile);

    let missing = workspace.join("missing.txt");
    assert!(matches!(
        classify_upload_path(&workspace, &missing),
        Err(BrowserError::MissingFile { path }) if path == missing
    ));

    let escaping_link = workspace.join("escaping-link.txt");
    if create_file_symlink(&outside, &escaping_link).is_ok() {
        let (resolved, risk) =
            classify_upload_path(&workspace, &escaping_link).expect("classify escaping symlink");
        assert_eq!(
            resolved,
            outside.canonicalize().expect("canonical symlink target")
        );
        assert_eq!(risk, BrowserRisk::OutsideWorkspaceFile);
    }
}

#[test]
fn browser_storage_layout_is_hashed_stable_isolated_and_created() {
    let temp = TestDir::new("storage");
    let app_config_dir = temp.path().join("config");
    let project_id = "private/project:id";
    let first = BrowserStorageLayout::new(&app_config_dir, project_id);
    let same = BrowserStorageLayout::new(&app_config_dir, project_id);
    let other = BrowserStorageLayout::new(&app_config_dir, "another-project");

    assert_eq!(first, same);
    assert_ne!(first, other);
    assert_eq!(first.profile_dir(), first.profile_dir.as_path());
    assert_eq!(first.downloads_dir(), first.downloads_dir.as_path());
    assert_eq!(first.resources_dir(), first.resources_dir.as_path());

    let browser_root = app_config_dir.join("browser");
    for path in [
        &first.profile_dir,
        &first.downloads_dir,
        &first.resources_dir,
    ] {
        assert!(path.starts_with(&browser_root));
        assert!(!path.to_string_lossy().contains(project_id));
        let hash = path
            .file_name()
            .and_then(|value| value.to_str())
            .expect("hash path component");
        assert_eq!(hash.len(), 64);
        assert!(hash
            .chars()
            .all(|character| character.is_ascii_digit() || ('a'..='f').contains(&character)));
        assert!(!path.exists());
    }

    first.ensure().expect("create browser storage layout");
    assert!(first.profile_dir.is_dir());
    assert!(first.downloads_dir.is_dir());
    assert!(first.resources_dir.is_dir());
}

#[test]
fn browser_recipe_schema_v1_is_camel_case_and_round_trips() {
    assert_eq!(BROWSER_RECIPE_SCHEMA_VERSION, 1);
    let recipe = sample_browser_recipe();

    let value = serde_json::to_value(&recipe).expect("serialize recipe");
    assert_eq!(value["schemaVersion"], 1);
    assert_eq!(value["startUrl"], "https://example.test/checkout");
    assert!(value.get("schema_version").is_none());
    assert_eq!(value["inputs"][0]["defaultValue"], "books");
    assert!(value["inputs"][1].get("defaultValue").is_none());
    assert_eq!(value["steps"][0]["valueRef"], "query");
    assert_eq!(value["steps"][0]["waitCondition"], "networkIdle");

    let actions = [
        (BrowserRecipeAction::Navigate, "navigate"),
        (BrowserRecipeAction::Click, "click"),
        (BrowserRecipeAction::Hover, "hover"),
        (BrowserRecipeAction::Focus, "focus"),
        (BrowserRecipeAction::Type, "type"),
        (BrowserRecipeAction::Clear, "clear"),
        (BrowserRecipeAction::Select, "select"),
        (BrowserRecipeAction::Keypress, "keypress"),
        (BrowserRecipeAction::Scroll, "scroll"),
        (BrowserRecipeAction::DragDrop, "dragDrop"),
        (BrowserRecipeAction::Wait, "wait"),
        (BrowserRecipeAction::Screenshot, "screenshot"),
        (BrowserRecipeAction::Cdp, "cdp"),
    ];
    for (action, expected) in actions {
        assert_eq!(serde_json::to_value(action).unwrap(), expected);
    }

    let input_kinds = [
        (BrowserRecipeInputKind::Text, "text"),
        (BrowserRecipeInputKind::Url, "url"),
        (BrowserRecipeInputKind::File, "file"),
        (BrowserRecipeInputKind::Secret, "secret"),
    ];
    for (kind, expected) in input_kinds {
        assert_eq!(serde_json::to_value(kind).unwrap(), expected);
    }

    let round_trip: BrowserRecipeV1 = serde_json::from_value(value).expect("round-trip recipe");
    assert_eq!(round_trip, recipe);
}

#[test]
fn browser_recipe_validation_rejects_unsafe_or_secret_bearing_schema() {
    assert_eq!(sample_browser_recipe().validate(), Ok(()));

    let mut recipe = sample_browser_recipe();
    recipe.schema_version = 2;
    assert!(matches!(
        recipe.validate(),
        Err(BrowserError::UnsupportedRecipeVersion { version: 2 })
    ));

    for invalid_id in ["", "   ", "../escape", "nested/path", "-leading"] {
        let mut recipe = sample_browser_recipe();
        recipe.id = invalid_id.to_string();
        assert!(matches!(
            recipe.validate(),
            Err(BrowserError::InvalidRecipe { .. })
        ));
    }

    let mut recipe = sample_browser_recipe();
    recipe.name = "  ".to_string();
    assert!(matches!(
        recipe.validate(),
        Err(BrowserError::InvalidRecipe { .. })
    ));

    let mut recipe = sample_browser_recipe();
    recipe.inputs.push(recipe.inputs[0].clone());
    assert!(matches!(
        recipe.validate(),
        Err(BrowserError::InvalidRecipe { .. })
    ));

    let mut recipe = sample_browser_recipe();
    recipe
        .inputs
        .iter_mut()
        .find(|input| input.kind == BrowserRecipeInputKind::Secret)
        .expect("secret input")
        .default_value = Some(String::new());
    assert!(matches!(
        recipe.validate(),
        Err(BrowserError::InvalidRecipe { .. })
    ));
}

#[test]
fn browser_recipe_direct_serialization_rejects_secret_defaults() {
    let mut recipe = sample_browser_recipe();
    recipe
        .inputs
        .iter_mut()
        .find(|input| input.kind == BrowserRecipeInputKind::Secret)
        .expect("secret input")
        .default_value = Some("must-not-be-serialized".to_string());

    let error = serde_json::to_string(&recipe).expect_err("secret default must be rejected");
    let message = error.to_string();
    assert!(message.contains("secret input default"));
    assert!(!message.contains("must-not-be-serialized"));
}

#[test]
fn browser_recipe_save_and_load_use_the_exact_pretty_repository_path() {
    let temp = TestDir::new("recipe-persistence");
    let project_root = temp.path().join("project");
    std::fs::create_dir_all(&project_root).expect("project root");
    let recipe = sample_browser_recipe();
    let expected_path = project_root
        .join(".devmanager")
        .join("browser-workflows")
        .join("checkout-review.json");

    assert_eq!(
        recipe_path(&project_root, "checkout-review").expect("recipe path"),
        expected_path
    );
    assert!(matches!(
        recipe_path(&project_root, "../escape"),
        Err(BrowserError::InvalidRecipe { .. })
    ));

    let saved_path = save_recipe(&project_root, &recipe).expect("save recipe");
    assert_eq!(saved_path, expected_path);
    let json = std::fs::read_to_string(&saved_path).expect("saved recipe JSON");
    assert!(json.ends_with('\n'));
    assert!(json.contains("\n  \"schemaVersion\": 1,"));
    assert!(!json.contains("schema_version"));

    let loaded = load_recipe(&project_root, "checkout-review").expect("load recipe");
    assert_eq!(loaded, recipe);
}

#[test]
fn browser_recipe_load_reports_unknown_schema_before_v1_parse_errors() {
    let temp = TestDir::new("recipe-version");
    let project_root = temp.path().join("project");
    let path = recipe_path(&project_root, "future-recipe").expect("future recipe path");
    std::fs::create_dir_all(path.parent().expect("workflow directory"))
        .expect("create workflow directory");
    std::fs::write(&path, "{\"schemaVersion\":99,\"futureShape\":true}\n")
        .expect("write future recipe");

    assert!(matches!(
        load_recipe(&project_root, "future-recipe"),
        Err(BrowserError::UnsupportedRecipeVersion { version: 99 })
    ));
}

#[test]
fn browser_workspace_is_cleared_when_an_ai_tab_becomes_non_ai() {
    let mut state = AppState::default();
    state.open_ai_tab(
        "project-1",
        TabType::Claude,
        "server-collision".to_string(),
        "claude-pty".to_string(),
        None,
    );
    state.update_browser_workspace("server-collision", |workspace| {
        workspace.pane_open = true;
    });
    assert!(state
        .find_tab("server-collision")
        .unwrap()
        .browser_workspace
        .is_some());

    state.ensure_server_tab("project-1", "server-collision", None);
    assert!(state
        .find_tab("server-collision")
        .unwrap()
        .browser_workspace
        .is_none());

    state.open_ai_tab(
        "project-1",
        TabType::Codex,
        "ssh-connection-tab".to_string(),
        "codex-pty".to_string(),
        None,
    );
    state.update_browser_workspace("ssh-connection-tab", |_| {});
    assert!(state
        .find_tab("ssh-connection-tab")
        .unwrap()
        .browser_workspace
        .is_some());

    state.open_ssh_tab("project-1", "ssh-connection", None);
    assert!(state
        .find_tab("ssh-connection-tab")
        .unwrap()
        .browser_workspace
        .is_none());
}
