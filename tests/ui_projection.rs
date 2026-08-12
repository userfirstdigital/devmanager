use devmanager::ui::preview::{
    parse_preview_args, PreviewApplication, PreviewDismiss, PreviewError, PreviewOutputCapability,
    PreviewPathPolicy, PreviewRequest, PREVIEW_SCHEMA,
};
use devmanager::ui::{self, PreviewInitReport};
use std::cell::Cell;
use std::fs;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Mutex;
use tempfile::{tempdir, TempDir};

const FIXTURE_JSON: &str = r#"{
  "schema": "devmanager.ui.preview/v1",
  "id": "theme-gallery",
  "title": "Theme Gallery",
  "capture": {
    "cursor": "excluded",
    "border": "excluded"
  },
  "root": {
    "kind": "minimal",
    "label": "DevManager native preview"
  }
}"#;

static HEADLESS_INIT_TEST_LOCK: Mutex<()> = Mutex::new(());

fn repository_policy() -> PreviewPathPolicy {
    PreviewPathPolicy::for_workspace(env!("CARGO_MANIFEST_DIR"))
}

fn repository_fixture() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/ui/theme-gallery.json")
}

fn repository_output(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join(".devmanager-next/evidence/phase-05/screenshots")
        .join(name)
}

fn temporary_policy() -> (TempDir, PreviewPathPolicy) {
    let root = tempdir().expect("temporary preview root");
    let fixture_root = root.path().join("fixtures/ui");
    let output_root = root.path().join("evidence/screenshots");
    fs::create_dir_all(&fixture_root).expect("fixture root");
    fs::create_dir_all(&output_root).expect("output root");
    let policy = PreviewPathPolicy::new(&fixture_root, &output_root, root.path().join("temp"));
    (root, policy)
}

fn write_fixture(policy: &PreviewPathPolicy, name: &str, contents: &str) -> PathBuf {
    let path = policy.fixture_root().join(name);
    fs::write(&path, contents).expect("fixture contents");
    path
}

fn valid_request(policy: &PreviewPathPolicy) -> PreviewRequest {
    let fixture = write_fixture(policy, "valid.json", FIXTURE_JSON);
    let output = policy.output_root().join("preview.png");
    PreviewRequest::validate(fixture, output, policy).expect("valid preview request")
}

#[test]
fn component_init_registers_devmanager_resources_once() {
    let _lock = HEADLESS_INIT_TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let policy = repository_policy();
    let request = PreviewRequest::validate(
        repository_fixture(),
        repository_output("component-init.png"),
        &policy,
    )
    .expect("checked-in fixture should be accepted");
    let preview = PreviewApplication::load(request, &policy).expect("fixture should load");

    let before = ui::component_init_count();
    let report: PreviewInitReport = preview
        .initialize_headless()
        .expect("headless initialization should be isolated");
    let repeated = preview
        .initialize_headless()
        .expect("repeated initialization should reuse the first report");

    assert_eq!(report, repeated);
    assert_eq!(report.component_init_count, before + 1);
    assert!(report.assets_registered);
    assert!(report.fonts_registered);
    assert!(report.actions_registered);
    assert!(report.root_constructed);
    assert!(!report.production_host_started);
    assert_eq!(ui::component_init_count(), report.component_init_count);
}

#[test]
fn preview_output_metadata_is_deterministic_and_does_not_claim_write() {
    let (_root, policy) = temporary_policy();
    let request = valid_request(&policy);
    let output = request.output_path().to_path_buf();
    let preview = PreviewApplication::load(request, &policy).expect("fixture should load");

    let first = preview.output_metadata();
    let second = preview.output_metadata();

    assert_eq!(first, second);
    assert_eq!(first.schema, PREVIEW_SCHEMA);
    assert_eq!(first.fixture_id, "theme-gallery");
    assert_eq!(first.output_path, output);
    assert_eq!(first.format, "png");
    assert_eq!(
        first.capability,
        if cfg!(windows) {
            PreviewOutputCapability::VisibleWindowsNativeCapture
        } else {
            PreviewOutputCapability::HeadlessProjectionOnly
        }
    );
    assert!(!first.output_written);
    assert!(!first.host_started);
    assert!(!output.exists());
}

