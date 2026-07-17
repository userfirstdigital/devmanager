use devmanager::browser::{
    browser_recording_review_result, browser_recording_save_would_overwrite,
    browser_recording_status_result, discard_browser_recording, effective_browser_recording_risk,
    load_recipe, save_browser_recording_review, save_recipe, BrowserError, BrowserRecipeInputKind,
    BrowserRecipeLocator, BrowserRecordingAction, BrowserRecordingActor, BrowserRecordingOperation,
    BrowserRecordingStatus, BrowserResourceKind, BrowserResourceLimits, BrowserResourceStore,
    BrowserRisk, BrowserWorkflowCoordinator, BrowserWorkspaceKey,
};
use serde_json::Value;

fn workspace(project: &str, conversation: &str) -> BrowserWorkspaceKey {
    BrowserWorkspaceKey::new(project, conversation).expect("valid workspace")
}

fn locator(test_id: &str) -> BrowserRecipeLocator {
    BrowserRecipeLocator {
        test_id: Some(test_id.to_string()),
        ..BrowserRecipeLocator::default()
    }
}

fn temporary_root(label: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "devmanager-{label}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ))
}

fn reviewed_navigation(
    coordinator: &BrowserWorkflowCoordinator,
    owner: &BrowserWorkspaceKey,
    url: &str,
) -> u64 {
    let instance = coordinator.start(owner.clone()).expect("start recording");
    let reservation = coordinator
        .reserve_on(
            &instance,
            BrowserRecordingActor::Agent,
            "tab-a",
            BrowserRisk::Normal,
        )
        .unwrap();
    coordinator
        .commit(
            reservation,
            BrowserRecordingAction::navigate(url).expect("safe navigation action"),
        )
        .unwrap();
    coordinator.stop(&instance).expect("stop exact recording");
    instance.id()
}

