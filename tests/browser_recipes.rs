use devmanager::browser::{
    list_recipes, load_recipe, recipe_path, save_recipe, BrowserError, BrowserRecipeAction,
    BrowserRecipeAssertion, BrowserRecipeInput, BrowserRecipeInputKind, BrowserRecipeLocator,
    BrowserRecipeStep, BrowserRecipeV1, BrowserRecipeValue, BrowserRecipeViewport,
    BrowserRecipeWait, BROWSER_RECIPE_SCHEMA_VERSION,
};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Barrier};
use std::time::{SystemTime, UNIX_EPOCH};

struct TestDir(PathBuf);

impl TestDir {
    fn new(label: &str) -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "devmanager-browser-recipe-{label}-{}-{nanos:x}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path).expect("create test directory");
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn checkout_locator() -> BrowserRecipeLocator {
    BrowserRecipeLocator {
        accessibility_role: Some("textbox".to_string()),
        accessibility_name: Some("Search".to_string()),
        test_id: Some("checkout-search".to_string()),
        css_selectors: vec!["input[name='query']".to_string()],
    }
}

fn sample_recipe() -> BrowserRecipeV1 {
    BrowserRecipeV1 {
        schema_version: BROWSER_RECIPE_SCHEMA_VERSION,
        id: "checkout-review".to_string(),
        name: "Checkout review".to_string(),
        description: "Review checkout without submitting".to_string(),
        start_url: "https://example.test/checkout".to_string(),
        viewport: BrowserRecipeViewport {
            width: 1280,
            height: 720,
            scale_percent: 100,
        },
        inputs: vec![BrowserRecipeInput {
            name: "query".to_string(),
            kind: BrowserRecipeInputKind::Text,
            default_value: Some("books".to_string()),
        }],
        steps: vec![BrowserRecipeStep {
            id: "enter-query".to_string(),
            action: BrowserRecipeAction::Type {
                locator: checkout_locator(),
                value: BrowserRecipeValue::Input {
                    name: "query".to_string(),
                },
            },
            wait: Some(BrowserRecipeWait::ElementVisible {
                locator: checkout_locator(),
                timeout_ms: 5_000,
            }),
            assertions: vec![BrowserRecipeAssertion::Url {
                value: BrowserRecipeValue::Literal {
                    value: "https://example.test/checkout".to_string(),
                },
                exact: true,
            }],
        }],
    }
}

#[test]
fn browser_recipe_strict_typed_v1_round_trips_with_deterministic_bytes() {
    let temp = TestDir::new("typed-round-trip");
    let recipe = sample_recipe();

    let first_path = save_recipe(temp.path(), &recipe).expect("first save");
    let first = std::fs::read(&first_path).expect("first bytes");
    let second_path = save_recipe(temp.path(), &recipe).expect("second save");
    let second = std::fs::read(&second_path).expect("second bytes");

    assert_eq!(first, second);
    assert_eq!(
        format!("{:x}", Sha256::digest(&first)),
        "ad4cdca4659936b33d9280a9f638d509d9b0d565e0778a5202d1969915602785"
    );
    assert_eq!(first.last(), Some(&b'\n'));
    assert!(
        std::fs::read_dir(first_path.parent().expect("workflow directory"))
            .expect("list workflow directory")
            .all(|entry| !entry
                .expect("workflow entry")
                .file_name()
                .to_string_lossy()
                .ends_with(".tmp"))
    );
    let json: Value = serde_json::from_slice(&first).expect("strict recipe JSON");
    assert_eq!(json["steps"][0]["action"]["type"], "type");
    assert_eq!(json["steps"][0]["action"]["value"]["type"], "input");
    assert_eq!(json["steps"][0]["wait"]["type"], "elementVisible");
    assert_eq!(json["steps"][0]["assertions"][0]["type"], "url");
    assert!(json["steps"][0].get("valueRef").is_none());
    assert!(json["steps"][0].get("waitCondition").is_none());

    assert_eq!(
        load_recipe(temp.path(), "checkout-review").expect("load strict recipe"),
        recipe
    );
}

#[test]
fn browser_recipe_rejects_unknown_nested_fields_and_the_old_flat_step_shape() {
    let base = serde_json::to_value(sample_recipe()).expect("serialize strict recipe");
    let nested_unknowns = [
        "/viewport/future",
        "/inputs/0/future",
        "/steps/0/future",
        "/steps/0/action/future",
        "/steps/0/action/locator/future",
        "/steps/0/action/value/future",
        "/steps/0/wait/future",
        "/steps/0/wait/locator/future",
        "/steps/0/assertions/0/future",
        "/steps/0/assertions/0/value/future",
    ];

    for pointer in nested_unknowns {
        let mut candidate = base.clone();
        candidate
            .pointer_mut(pointer.rsplit_once('/').map_or("", |(parent, _)| parent))
            .and_then(Value::as_object_mut)
            .expect("nested object")
            .insert("future".to_string(), json!(true));
        let error = serde_json::from_value::<BrowserRecipeV1>(candidate)
            .expect_err("unknown nested field must fail");
        assert!(
            error.to_string().contains("unknown field"),
            "unexpected error for {pointer}: {error}"
        );
    }

    let old_flat = json!({
        "schemaVersion": 1,
        "id": "legacy-flat",
        "name": "Legacy flat",
        "description": "must not be interpreted",
        "startUrl": "https://example.test/",
        "viewport": { "width": 1280, "height": 720, "scalePercent": 100 },
        "inputs": [],
        "steps": [{
            "id": "old-step",
            "action": "type",
            "locator": { "testId": "name" },
            "valueRef": "name",
            "waitCondition": "networkIdle",
            "assertions": ["urlContains:/done"]
        }]
    });
    assert!(serde_json::from_value::<BrowserRecipeV1>(old_flat).is_err());
}

#[test]
fn browser_recipe_load_reports_future_version_before_partial_shape_parsing() {
    let temp = TestDir::new("future-version");
    let path = temp
        .path()
        .join(".devmanager")
        .join("browser-workflows")
        .join("future.json");
    std::fs::create_dir_all(path.parent().expect("workflow directory"))
        .expect("create workflow directory");
    std::fs::write(
        &path,
        br#"{"schemaVersion":2,"steps":[{"futureSecret":"must-not-parse"}]}"#,
    )
    .expect("write future recipe");

    assert!(matches!(
        load_recipe(temp.path(), "future"),
        Err(BrowserError::UnsupportedRecipeVersion { version: 2 })
    ));
}

#[test]
fn browser_recipe_list_reads_only_direct_safe_slug_json_files_in_id_order() {
    let temp = TestDir::new("strict-list");
    let mut second = sample_recipe();
    second.id = "z-last".to_string();
    second.name = "Last".to_string();
    save_recipe(temp.path(), &second).expect("save last recipe");
    let mut first = sample_recipe();
    first.id = "a-first".to_string();
    first.name = "First".to_string();
    save_recipe(temp.path(), &first).expect("save first recipe");

    let root = temp.path().join(".devmanager").join("browser-workflows");
    std::fs::write(root.join("README.md"), "not a recipe").expect("write README");
    std::fs::write(root.join("ignored.json.tmp"), "not a recipe").expect("write temp");
    std::fs::write(root.join("-unsafe.json"), "not a recipe").expect("write unsafe slug");
    std::fs::create_dir_all(root.join("nested")).expect("create nested directory");
    std::fs::write(root.join("nested").join("hidden.json"), "not a recipe")
        .expect("write nested recipe");

    let recipes = list_recipes(temp.path()).expect("list direct recipes");
    assert_eq!(
        recipes
            .iter()
            .map(|recipe| recipe.id.as_str())
            .collect::<Vec<_>>(),
        ["a-first", "z-last"]
    );
}

#[test]
fn browser_recipe_paths_reject_traversal_and_non_directory_components() {
    let temp = TestDir::new("path-containment");
    for unsafe_id in [
        "",
        "../escape",
        "nested/path",
        "nested\\path",
        ".hidden",
        "-leading",
        "trailing-",
    ] {
        assert!(matches!(
            recipe_path(temp.path(), unsafe_id),
            Err(BrowserError::InvalidRecipe { .. })
        ));
    }

    let project = temp.path().join("project");
    std::fs::create_dir_all(&project).expect("create project");
    std::fs::write(project.join(".devmanager"), "not a directory")
        .expect("write hostile path component");
    assert!(matches!(
        save_recipe(&project, &sample_recipe()),
        Err(BrowserError::OutsideWorkspace { .. })
    ));
}

#[test]
fn browser_recipe_serialization_rejects_credential_material_without_echoing_it() {
    let mut recipe = sample_recipe();
    recipe.description = "Authorization: Bearer checkpoint-secret-123".to_string();

    let error = serde_json::to_string(&recipe).expect_err("credential material must not serialize");
    let message = error.to_string();
    assert!(message.contains("credential-like material"));
    assert!(!message.contains("checkpoint-secret-123"));
}

#[test]
fn browser_recipe_identifiers_reject_bare_credentials_on_every_wire_boundary() {
    for credential_id in [
        "sk-proj-abcdefghijklmnopqrstuvwxyz0123456789ABCDEFG",
        "ghp_abcdefghijklmnopqrstuvwxyz0123456789",
    ] {
        let mut recipe = sample_recipe();
        recipe.id = credential_id.to_string();
        let validation = recipe.validate().expect_err("recipe id must be rejected");
        assert!(!format!("{validation:?}").contains(credential_id));
        let serialization = serde_json::to_string(&recipe)
            .expect_err("credential-shaped recipe id must not serialize");
        assert!(!serialization.to_string().contains(credential_id));

        let mut recipe_wire = serde_json::to_value(sample_recipe()).unwrap();
        recipe_wire["id"] = json!(credential_id);
        let deserialization = serde_json::from_value::<BrowserRecipeV1>(recipe_wire)
            .expect_err("credential-shaped recipe id must not deserialize");
        assert!(!deserialization.to_string().contains(credential_id));

        let mut step = sample_recipe().steps.remove(0);
        step.id = credential_id.to_string();
        let direct_serialization = serde_json::to_string(&step)
            .expect_err("credential-shaped step id must not serialize directly");
        assert!(!direct_serialization.to_string().contains(credential_id));

        let mut step_wire = serde_json::to_value(sample_recipe().steps.remove(0)).unwrap();
        step_wire["id"] = json!(credential_id);
        let direct_deserialization = serde_json::from_value::<BrowserRecipeStep>(step_wire)
            .expect_err("credential-shaped step id must not deserialize directly");
        assert!(!direct_deserialization.to_string().contains(credential_id));

        let mut recipe = sample_recipe();
        recipe.steps[0].id = credential_id.to_string();
        let validation = recipe.validate().expect_err("step id must be rejected");
        assert!(!format!("{validation:?}").contains(credential_id));
        let serialization = serde_json::to_string(&recipe)
            .expect_err("credential-shaped nested step id must not serialize");
        assert!(!serialization.to_string().contains(credential_id));
    }

    let mut ordinary = sample_recipe();
    ordinary.id = "sketch-project_2".to_string();
    ordinary.steps[0].id = "gh-preview_2".to_string();
    assert_eq!(ordinary.validate(), Ok(()));
    let encoded = serde_json::to_string(&ordinary).unwrap();
    assert_eq!(
        serde_json::from_str::<BrowserRecipeV1>(&encoded).unwrap(),
        ordinary
    );
    let step = ordinary.steps[0].clone();
    assert_eq!(
        serde_json::from_str::<BrowserRecipeStep>(&serde_json::to_string(&step).unwrap()).unwrap(),
        step
    );
}

#[test]
fn browser_recipe_validation_rejects_invalid_references_types_and_defaults() {
    let mut recipe = sample_recipe();
    recipe.steps[0].action = BrowserRecipeAction::Navigate {
        url: BrowserRecipeValue::Input {
            name: "missing".to_string(),
        },
    };
    assert!(matches!(
        recipe.validate(),
        Err(BrowserError::InvalidRecipe { .. })
    ));

    let mut recipe = sample_recipe();
    recipe.steps[0].action = BrowserRecipeAction::Navigate {
        url: BrowserRecipeValue::Input {
            name: "query".to_string(),
        },
    };
    assert!(matches!(
        recipe.validate(),
        Err(BrowserError::InvalidRecipe { .. })
    ));

    let mut recipe = sample_recipe();
    recipe.inputs.push(BrowserRecipeInput {
        name: "upload".to_string(),
        kind: BrowserRecipeInputKind::File,
        default_value: Some("C:\\private\\contents.bin".to_string()),
    });
    let error = serde_json::to_string(&recipe).expect_err("file default must not serialize");
    assert!(!error.to_string().contains("contents.bin"));

    let mut recipe = sample_recipe();
    recipe.inputs.push(BrowserRecipeInput {
        name: "password".to_string(),
        kind: BrowserRecipeInputKind::Secret,
        default_value: None,
    });
    recipe.steps[0].action = BrowserRecipeAction::Type {
        locator: BrowserRecipeLocator {
            accessibility_role: Some("textbox".to_string()),
            accessibility_name: Some("Password".to_string()),
            test_id: None,
            css_selectors: vec!["input[type='password']".to_string()],
        },
        value: BrowserRecipeValue::Input {
            name: "query".to_string(),
        },
    };
    assert!(matches!(
        recipe.validate(),
        Err(BrowserError::InvalidRecipe { .. })
    ));
    if let BrowserRecipeAction::Type { value, .. } = &mut recipe.steps[0].action {
        *value = BrowserRecipeValue::Input {
            name: "password".to_string(),
        };
    }
    assert_eq!(recipe.validate(), Ok(()));
}

#[test]
fn browser_recipe_input_wire_rejects_secret_and_file_defaults_on_deserialize() {
    for kind in ["secret", "file"] {
        let json = format!(
            r#"{{"name":"sensitive-input","kind":"{kind}","defaultValue":"nested-sensitive-sentinel"}}"#
        );
        let error = serde_json::from_str::<BrowserRecipeInput>(&json)
            .expect_err("sensitive input default must not deserialize");
        assert!(error.to_string().contains("input default"));
        assert!(!error.to_string().contains("nested-sensitive-sentinel"));
    }
}

#[test]
fn browser_recipe_validation_rejects_invalid_locators_values_waits_and_assertions() {
    let mut recipe = sample_recipe();
    if let BrowserRecipeAction::Type { locator, .. } = &mut recipe.steps[0].action {
        *locator = BrowserRecipeLocator {
            accessibility_role: Some("textbox".to_string()),
            accessibility_name: None,
            test_id: None,
            css_selectors: Vec::new(),
        };
    }
    assert!(matches!(
        recipe.validate(),
        Err(BrowserError::InvalidRecipe { .. })
    ));

    let mut recipe = sample_recipe();
    recipe.steps[0].wait = Some(BrowserRecipeWait::Duration { duration_ms: 0 });
    assert!(matches!(
        recipe.validate(),
        Err(BrowserError::InvalidRecipe { .. })
    ));

    let mut recipe = sample_recipe();
    recipe.steps[0].assertions = vec![BrowserRecipeAssertion::Text {
        value: BrowserRecipeValue::Literal {
            value: "   ".to_string(),
        },
        present: true,
    }];
    assert!(matches!(
        recipe.validate(),
        Err(BrowserError::InvalidRecipe { .. })
    ));

    let mut recipe = sample_recipe();
    recipe.steps.push(recipe.steps[0].clone());
    assert!(matches!(
        recipe.validate(),
        Err(BrowserError::InvalidRecipe { .. })
    ));
}

#[test]
fn browser_recipe_validation_requires_steps_and_upload_actions_for_file_targets() {
    let mut recipe = sample_recipe();
    recipe.steps.clear();
    assert!(matches!(
        recipe.validate(),
        Err(BrowserError::InvalidRecipe { .. })
    ));

    let mut recipe = sample_recipe();
    recipe.steps[0].action = BrowserRecipeAction::Type {
        locator: BrowserRecipeLocator {
            accessibility_role: Some("textbox".to_string()),
            accessibility_name: Some("Upload avatar".to_string()),
            test_id: Some("avatar-upload".to_string()),
            css_selectors: vec!["input[type='file']".to_string()],
        },
        value: BrowserRecipeValue::Literal {
            value: "raw-file-content".to_string(),
        },
    };
    assert!(matches!(
        recipe.validate(),
        Err(BrowserError::InvalidRecipe { .. })
    ));
}

#[test]
fn browser_recipe_wire_rejects_secret_and_file_content_aliases_without_echoing_values() {
    let base = serde_json::to_value(sample_recipe()).expect("serialize strict recipe");
    let cases = [
        ("", "cookies"),
        ("/inputs/0", "secretValue"),
        ("/steps/0/action", "password"),
        ("/steps/0/action/locator", "fileContents"),
        ("/steps/0/wait", "authorizationHeader"),
        ("/steps/0/assertions/0", "tokenValue"),
    ];
    for (parent, field) in cases {
        let mut candidate = base.clone();
        candidate
            .pointer_mut(parent)
            .and_then(Value::as_object_mut)
            .expect("wire object")
            .insert(field.to_string(), json!("wire-secret-sentinel-987"));
        let error = serde_json::from_value::<BrowserRecipeV1>(candidate)
            .expect_err("secret/file alias must be unknown");
        let message = error.to_string();
        assert!(
            message.contains("unknown field"),
            "unexpected error: {message}"
        );
        assert!(!message.contains("wire-secret-sentinel-987"));
    }
}

#[test]
fn browser_recipe_concurrent_saves_leave_one_complete_document_and_no_temps() {
    let temp = TestDir::new("concurrent-save");
    let barrier = Arc::new(Barrier::new(8));
    let root = Arc::new(temp.path().to_path_buf());
    let threads = (0..8)
        .map(|index| {
            let barrier = Arc::clone(&barrier);
            let root = Arc::clone(&root);
            std::thread::spawn(move || {
                let mut recipe = sample_recipe();
                recipe.description = format!("complete writer {index}");
                barrier.wait();
                save_recipe(root.as_path(), &recipe).expect("atomic concurrent save");
            })
        })
        .collect::<Vec<_>>();
    for thread in threads {
        thread.join().expect("join recipe writer");
    }

    let loaded = load_recipe(temp.path(), "checkout-review").expect("load complete winner");
    assert!(loaded.description.starts_with("complete writer "));
    let workflow = temp.path().join(".devmanager").join("browser-workflows");
    let entries = std::fs::read_dir(workflow)
        .expect("list workflow directory")
        .map(|entry| entry.expect("workflow entry").file_name())
        .collect::<Vec<_>>();
    assert_eq!(entries, [std::ffi::OsString::from("checkout-review.json")]);
}

#[cfg(windows)]
#[test]
fn browser_recipe_windows_replace_failure_preserves_old_bytes_and_cleans_temp() {
    use std::os::windows::fs::OpenOptionsExt;

    let temp = TestDir::new("windows-replace-failure");
    let recipe = sample_recipe();
    let path = save_recipe(temp.path(), &recipe).expect("save original recipe");
    let original = std::fs::read(&path).expect("read original bytes");
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .share_mode(0)
        .open(&path)
        .expect("lock destination against replacement");
    let mut replacement = recipe;
    replacement.description = "new complete document".to_string();

    assert!(matches!(
        save_recipe(temp.path(), &replacement),
        Err(BrowserError::Io { ref operation, .. }) if operation == "replace recipe atomically"
    ));
    drop(lock);

    assert_eq!(
        std::fs::read(&path).expect("read preserved bytes"),
        original
    );
    assert!(
        std::fs::read_dir(path.parent().expect("workflow directory"))
            .expect("list workflow directory")
            .all(|entry| !entry
                .expect("workflow entry")
                .file_name()
                .to_string_lossy()
                .ends_with(".tmp"))
    );
}

#[test]
fn browser_recipe_rejects_duplicate_top_level_and_nested_members() {
    let valid = serde_json::to_string_pretty(&sample_recipe()).expect("serialize valid recipe");
    let duplicate_version = valid.replacen(
        "\"schemaVersion\": 1",
        "\"schemaVersion\": 2,\n  \"schemaVersion\": 1",
        1,
    );
    let error = serde_json::from_str::<BrowserRecipeV1>(&duplicate_version)
        .expect_err("duplicate schemaVersion must not use the last value");
    assert!(error.to_string().contains("duplicate"));

    let duplicate_action_type = valid.replacen(
        "\"type\": \"type\",\n        \"locator\"",
        "\"type\": \"click\",\n        \"type\": \"type\",\n        \"locator\"",
        1,
    );
    let error = serde_json::from_str::<BrowserRecipeV1>(&duplicate_action_type)
        .expect_err("duplicate nested action member must fail");
    assert!(error.to_string().contains("duplicate"));

    let duplicate_value_name = valid.replacen(
        "\"type\": \"input\",\n          \"name\": \"query\"",
        "\"type\": \"input\",\n          \"name\": \"missing\",\n          \"name\": \"query\"",
        1,
    );
    let error = serde_json::from_str::<BrowserRecipeV1>(&duplicate_value_name)
        .expect_err("duplicate nested value member must fail");
    assert!(error.to_string().contains("duplicate"));
}

#[test]
fn browser_recipe_public_nested_wire_rejects_context_free_unsafe_values() {
    let upload_literal = r#"{
        "type":"upload",
        "locator":{"testId":"file-upload"},
        "file":{"type":"literal","value":"raw-private-file-contents"}
    }"#;
    assert!(serde_json::from_str::<BrowserRecipeAction>(upload_literal).is_err());