#[test]
fn preview_fixture_rejects_whitespace_only_required_fields() {
    let (_root, policy) = temporary_policy();
    let fixture = write_fixture(
        &policy,
        "whitespace.json",
        &FIXTURE_JSON
            .replace("theme-gallery", "   ")
            .replace("Theme Gallery", "  ")
            .replace("DevManager native preview", " "),
    );
    let request = PreviewRequest::validate(
        fixture,
        policy.output_root().join("whitespace.png"),
        &policy,
    )
    .expect("path validation should precede fixture parsing");

    let error = PreviewApplication::load(request, &policy)
        .expect_err("whitespace-only fixture fields must be rejected");
    assert!(matches!(error, PreviewError::MalformedFixture { .. }));
}

#[test]
fn preview_root_projection_is_deterministic() {
    let (_root, policy) = temporary_policy();
    let first = PreviewApplication::load(valid_request(&policy), &policy).expect("first fixture");

    let fixture = write_fixture(&policy, "valid-again.json", FIXTURE_JSON);
    let second_request = PreviewRequest::validate(
        fixture,
        policy.output_root().join("preview-again.png"),
        &policy,
    )
    .expect("second request");
    let second = PreviewApplication::load(second_request, &policy).expect("second fixture");

    assert_eq!(first.root_snapshot(), second.root_snapshot());
    assert_eq!(first.resources(), second.resources());
    assert_eq!(
        first.root_snapshot().body,
        "DevManager native preview: Theme Gallery"
    );
}

#[test]
fn preview_validation_accepts_only_checked_fixture_and_png_roots() {
    let (_root, policy) = temporary_policy();
    let request = valid_request(&policy);

    assert_eq!(
        request.fixture_path(),
        policy.fixture_root().join("valid.json")
    );
    assert_eq!(
        request.output_path(),
        policy.output_root().join("preview.png")
    );
}

#[test]
fn preview_validation_rejects_no_args() {
    let policy = repository_policy();
    let error = parse_preview_args(Vec::<String>::new(), &policy).expect_err("no args must fail");

    assert!(matches!(error, PreviewError::Usage(_)));
}

#[test]
fn preview_validation_rejects_traversal_and_outside_fixture() {
    let (root, policy) = temporary_policy();
    let outside_fixture = root.path().join("outside.json");
    fs::write(&outside_fixture, FIXTURE_JSON).expect("outside fixture");
    let output = policy.output_root().join("outside.png");

    let error = PreviewRequest::validate(outside_fixture, output, &policy)
        .expect_err("outside fixture must fail");
    assert!(matches!(error, PreviewError::OutsideApprovedRoot { .. }));

    let fixture = write_fixture(&policy, "traversal.json", FIXTURE_JSON);
    let traversal = policy.output_root().join("..").join("escaped.png");
    let error = PreviewRequest::validate(fixture, traversal, &policy)
        .expect_err("traversal output must fail");
    assert!(matches!(error, PreviewError::OutsideApprovedRoot { .. }));
}

#[test]
fn preview_validation_rejects_missing_oversized_and_malformed_fixtures() {
    let (_root, policy) = temporary_policy();
    let missing = policy.fixture_root().join("missing.json");
    let missing_error =
        PreviewRequest::validate(&missing, policy.output_root().join("missing.png"), &policy)
            .expect_err("missing fixture must fail");
    assert!(matches!(missing_error, PreviewError::FixtureMissing { .. }));

    let malformed = write_fixture(&policy, "malformed.json", "{ not json");
    let malformed_request = PreviewRequest::validate(
        malformed,
        policy.output_root().join("malformed.png"),
        &policy,
    )
    .expect("path validation should precede parsing");
    let malformed_error = PreviewApplication::load(malformed_request, &policy)
        .expect_err("malformed fixture must fail");
    assert!(matches!(
        malformed_error,
        PreviewError::MalformedFixture { .. }
    ));

    let oversized = write_fixture(
        &policy,
        "oversized.json",
        &format!("{}{}", FIXTURE_JSON, "x".repeat(300_000)),
    );
    let oversized_error = PreviewRequest::validate(
        oversized,
        policy.output_root().join("oversized.png"),
        &policy,
    )
    .expect_err("oversized fixture must fail");
    assert!(matches!(
        oversized_error,
        PreviewError::FixtureTooLarge { .. }
    ));
}