#[test]
fn recording_status_and_review_resource_use_one_exact_coordinator_and_value_safe_wire() {
    let root = temporary_root("recording-mcp-resource");
    let store = BrowserResourceStore::open(&root, BrowserResourceLimits::default()).unwrap();
    let owner = workspace("project-a", "conversation-a");
    let other = workspace("project-a", "conversation-b");
    let coordinator = BrowserWorkflowCoordinator::default();

    let inactive =
        browser_recording_status_result(&coordinator, &owner, BrowserRecordingOperation::Status);
    assert_eq!(inactive.status, BrowserRecordingStatus::Inactive);
    assert_eq!(inactive.recording_id, None);
    assert!(inactive.resource.is_none());

    let instance = coordinator.start(owner.clone()).expect("start recording");
    let secret = coordinator
        .reserve_on(
            &instance,
            BrowserRecordingActor::User,
            "tab-a",
            BrowserRisk::AccountSecurity,
        )
        .unwrap();
    coordinator
        .commit(
            secret,
            BrowserRecordingAction::type_text(
                locator("password"),
                "authorization=Bearer recording-resource-secret-sentinel",
            )
            .unwrap(),
        )
        .unwrap();
    let file = coordinator
        .reserve_on(
            &instance,
            BrowserRecordingActor::Agent,
            "tab-a",
            BrowserRisk::Normal,
        )
        .unwrap();
    coordinator
        .commit(
            file,
            BrowserRecordingAction::upload(locator("receipt")).unwrap(),
        )
        .unwrap();
    coordinator.stop(&instance).expect("stop exact recording");

    let wrong_route =
        browser_recording_status_result(&coordinator, &other, BrowserRecordingOperation::Status);
    assert_eq!(wrong_route.status, BrowserRecordingStatus::Inactive);
    assert_eq!(wrong_route.recording_id, None);

    let review = browser_recording_review_result(
        &coordinator,
        &owner,
        BrowserRecordingOperation::Review,
        &store,
    )
    .expect("create exact review resource");
    assert_eq!(review.status, BrowserRecordingStatus::Review);
    assert_eq!(review.recording_id, Some(instance.id()));
    assert_eq!(review.step_count, 2);
    assert!(review.valid);
    assert_eq!(
        review
            .inputs
            .iter()
            .map(|input| (input.name.as_str(), input.kind))
            .collect::<Vec<_>>(),
        vec![
            ("secret", BrowserRecipeInputKind::Secret),
            ("file", BrowserRecipeInputKind::File),
        ]
    );
    let handle = review.resource.expect("review resource handle");
    assert_eq!(handle.kind, BrowserResourceKind::WorkflowReview);
    assert!(
        !handle.pinned,
        "review resources must use bounded temporary cleanup"
    );
    let resource = store.read(&owner, &handle.id).expect("read owned review");
    assert!(store.read(&other, &handle.id).is_err());
    let document: Value = serde_json::from_slice(&resource.bytes).expect("review JSON");
    assert!(
        !String::from_utf8_lossy(&resource.bytes).contains("recording-resource-secret-sentinel")
    );
    assert_eq!(document["version"], 1);
    assert_eq!(document["recordingId"], instance.id());
    let inputs = document["recipe"]["inputs"].as_array().unwrap();
    for input in inputs {
        if matches!(input["kind"].as_str(), Some("secret" | "file")) {
            let object = input.as_object().unwrap();
            assert_eq!(object.len(), 2);
            assert!(object.contains_key("name"));
            assert!(object.contains_key("kind"));
            assert!(!object.contains_key("defaultValue"));
            assert!(!object.contains_key("path"));
            assert!(!object.contains_key("value"));
        }
    }

    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn recording_save_escalates_overwrite_and_only_retires_after_atomic_success() {
    let root = temporary_root("recording-mcp-save");
    std::fs::create_dir_all(&root).unwrap();
    let root = root.canonicalize().unwrap();
    let owner = workspace("project-a", "conversation-a");
    let coordinator = BrowserWorkflowCoordinator::default();

    let first_id = reviewed_navigation(&coordinator, &owner, "https://example.test/first");
    assert!(
        !browser_recording_save_would_overwrite(&coordinator, &owner, first_id, &root).unwrap()
    );
    assert_eq!(
        effective_browser_recording_risk(
            BrowserRisk::Normal,
            BrowserRecordingOperation::Save,
            false,
        ),
        BrowserRisk::Normal,
    );
    let first = save_browser_recording_review(&coordinator, &owner, first_id, &root, false)
        .expect("new recipe save");
    assert_eq!(first.status, BrowserRecordingStatus::Inactive);
    assert_eq!(first.operation, BrowserRecordingOperation::Save);
    assert_eq!(first.overwrote_existing, Some(false));
    assert_eq!(
        browser_recording_status_result(&coordinator, &owner, BrowserRecordingOperation::Status,)
            .status,
        BrowserRecordingStatus::Inactive,
    );

    let saved = load_recipe(&root, first.recipe_id.as_deref().unwrap()).expect("saved recipe");
    assert_eq!(saved.steps.len(), 1);
    assert_eq!(first.recording_id, Some(first_id));

    let second_id = reviewed_navigation(&coordinator, &owner, "https://example.test/second");
    save_browser_recording_review(&coordinator, &owner, first_id, &root, true)
        .expect_err("stale save must not mutate a replacement review");
    assert_eq!(
        coordinator.current_instance(&owner).unwrap().id(),
        second_id,
        "replacement review survives a stale approval resume",
    );
    let second_recipe_id =
        browser_recording_status_result(&coordinator, &owner, BrowserRecordingOperation::Status)
            .recipe_id
            .expect("second recipe id");
    let mut existing = saved.clone();
    existing.id = second_recipe_id;
    save_recipe(&root, &existing).expect("prepare an existing valid recipe");
    assert!(
        browser_recording_save_would_overwrite(&coordinator, &owner, second_id, &root).unwrap()
    );
    assert_eq!(
        effective_browser_recording_risk(
            BrowserRisk::Normal,
            BrowserRecordingOperation::Save,
            true,
        ),
        BrowserRisk::Destructive,
    );
    let refused = save_browser_recording_review(&coordinator, &owner, second_id, &root, false)
        .expect_err("normal-risk save must not overwrite");
    assert!(!refused
        .to_string()
        .contains(root.to_string_lossy().as_ref()));
    assert_eq!(
        browser_recording_status_result(&coordinator, &owner, BrowserRecordingOperation::Status,)
            .status,
        BrowserRecordingStatus::Review,
        "failed save must retain the exact review",
    );
    let overwritten = save_browser_recording_review(&coordinator, &owner, second_id, &root, true)
        .expect("destructive-approved overwrite");
    assert_eq!(overwritten.recording_id, Some(second_id));
    assert_eq!(overwritten.overwrote_existing, Some(true));
    assert_eq!(
        browser_recording_status_result(&coordinator, &owner, BrowserRecordingOperation::Status,)
            .status,
        BrowserRecordingStatus::Inactive,
    );

    let third_id = reviewed_navigation(&coordinator, &owner, "https://example.test/third");
    let unavailable = root.join("missing-project-root");
    let error = save_browser_recording_review(&coordinator, &owner, third_id, &unavailable, true)
        .expect_err("unavailable authenticated root");
    assert!(!error.to_string().contains(root.to_string_lossy().as_ref()));
    assert_eq!(
        browser_recording_status_result(&coordinator, &owner, BrowserRecordingOperation::Status,)
            .recording_id,
        Some(third_id),
        "storage failure must retain the exact review",
    );

    let race_root = temporary_root("recording-mcp-race");
    std::fs::create_dir_all(&race_root).unwrap();
    let race_root = race_root.canonicalize().unwrap();
    assert!(
        !browser_recording_save_would_overwrite(&coordinator, &owner, third_id, &race_root)
            .unwrap()
    );
    let third_recipe_id =
        browser_recording_status_result(&coordinator, &owner, BrowserRecordingOperation::Status)
            .recipe_id
            .expect("third recipe id");
    let mut raced_destination = saved.clone();
    raced_destination.id = third_recipe_id;
    save_recipe(&race_root, &raced_destination).expect("destination appears after risk probe");
    save_browser_recording_review(&coordinator, &owner, third_id, &race_root, false)
        .expect_err("new-file save must fail closed when destination appears");
    assert_eq!(coordinator.status(&owner), BrowserRecordingStatus::Review);

    let discarded =
        discard_browser_recording(&coordinator, &owner, third_id).expect("discard exact review");
    assert_eq!(discarded.operation, BrowserRecordingOperation::Discard);
    assert_eq!(discarded.status, BrowserRecordingStatus::Inactive);
    assert_eq!(discarded.recording_id, Some(third_id));
    assert_eq!(
        effective_browser_recording_risk(
            BrowserRisk::Normal,
            BrowserRecordingOperation::Discard,
            false,
        ),
        BrowserRisk::Destructive,
    );
    assert_eq!(coordinator.status(&owner), BrowserRecordingStatus::Inactive);

    std::fs::remove_dir_all(root).unwrap();
    std::fs::remove_dir_all(race_root).unwrap();
}

#[test]
fn recording_review_resource_failure_is_fixed_path_free_and_retains_review() {
    let root = temporary_root("recording-resource-path-sentinel");
    let store = BrowserResourceStore::open(&root, BrowserResourceLimits::default()).unwrap();
    let owner = workspace("project-a", "conversation-a");
    let coordinator = BrowserWorkflowCoordinator::default();
    let instance_id = reviewed_navigation(&coordinator, &owner, "https://example.test/review");

    std::fs::remove_dir_all(store.root()).unwrap();
    std::fs::write(store.root(), b"force resource persistence failure").unwrap();
    let error = browser_recording_review_result(
        &coordinator,
        &owner,
        BrowserRecordingOperation::Review,
        &store,
    )
    .expect_err("resource persistence must fail");

    assert_eq!(error, BrowserError::RecordingResourceUnavailable);
    assert_eq!(
        error.to_string(),
        "browser recording review resource is unavailable"
    );
    assert!(!error
        .to_string()
        .contains("recording-resource-path-sentinel"));
    assert_eq!(
        coordinator.current_instance(&owner).unwrap().id(),
        instance_id,
        "resource failure must retain the exact Review state",
    );
    assert_eq!(coordinator.status(&owner), BrowserRecordingStatus::Review);

    std::fs::remove_file(root).unwrap();
}