    let password_literal = r#"{
        "type":"type",
        "locator":{"accessibilityRole":"textbox","accessibilityName":"Password"},
        "value":{"type":"literal","value":"raw-password-value"}
    }"#;
    assert!(serde_json::from_str::<BrowserRecipeAction>(password_literal).is_err());

    assert!(serde_json::from_str::<BrowserRecipeValue>(
        r#"{"type":"literal","value":"Authorization: Bearer direct-secret"}"#
    )
    .is_err());
    assert!(
        serde_json::from_str::<BrowserRecipeWait>(r#"{"type":"duration","durationMs":0}"#).is_err()
    );
    assert!(serde_json::from_str::<BrowserRecipeViewport>(
        r#"{"width":0,"height":720,"scalePercent":100}"#
    )
    .is_err());
    assert!(
        serde_json::from_str::<BrowserRecipeLocator>(r#"{"accessibilityRole":"textbox"}"#).is_err()
    );
    assert!(serde_json::from_str::<BrowserRecipeAssertion>(
        r#"{"type":"text","value":{"type":"literal","value":"   "},"present":true}"#
    )
    .is_err());
}

#[test]
fn browser_recipe_repair_uses_one_private_domain_separated_atomic_replace_contract() {
    let recipes = include_str!("../src/browser/recipes.rs");

    assert!(recipes.contains("b\"devmanager.browser-recipe-v1.sha256\\0\""));
    assert!(recipes.contains("struct BrowserRecipeDigestV1"));
    assert!(!recipes
        .contains("derive(Debug, Clone, Serialize)\npub(crate) struct BrowserRecipeDigestV1"));
    assert!(recipes.contains("fn canonical_browser_recipe_digest"));
    assert!(recipes.contains("fn recipe_locator_at"));
    assert!(recipes.contains("fn replace_recipe_locator_at"));
    assert!(recipes.contains("fn replace_recipe_locator_atomic"));
    assert_eq!(recipes.matches("static RECIPE_WRITE_GATE").count(), 1);
    assert!(!recipes.contains("REPAIR_WRITE_GATE"));
    assert!(recipes.contains("non-cooperating external writers"));
}