#[test]
fn preview_validation_rejects_existing_sensitive_output() {
    let (_root, policy) = temporary_policy();
    let fixture = write_fixture(&policy, "existing.json", FIXTURE_JSON);
    let output = policy.output_root().join("existing.png");
    fs::write(&output, b"do not overwrite").expect("existing output");

    let error = PreviewRequest::validate(fixture, output, &policy)
        .expect_err("existing output must not be overwritten");
    assert!(matches!(error, PreviewError::OutputAlreadyExists { .. }));

    let production_fixture = write_fixture(&policy, "production.json", FIXTURE_JSON);
    let production = Path::new(r"C:\Users\micro\AppData\Roaming\DevManager\config.json");
    let error = PreviewRequest::validate(production_fixture, production, &policy)
        .expect_err("production config path must fail");
    assert!(matches!(error, PreviewError::SensitivePath { .. }));

    let sensitive_fixture = write_fixture(&policy, "config.json", FIXTURE_JSON);
    let error = PreviewRequest::validate(
        sensitive_fixture,
        policy.output_root().join("fixture.png"),
        &policy,
    )
    .expect_err("sensitive fixture path must fail");
    assert!(matches!(error, PreviewError::SensitivePath { .. }));
}

#[cfg(not(windows))]
#[test]
fn preview_execution_returns_explicit_visible_windows_unavailable_error_without_writing_output() {
    let (_root, policy) = temporary_policy();
    let request = valid_request(&policy);
    let output = request.output_path().to_path_buf();
    let preview = PreviewApplication::load(request, &policy).expect("fixture should load");

    let error = preview
        .render_to_output()
        .expect_err("non-Windows visual capture must be unavailable");
    assert!(matches!(
        error,
        PreviewError::VisibleWindowsCaptureUnavailable { .. }
    ));
    assert!(!output.exists());
}

#[test]
fn preview_request_construction_rejects_production_paths_before_load() {
    let (root, policy) = temporary_policy();
    let production_root = root.path().join("production");
    fs::create_dir_all(&production_root).expect("production fixture directory");
    let production_fixture = production_root.join("session.json");
    fs::write(&production_fixture, FIXTURE_JSON).expect("production fixture");

    let error = PreviewRequest::validate(
        production_fixture,
        policy.output_root().join("bypass.png"),
        &policy,
    )
    .expect_err("PreviewRequest construction must not bypass isolation");
    assert!(matches!(error, PreviewError::OutsideApprovedRoot { .. }));
}

#[test]
fn task_cockpit_actions_are_registered_and_dispatch_through_gpui() {
    let dispatched = Rc::new(Cell::new(false));
    let dispatched_in_app = Rc::clone(&dispatched);
    let application = gpui::Application::headless();

    application.run(move |cx| {
        let expected = [
            "host.actions",
            "host.status",
            "task.list",
            "task.show",
            "task.create",
            "task.rename",
        ];
        for action_name in expected {
            assert!(
                cx.all_action_names().contains(&action_name),
                "{action_name} must be an actual GPUI action registration"
            );
            let action = cx
                .build_action(action_name, None)
                .expect("registered actions must be dynamically buildable");
            cx.dispatch_action(action.as_ref());
        }

        cx.on_action::<PreviewDismiss>(move |_, _| dispatched_in_app.set(true));
        cx.dispatch_action(&PreviewDismiss);
        assert!(
            dispatched.get(),
            "GPUI must dispatch the retained PreviewDismiss action"
        );
        cx.quit();
    });
}
