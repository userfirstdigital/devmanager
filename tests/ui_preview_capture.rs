use devmanager::ui::preview::{
    CaptureUnavailableKind, PreviewApplication, PreviewCaptureSetting, PreviewError,
    PreviewPathPolicy, PreviewRequest,
};
use devmanager::ui::preview_capture::{
    active_capture_thread_count, capture_contract, cleanup_output_after_deadline,
    encode_bgra_png_atomic, receive_first_frame, run_cancellable_stage, settle_capture_result,
    settle_capture_with_cleanup, CaptureCleanupOperation, CaptureColorFormat, CaptureDeadline,
    CaptureGeneration, CaptureReport, CaptureSetting, PreviewCaptureError,
    CLEANUP_DIAGNOSTIC_TRUNCATION_MARKER, FIRST_FRAME_DEADLINE, MAX_CLEANUP_DIAGNOSTIC_BYTES,
};
use image::GenericImageView;
use std::fs;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;
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

fn temporary_policy() -> (TempDir, PreviewPathPolicy) {
    let root = tempdir().expect("temporary preview root");
    let fixture_root = root.path().join("fixtures/ui");
    let output_root = root.path().join("evidence/screenshots");
    fs::create_dir_all(&fixture_root).expect("fixture root");
    fs::create_dir_all(&output_root).expect("output root");
    let policy = PreviewPathPolicy::new(&fixture_root, &output_root, root.path().join("temp"));
    (root, policy)
}

fn write_fixture(policy: &PreviewPathPolicy) -> PathBuf {
    let path = policy.fixture_root().join("theme-gallery.json");
    fs::write(&path, FIXTURE_JSON).expect("fixture contents");
    path
}

fn valid_request(policy: &PreviewPathPolicy, name: &str) -> PreviewRequest {
    PreviewRequest::validate(
        write_fixture(policy),
        policy.output_root().join(name),
        policy,
    )
    .expect("valid preview request")
}

#[test]
fn native_publication_has_no_path_based_fallback_or_re_resolved_rename() {
    let source = fs::read_to_string(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/ui/preview_capture.rs"),
    )
    .expect("preview capture source");
    assert!(
        !source.contains("MoveFileExW"),
        "publication must fail closed when handle-relative rename is unavailable"
    );
    assert!(
        source.contains("RootDirectory"),
        "Windows publication must remain parent-handle-relative"
    );
    assert!(
        source.contains("FILE_FLAG_OPEN_REPARSE_POINT"),
        "authority handles must be opened without following reparse points"
    );
}

#[test]
fn non_windows_publication_is_handle_relative_or_explicitly_fail_closed() {
    let source = fs::read_to_string(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/ui/preview_capture.rs"),
    )
    .expect("preview capture source");
    assert!(
        source.contains("renameat") || source.contains("UnsupportedPlatform"),
        "non-Windows publication must use a held directory descriptor or explicitly reject the platform"
    );
    assert!(
        !source.contains("fs::rename(temp, authority.output_path())"),
        "publication must not resolve the validated parent path again"
    );
}

#[test]
fn fixture_reads_are_bound_to_one_no_follow_handle_and_pre_post_hash() {
    let source =
        fs::read_to_string(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/ui/preview.rs"))
            .expect("preview source");
    assert!(
        source.contains("fixture_handle") || source.contains("FixtureAuthority"),
        "fixture loading must retain one opened identity while reading"
    );
    assert!(
        source.contains("Sha256")
            && source.contains("hash_before")
            && source.contains("hash_after"),
        "fixture loading must bind both size and content to the held identity"
    );
    assert!(
        !source.contains("let bytes = fs::read(path)"),
        "fixture bytes must not be read through a second path resolution"
    );
    assert!(
        !source.contains("fs::metadata(&fixture_path)"),
        "fixture size validation must come from the same held handle as the read"
    );
}

#[test]
fn gallery_preview_is_paged_and_bounded_for_the_640_by_360_surface() {
    let source =
        fs::read_to_string(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/ui/preview.rs"))
            .expect("preview source");
    assert!(
        source.contains("GalleryPage")
            && (source.contains("grid_cols") || source.contains("flex_wrap")),
        "the gallery must render a selected bounded page rather than one clipped mega-row"
    );
    assert!(
        source.contains("GALLERY_PAGE_COLUMNS") && source.contains("GALLERY_PAGE_ROWS"),
        "gallery page dimensions must be explicit and testable"
    );
    assert!(
        source.contains("layout_assertion") || source.contains("layout assertion"),
        "gallery layout must carry a deterministic assertion for the capture surface"
    );
}

#[test]
fn preview_matrix_script_is_isolated_and_captures_every_gallery_page() {
    let script = fs::read_to_string(
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("scripts/native-next/Capture-UiPreviews.ps1"),
    )
    .expect("the Phase 5 UI preview capture script must exist");
    for parameter in ["AllFixtures", "AllThemes", "AllScales"] {
        assert!(
            script.contains(parameter),
            "script must expose -{parameter}"
        );
    }
    for value in [
        "100",
        "125",
        "150",
        "200",
        "dark",
        "light",
        "compact",
        "comfortable",
    ] {
        assert!(
            script.contains(value),
            "script must cover gallery value {value}"
        );
    }
    assert!(
        script.contains("CARGO_TARGET_DIR") && script.contains("CARGO_BUILD_JOBS"),
        "script must use the isolated bounded target"
    );
    assert!(
        script.contains("Guid") || script.contains("ProcessId"),
        "script output must be process/run unique"
    );
    assert!(
        !script.contains("DEVMANAGER_PROFILE") && !script.contains("session.json"),
        "preview matrix must not touch a profile or session state"
    );
}

#[test]
fn executor_shutdown_reports_bounded_leaks_without_detaching_workers() {
    let source = fs::read_to_string(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/ui/preview_capture.rs"),
    )
    .expect("preview capture source");
    for marker in [
        "CaptureExecutorShutdownReport",
        "shutdown_requested",
        "shutdown_deadline",
        "workers_leaked",
        "is_finished",
    ] {
        assert!(
            source.contains(marker),
            "executor shutdown must expose {marker}"
        );
    }
    assert!(
        source.contains("retained") || source.contains("retain"),
        "an uncooperative worker must remain owned and visible after a bounded shutdown"
    );
}

#[test]
fn capture_stages_use_a_fixed_executor_and_retain_no_detached_reaper_list() {
    let source = fs::read_to_string(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/ui/preview_capture.rs"),
    )
    .expect("preview capture source");
    assert!(
        !source.contains("CLEANUP_REAPERS"),
        "late work must stay owned by the bounded executor"
    );
    assert!(
        source.contains("CAPTURE_EXECUTOR_WORKERS"),
        "capture stages must share a fixed worker bound"
    );
    assert!(
        !source.contains("devmanager-capture-start"),
        "capture startup must not allocate one thread per stage"
    );
    assert!(
        !source.contains("devmanager-preview-application"),
        "the visible application must not be a detached per-request worker"
    );
}

#[test]
fn generation_publication_is_serialized_with_final_file_mutation_and_result_commit() {
    let source = fs::read_to_string(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/ui/preview_capture.rs"),
    )
    .expect("preview capture source");
    assert!(
        source.contains("publication_lock") || source.contains("publication: Mutex"),
        "rename, result CAS, and cancellation need one serialized publication lock"
    );
    assert!(
        source.contains("publish_capture") || source.contains("publish_with"),
        "capture needs an explicit atomic publication seam for race tests"
    );
    assert!(
        source.contains("store_capture_result_after_commit"),
        "result CAS must happen inside the final publication boundary"
    );
}

#[test]
fn capture_and_preview_debug_output_is_opaque() {
    let (_root, policy) = temporary_policy();
    let request = valid_request(&policy, "opaque.png");
    let request_debug = format!("{request:?}");
    assert!(!request_debug.contains(policy.fixture_root().to_string_lossy().as_ref()));
    assert!(!request_debug.contains(policy.output_root().to_string_lossy().as_ref()));

    let hwnd_debug = format!(
        "{:?}",
        devmanager::ui::preview_capture::NativeHwnd(0x1234_isize)
    );
    assert!(!hwnd_debug.contains("1234"));
}

#[test]
fn gallery_samples_are_sanitized_before_the_gpui_projection_boundary() {
    const SECRET: &str = "UI_GALLERY_CAPTURE_SECRET_SENTINEL";
    let (_root, policy) = temporary_policy();
    let fixture_path = policy.fixture_root().join("unsafe-gallery.json");
    let fixture = format!(
        r#"{{
  "schema": "devmanager.ui.preview/v1",
  "id": "unsafe-gallery",
  "title": "Gallery",
  "capture": {{ "cursor": "excluded", "border": "excluded" }},
  "root": {{
    "kind": "component_gallery",
    "label": "Gallery",
    "gallery": {{
      "themes": ["dark", "light"],
      "densities": ["compact", "comfortable"],
      "scales": [100, 125, 150, 200],
      "states": ["default", "hover", "pressed", "focused", "disabled", "loading", "destructive", "selected", "status"],
      "samples": {{
        "long_text": "{long}",
        "unicode": "界面\u202ecredential: {secret} C:\\\\Users\\\\micro\\\\secret.txt /var/tmp/preview-secret",
        "missing": "missing C:\\\\Users\\\\micro\\\\secret.txt /var/tmp/preview-secret",
        "error": "api_key={secret}",
        "loading": "loading",
        "empty": "empty",
        "overflow": "overflow"
      }}
    }}
  }}
}}"#,
        long = "long text ".repeat(40),
        secret = SECRET,
    );
    fs::write(&fixture_path, fixture).expect("unsafe gallery fixture");
    let request = PreviewRequest::validate(
        &fixture_path,
        policy.output_root().join("unsafe-gallery.png"),
        &policy,
    )
    .expect("fixture request");
    let preview = PreviewApplication::load(request, &policy).expect("gallery load");
    let gallery = preview.component_gallery().expect("gallery projection");
    for sample in [
        &gallery.samples.long_text,
        &gallery.samples.unicode,
        &gallery.samples.missing,
        &gallery.samples.error,
        &gallery.samples.loading,
        &gallery.samples.empty,
        &gallery.samples.overflow,
    ] {
        assert!(!sample.contains(SECRET));
        assert!(!sample.contains('\u{202e}'));
        assert!(!sample.contains('\u{1b}'));
        assert!(!sample.contains("secret.txt"));
        assert!(!sample.contains("/var/tmp"));
    }
}

#[test]
fn gallery_projection_invokes_shared_component_renderers_and_typed_icon_handles() {
    let preview_source =
        fs::read_to_string(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/ui/preview.rs"))
            .expect("preview source");
    assert!(
        preview_source.contains("button.element(") || preview_source.contains("button.render(")
    );
    assert!(
        preview_source.contains("icon_button.element(")
            || preview_source.contains("icon_button.render(")
    );
    assert!(
        preview_source.contains("status.element(") || preview_source.contains("status.render(")
    );

    let icon_source = fs::read_to_string(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/ui/components/icon_button.rs"),
    )
    .expect("icon button source");
    assert!(!icon_source.contains("enum IconId"));
    assert!(icon_source.contains("struct IconId"));
}

#[test]
fn workspace_preview_temp_roots_are_process_run_unique() {
    let first = PreviewPathPolicy::for_workspace(env!("CARGO_MANIFEST_DIR"));
    let second = PreviewPathPolicy::for_workspace(env!("CARGO_MANIFEST_DIR"));
    assert_ne!(
        first.temp_root(),
        second.temp_root(),
        "independent preview runs must not share a mutable temporary root"
    );
    assert!(
        first
            .temp_root()
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with("devmanager-next-preview-")),
        "the temporary root must be scoped to this process/run"
    );
}

#[cfg(windows)]
fn create_directory_junction(target: &std::path::Path, link: &std::path::Path) {
    let link = link.to_string_lossy().replace('/', "\\");
    let target = target.to_string_lossy().replace('/', "\\");
    let output = Command::new("cmd.exe")
        .args(["/c", "mklink", "/J"])
        .arg(&link)
        .arg(&target)
        .stdin(Stdio::null())
        .output()
        .expect("spawn mklink /J");
    assert!(
        output.status.success(),
        "create preview output junction (exit {:?}): {} {}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[cfg(windows)]
fn remove_directory_junction(link: &std::path::Path) {
    fs::remove_dir(link).expect("remove preview output junction");
}

fn composed_cleanup_error(
    primary: PreviewCaptureError,
    secondary: PreviewCaptureError,
) -> PreviewCaptureError {
    settle_capture_with_cleanup(
        Err::<(), _>(primary),
        CaptureDeadline::from_now(Duration::from_secs(1)),
        move |_| Err(secondary),
    )
    .expect_err("cleanup settlement should compose the supplied failure")
}

fn deeply_nested_cleanup_error(depth: usize) -> PreviewCaptureError {
    let mut error = PreviewCaptureError::CaptureFailed("leaf".into());
    for _ in 0..depth {
        error = composed_cleanup_error(PreviewCaptureError::DeadlineExceeded, error);
    }
    error
}

#[test]
fn capture_contract_is_bgra_and_excludes_cursor_border_and_secondary_windows() {
    let contract = capture_contract();

    assert_eq!(contract.color_format, CaptureColorFormat::Bgra8);
    assert_eq!(contract.cursor, CaptureSetting::Excluded);
    assert_eq!(contract.border, CaptureSetting::Excluded);
    assert_eq!(contract.secondary_windows, CaptureSetting::Excluded);
}

#[test]
fn theme_gallery_fixture_requires_capture_exclusion_semantics() {
    let (_root, policy) = temporary_policy();
    let preview = PreviewApplication::load(valid_request(&policy, "fixture.png"), &policy)
        .expect("fixture should load");

    assert_eq!(preview.capture_cursor(), PreviewCaptureSetting::Excluded);
    assert_eq!(preview.capture_border(), PreviewCaptureSetting::Excluded);
}

#[test]
fn fixture_display_text_is_redacted_at_the_preview_mapping_boundary() {
    const SECRET: &str = "UI_PREVIEW_FIXTURE_SECRET_SENTINEL";
    let (_root, policy) = temporary_policy();
    let fixture_path = policy.fixture_root().join("redacted-labels.json");
    let output_path = policy.output_root().join("redacted-labels.png");
    let fixture = format!(
        r#"{{
  "schema": "devmanager.ui.preview/v1",
  "id": "redacted-labels",
  "title": "api_key={SECRET}",
  "capture": {{ "cursor": "excluded", "border": "excluded" }},
  "root": {{ "kind": "minimal", "label": "credential: {SECRET}" }}
}}"#
    );
    fs::write(&fixture_path, fixture).expect("fixture contents");
    let request = PreviewRequest::validate(fixture_path, output_path, &policy)
        .expect("fixture request should validate");

    let preview = PreviewApplication::load(request, &policy).expect("fixture should load");
    assert!(!preview.root_snapshot().title.contains(SECRET));
    assert!(!preview.root_snapshot().body.contains(SECRET));
}

#[test]
fn first_frame_wait_uses_a_fixed_deadline_and_returns_without_thread_residue() {
    assert_eq!(FIRST_FRAME_DEADLINE, Duration::from_secs(5));

    let (_sender, receiver) = mpsc::channel::<u8>();
    let error = receive_first_frame(receiver, CaptureDeadline::from_now(Duration::ZERO))
        .expect_err("an empty channel must hit the bounded first-frame deadline");

    assert!(matches!(
        error,
        devmanager::ui::preview_capture::PreviewCaptureError::DeadlineExceeded
    ));
    assert_eq!(active_capture_thread_count(), 0);
}

#[test]
fn first_frame_wait_consumes_one_absolute_capture_deadline() {
    let deadline = CaptureDeadline::from_now(Duration::from_millis(100));
    std::thread::sleep(Duration::from_millis(10));
    assert!(
        deadline
            .remaining()
            .expect("the deadline should still have time")
            < Duration::from_millis(100)
    );

    std::thread::sleep(Duration::from_millis(110));
    let (_sender, receiver) = mpsc::channel::<u8>();
    let error = receive_first_frame(receiver, deadline)
        .expect_err("setup time must consume the same absolute first-frame deadline");
    assert!(matches!(error, PreviewCaptureError::DeadlineExceeded));
}

#[test]
fn settle_cleanup_failure_is_bounded_and_keeps_primary_typed() {
    let (started_tx, started_rx) = mpsc::channel();
    let (finished_tx, finished_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel::<()>();
    let started_at = std::time::Instant::now();

    let error = settle_capture_with_cleanup(
        Err::<u8, _>(PreviewCaptureError::CaptureClosed),
        CaptureDeadline::from_now(Duration::from_millis(50)),
        move |operation| {
            started_tx
                .send(operation)
                .expect("cleanup worker should start");
            let result = release_rx
                .recv()
                .map_err(|_| PreviewCaptureError::DeadlineExceeded);
            finished_tx.send(()).expect("cleanup worker should finish");
            result
        },
    )
    .expect_err("a cleanup operation that outlives the deadline must fail settlement");

    assert_eq!(
        started_rx
            .recv_timeout(Duration::from_millis(100))
            .expect("settlement should start cleanup"),
        CaptureCleanupOperation::Stop
    );
    assert!(
        started_at.elapsed() < Duration::from_millis(300),
        "settlement exceeded its shared cleanup deadline"
    );
    assert!(matches!(
        &error,
        PreviewCaptureError::CleanupFailed(context)
            if matches!(context.primary(), PreviewCaptureError::CaptureClosed)
            && context.operation() == "stop"
            && matches!(context.secondary(), PreviewCaptureError::DeadlineExceeded)
    ));

    drop(release_tx);
    finished_rx
        .recv_timeout(Duration::from_millis(300))
        .expect("the explicit cleanup reaper must retain and finish the worker");
}

#[test]
fn cleanup_failure_remains_visible_with_the_primary_capture_error() {
    let error = composed_cleanup_error(
        PreviewCaptureError::DeadlineExceeded,
        PreviewCaptureError::CaptureFailed("capture thread could not be joined".into()),
    );

    assert!(matches!(
        &error,
        PreviewCaptureError::CleanupFailed(context)
            if matches!(context.primary(), PreviewCaptureError::DeadlineExceeded)
            && context.operation() == "stop"
            && matches!(
                context.secondary(),
                PreviewCaptureError::CaptureFailed(message)
                    if message == "capture thread could not be joined"
            )
    ));
    assert!(error.to_string().contains(
        "cleanup stop failed: Windows Graphics Capture failed: capture thread could not be joined"
    ));
}

#[test]
fn deeply_nested_cleanup_diagnostic_is_depth_bounded_at_the_formatting_boundary() {
    let error = deeply_nested_cleanup_error(128);

    let rendered = error.to_string();
    assert!(rendered.len() <= MAX_CLEANUP_DIAGNOSTIC_BYTES);
    assert!(rendered.ends_with(CLEANUP_DIAGNOSTIC_TRUNCATION_MARKER));

    let mapped = PreviewError::from_capture_error(error, PathBuf::from("approved.png").as_path());
    assert!(matches!(
        mapped,
        PreviewError::CaptureCleanupFailed { primary, .. }
            if matches!(
                primary.as_ref(),
                PreviewError::VisibleWindowsCaptureUnavailable {
                    kind: CaptureUnavailableKind::DeadlineExceeded,
                    ..
                }
            )
    ));
}

#[test]
fn oversized_multibyte_cleanup_diagnostics_are_utf8_and_exactly_bounded() {
    let fixed_prefix = "Windows Graphics Capture failed: ";
    let payload_budget = MAX_CLEANUP_DIAGNOSTIC_BYTES
        .saturating_sub(fixed_prefix.len() + CLEANUP_DIAGNOSTIC_TRUNCATION_MARKER.len());
    let ascii_prefix = "a".repeat(payload_budget % 2);
    let multibyte = "é".repeat((payload_budget - ascii_prefix.len()) / 2);
    let message = format!("{ascii_prefix}{multibyte}overflow");
    let error = PreviewCaptureError::CaptureFailed(message);

    let rendered = error.to_string();
    assert_eq!(rendered.len(), MAX_CLEANUP_DIAGNOSTIC_BYTES);
    assert!(rendered.ends_with(CLEANUP_DIAGNOSTIC_TRUNCATION_MARKER));
    assert!(std::str::from_utf8(rendered.as_bytes()).is_ok());

    let mapped = PreviewError::from_capture_error(
        composed_cleanup_error(PreviewCaptureError::DeadlineExceeded, error),
        PathBuf::from("approved.png").as_path(),
    );
    assert!(matches!(
        mapped,
        PreviewError::CaptureCleanupFailed { primary, reason, .. }
            if matches!(
                primary.as_ref(),
                PreviewError::VisibleWindowsCaptureUnavailable {
                    kind: CaptureUnavailableKind::DeadlineExceeded,
                    ..
                }
            )
                && reason.len() == MAX_CLEANUP_DIAGNOSTIC_BYTES
                && reason.ends_with(CLEANUP_DIAGNOSTIC_TRUNCATION_MARKER)
    ));
}

#[test]
fn long_secret_like_capture_diagnostics_are_redacted_at_low_and_high_boundaries() {
    const SECRET: &str = "UI_PREVIEW_CAPTURE_LONG_SECRET_SENTINEL";
    let message = format!(
        "capture failed token={SECRET} {}",
        "x".repeat(MAX_CLEANUP_DIAGNOSTIC_BYTES * 2)
    );
    let low = PreviewCaptureError::CaptureFailed(message);
    let low_rendered = low.to_string();
    assert!(!low_rendered.contains(SECRET));
    assert!(low_rendered.len() <= MAX_CLEANUP_DIAGNOSTIC_BYTES);

    let high = PreviewError::from_capture_error(low, PathBuf::from("approved.png").as_path());
    let high_reason = match &high {
        PreviewError::WindowsGraphicsCaptureFailed { reason } => reason,
        other => panic!("unexpected mapped error: {other:?}"),
    };
    assert!(!high_reason.contains(SECRET));
    assert!(high_reason.len() <= MAX_CLEANUP_DIAGNOSTIC_BYTES);
    assert!(!high.to_string().contains(SECRET));
}

#[test]
fn high_level_preview_paths_and_errors_are_bounded_and_redacted() {
    const SECRET: &str = "UI_PREVIEW_PATH_SECRET_SENTINEL";
    let path = PathBuf::from(format!(
        r"C:\Users\preview-user\api_key={SECRET}\capture.png"
    ));
    let oversized_message = format!(
        "credential={SECRET} {}",
        "x".repeat(8 * MAX_CLEANUP_DIAGNOSTIC_BYTES)
    );
    let errors = [
        PreviewError::InvalidArgument(oversized_message.clone()),
        PreviewError::InvalidArgument(format!(
            "fixture must use the .json extension: {}",
            path.display()
        )),
        PreviewError::OutsideApprovedRoot {
            path: path.clone(),
            root_kind: "output",
        },
        PreviewError::SensitivePath { path: path.clone() },
        PreviewError::FixtureMissing { path: path.clone() },
        PreviewError::FixtureNotRegular { path: path.clone() },
        PreviewError::FixtureTooLarge {
            path: path.clone(),
            bytes: 999,
            max_bytes: 1,
        },
        PreviewError::FixtureIo {
            path: path.clone(),
            message: oversized_message.clone(),
        },
        PreviewError::MalformedFixture {
            path: path.clone(),
            message: oversized_message.clone(),
        },
        PreviewError::UnsupportedSchema {
            path: path.clone(),
            schema: oversized_message.clone(),
        },
        PreviewError::OutputAlreadyExists { path },
    ];

    for error in errors {
        let rendered = error.to_string();
        assert!(
            rendered.len() <= MAX_CLEANUP_DIAGNOSTIC_BYTES,
            "high-level error exceeded the diagnostic budget: {} bytes",
            rendered.len()
        );
        assert!(
            !rendered.contains(SECRET),
            "high-level error leaked a secret: {rendered:?}"
        );
    }
}

#[test]
fn invalid_output_extension_diagnostic_omits_the_source_path() {
    let (_root, policy) = temporary_policy();
    let output = policy
        .output_root()
        .join("nested")
        .join("output-api_key=UI_OUTPUT_PATH_SECRET_SENTINEL.txt");
    let error = match PreviewRequest::validate(write_fixture(&policy), output.clone(), &policy) {
        Err(error) => error,
        Ok(_) => panic!("a non-PNG output must be rejected"),
    };
    let rendered = error.to_string();

    assert!(rendered.contains("output must use the .png extension"));
    assert!(!rendered.contains(policy.output_root().to_string_lossy().as_ref()));
    assert!(!rendered.contains("UI_OUTPUT_PATH_SECRET_SENTINEL"));
    assert!(rendered.len() <= MAX_CLEANUP_DIAGNOSTIC_BYTES);
}

#[cfg(windows)]
#[test]
fn preview_rejects_junction_ancestors_before_fixture_or_output_io() {
    let (_root, policy) = temporary_policy();
    let redirect_target = policy
        .output_root()
        .parent()
        .expect("output parent")
        .join("redirect-target");
    fs::create_dir(&redirect_target).expect("redirect target");
    let redirect = policy.output_root().join("redirect");
    create_directory_junction(&redirect_target, &redirect);

    let error = PreviewRequest::validate(
        write_fixture(&policy),
        redirect.join("blocked.png"),
        &policy,
    )
    .expect_err("reparse ancestors must be rejected before output access");
    assert!(matches!(error, PreviewError::UnsafePath { .. }));

    remove_directory_junction(&redirect);
}

#[cfg(windows)]
#[test]
fn trusted_output_root_identity_blocks_a_junction_swap_after_validation() {
    let (_root, policy) = temporary_policy();
    let request = valid_request(&policy, "swap.png");
    let original_root = policy.output_root().to_path_buf();
    let moved_root = original_root.with_file_name("output-root-before-swap");
    fs::rename(&original_root, &moved_root).expect("move trusted output root");
    create_directory_junction(&moved_root, &original_root);

    let result = request.write_bgra_png_atomic(1, 1, &[0x1e, 0x14, 0x0a, 0xff]);
    assert!(matches!(
        result,
        Err(PreviewCaptureError::OutputFailed(message))
            if message.contains("trusted preview output root changed")
    ));

    remove_directory_junction(&original_root);
    fs::rename(&moved_root, &original_root).expect("restore trusted output root");
}

#[cfg(windows)]
#[test]
fn trusted_output_parent_identity_blocks_regular_directory_substitution() {
    let (_root, policy) = temporary_policy();
    let parent = policy.output_root().join("nested");
    fs::create_dir_all(&parent).expect("nested output parent");
    let request = valid_request(&policy, "nested/substitution.png");
    let moved_parent = policy.output_root().join("nested-before-substitution");
    fs::rename(&parent, &moved_parent).expect("move trusted output parent");
    fs::create_dir(&parent).expect("replace trusted output parent");

    let result = request.write_bgra_png_atomic(1, 1, &[0x1e, 0x14, 0x0a, 0xff]);
    assert!(matches!(
        result,
        Err(PreviewCaptureError::OutputFailed(message))
            if message.contains("trusted preview output parent changed")
    ));

    fs::remove_dir(&parent).expect("remove substituted output parent");
    fs::rename(&moved_parent, &parent).expect("restore trusted output parent");
}

#[test]
fn diagnostics_redact_a_complete_secret_bearing_line_before_bounding() {
    const SECRET: &str = "UI_PREVIEW_CAPTURE_PREFIX_SECRET_SENTINEL";
    let message = format!(
        "{SECRET} {} token=late-secret",
        "x".repeat(MAX_CLEANUP_DIAGNOSTIC_BYTES * 2)
    );

    let rendered = PreviewCaptureError::CaptureFailed(message).to_string();

    assert!(!rendered.contains(SECRET));
    assert!(rendered.len() <= MAX_CLEANUP_DIAGNOSTIC_BYTES);
}

#[test]
fn cancelled_capture_generation_cannot_publish_after_a_replacement_attempt() {
    let generations = CaptureGeneration::new();
    let stale = generations.begin();
    stale.cancel();
    assert!(!stale.is_active());

    let current = generations.begin();
    assert!(current.is_active());
    assert!(!stale.is_active());
    current.cancel();
    assert!(!current.is_active());
}

#[test]
fn injected_blocking_capture_stage_returns_at_the_shared_deadline_and_keeps_ownership() {
    let lease = CaptureGeneration::new().begin();
    let error = run_cancellable_stage(
        CaptureDeadline::from_now(Duration::from_millis(10)),
        lease.clone(),
        |_, worker_lease| {
            std::thread::sleep(Duration::from_millis(100));
            assert!(!worker_lease.is_active());
            Ok::<_, PreviewCaptureError>(())
        },
    )
    .expect_err("a stalled stage must return without waiting for its worker");
    assert!(matches!(error, PreviewCaptureError::DeadlineExceeded));
    assert!(!lease.is_active());
}

#[test]
fn capture_error_display_and_debug_redact_control_sequences_and_window_handles() {
    let error = PreviewCaptureError::CaptureFailed(
        "\u{1b}[31mapi_key=UI_CAPTURE_DEBUG_SECRET\u{1b}[0m hwnd=0x1234\u{7}".into(),
    );
    let display = error.to_string();
    let debug = format!("{error:?}");
    for rendered in [display, debug] {
        assert!(!rendered.contains('\u{1b}'));
        assert!(!rendered.contains("UI_CAPTURE_DEBUG_SECRET"));
        assert!(!rendered.contains('\u{7}'));
    }

    let foreground = PreviewCaptureError::ForegroundChanged {
        before: 0x1234,
        after: 0x5678,
    };
    assert!(!foreground.to_string().contains("1234"));
    assert!(!format!("{foreground:?}").contains("5678"));
}

#[test]
fn cleanup_timeout_is_reported_as_a_typed_deadline_failure() {
    let error = settle_capture_with_cleanup(
        Ok::<_, PreviewCaptureError>(()),
        CaptureDeadline::from_now(Duration::ZERO),
        |_| Ok(()),
    )
    .expect_err("an expired shared deadline must fail cleanup settlement");

    assert!(matches!(
        error,
        PreviewCaptureError::CleanupFailed(context)
            if matches!(context.secondary(), PreviewCaptureError::DeadlineExceeded)
    ));
}

#[test]
fn late_cleanup_remains_owned_after_shared_deadline() {
    let (started_tx, started_rx) = mpsc::channel();
    let (finished_tx, finished_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel::<()>();
    let error = settle_capture_with_cleanup(
        Err::<(), _>(PreviewCaptureError::DeadlineExceeded),
        CaptureDeadline::from_now(Duration::ZERO),
        move |_| {
            started_tx
                .send(())
                .expect("the cleanup probe receiver should remain available");
            release_rx
                .recv()
                .map_err(|_| PreviewCaptureError::DeadlineExceeded)?;
            finished_tx
                .send(())
                .expect("the cleanup worker should remain owned");
            Ok(())
        },
    )
    .expect_err("an expired deadline must return its cleanup failure");

    assert!(started_rx.recv_timeout(Duration::from_millis(100)).is_ok());
    assert!(matches!(
        error,
        PreviewCaptureError::CleanupFailed(context)
            if matches!(context.secondary(), PreviewCaptureError::DeadlineExceeded)
    ));

    release_tx
        .send(())
        .expect("the cleanup worker should be released");
    finished_rx
        .recv_timeout(Duration::from_millis(100))
        .expect("late cleanup must remain owned until it settles");
    assert_eq!(active_capture_thread_count(), 0);
}

#[test]
fn cleanup_worker_panics_are_reported_without_unwinding_the_capture_caller() {
    let error = settle_capture_with_cleanup(
        Err::<(), _>(PreviewCaptureError::CaptureClosed),
        CaptureDeadline::from_now(Duration::from_secs(1)),
        |_| -> Result<(), PreviewCaptureError> {
            panic!("capture cleanup worker panic");
        },
    )
    .expect_err("a panicking cleanup worker must remain a typed capture error");

    assert!(matches!(
        error,
        PreviewCaptureError::CleanupFailed(context)
            if matches!(context.primary(), PreviewCaptureError::CaptureClosed)
                && matches!(
                    context.secondary(),
                    PreviewCaptureError::CaptureFailed(message)
                        if message == "cleanup worker stopped without reporting a result"
                )
    ));
}

#[test]
fn late_output_cleanup_is_owned_and_leaves_no_residue() {
    let (_root, policy) = temporary_policy();
    let output = policy.output_root().join("late-output.png");
    fs::write(&output, b"late output").expect("late output fixture");

    let error = cleanup_output_after_deadline(
        &output,
        PreviewCaptureError::DeadlineExceeded,
        CaptureDeadline::from_now(Duration::ZERO),
    );

    assert!(matches!(
        error,
        PreviewCaptureError::CleanupFailed(context)
            if matches!(context.secondary(), PreviewCaptureError::DeadlineExceeded)
    ));
    for _ in 0..20 {
        active_capture_thread_count();
        if !output.exists() {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        !output.exists(),
        "late output cleanup left a published file"
    );
}

#[test]
fn final_capture_settlement_fences_a_late_success_and_cleans_output() {
    let (_root, policy) = temporary_policy();
    let output = policy.output_root().join("late-success.png");
    fs::write(&output, b"late success").expect("late output fixture");
    let report = CaptureReport {
        width: 1,
        height: 1,
        foreground_before: 1,
        foreground_after: 1,
    };

    let deadline = CaptureDeadline::from_now(Duration::from_millis(1));
    std::thread::sleep(Duration::from_millis(5));
    let error = settle_capture_result(&output, Ok(report), deadline)
        .expect_err("a success crossing the deadline must be rejected");

    assert!(matches!(
        error,
        PreviewCaptureError::CleanupFailed(context)
            if matches!(context.primary(), PreviewCaptureError::DeadlineExceeded)
    ));
    for _ in 0..20 {
        active_capture_thread_count();
        if !output.exists() {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        !output.exists(),
        "late success left a published file behind"
    );
}

#[test]
fn expired_png_encoding_is_bounded_and_leaves_no_temp_residue() {
    let (_root, policy) = temporary_policy();
    let output = policy.output_root().join("expired.png");
    let error = encode_bgra_png_atomic(
        &output,
        1,
        1,
        &[0x1e, 0x14, 0x0a, 0xff],
        CaptureDeadline::from_now(Duration::ZERO),
    )
    .expect_err("expired PNG encoding must stop at the shared deadline");

    assert!(matches!(error, PreviewCaptureError::DeadlineExceeded));
    assert!(!output.exists());
    assert!(fs::read_dir(policy.output_root())
        .expect("output directory")
        .next()
        .is_none());
}

#[test]
fn png_dimension_overflow_is_rejected_without_output_or_temp_residue() {
    let (_root, policy) = temporary_policy();
    let output = policy.output_root().join("overflow.png");
    let error = encode_bgra_png_atomic(
        &output,
        u32::MAX,
        u32::MAX,
        &[],
        CaptureDeadline::from_now(FIRST_FRAME_DEADLINE),
    )
    .expect_err("overflowing dimensions must not enter PNG output");

    assert!(matches!(error, PreviewCaptureError::PngFailed(_)));
    assert!(!output.exists());
    assert!(fs::read_dir(policy.output_root())
        .expect("output directory")
        .next()
        .is_none());
}

#[test]
fn render_error_mapping_preserves_actionable_capture_categories() {
    let output = PathBuf::from("approved.png");

    assert_eq!(
        PreviewError::from_capture_error(
            PreviewCaptureError::PngFailed("invalid frame".into()),
            &output,
        ),
        PreviewError::PngFailed {
            reason: "invalid frame".into()
        }
    );
    assert_eq!(
        PreviewError::from_capture_error(
            PreviewCaptureError::OutputFailed("disk full".into()),
            &output,
        ),
        PreviewError::OutputFailed {
            reason: "disk full".into()
        }
    );
    assert_eq!(
        PreviewError::from_capture_error(
            PreviewCaptureError::ForegroundChanged {
                before: 0x10,
                after: 0x20,
            },
            &output,
        ),
        PreviewError::ForegroundChanged {
            before: 0x10,
            after: 0x20,
        }
    );
    assert_eq!(
        PreviewError::from_capture_error(
            PreviewCaptureError::ApplicationFailed("window startup failed".into()),
            &output,
        ),
        PreviewError::ApplicationFailed {
            reason: "window startup failed".into()
        }
    );
    assert_eq!(
        PreviewError::from_capture_error(
            PreviewCaptureError::CaptureFailed("WGC rejected the item".into()),
            &output,
        ),
        PreviewError::WindowsGraphicsCaptureFailed {
            reason: "WGC rejected the item".into()
        }
    );
    let unavailable_cases = [
        (
            PreviewCaptureError::UnsupportedPlatform,
            CaptureUnavailableKind::UnsupportedPlatform,
        ),
        (
            PreviewCaptureError::InvalidHwnd,
            CaptureUnavailableKind::InvalidHwnd,
        ),
        (
            PreviewCaptureError::ForeignHwnd,
            CaptureUnavailableKind::ForeignHwnd,
        ),
        (
            PreviewCaptureError::InvalidWindowState { reason: "hidden" },
            CaptureUnavailableKind::InvalidWindowState { reason: "hidden" },
        ),
        (
            PreviewCaptureError::DeadlineExceeded,
            CaptureUnavailableKind::DeadlineExceeded,
        ),
        (
            PreviewCaptureError::CaptureClosed,
            CaptureUnavailableKind::CaptureClosed,
        ),
    ];
    for (error, expected_kind) in unavailable_cases {
        let mapped = PreviewError::from_capture_error(error, &output);
        assert!(matches!(
            mapped,
            PreviewError::VisibleWindowsCaptureUnavailable { kind, .. }
                if kind == expected_kind
        ));
    }

    let cleanup = PreviewError::from_capture_error(
        composed_cleanup_error(
            PreviewCaptureError::DeadlineExceeded,
            PreviewCaptureError::CaptureFailed("cleanup deadline exceeded".into()),
        ),
        &output,
    );
    assert!(matches!(
        cleanup,
        PreviewError::CaptureCleanupFailed {
            primary,
            operation,
            reason,
        } if operation == "stop"
            && reason == "Windows Graphics Capture failed: cleanup deadline exceeded"
            && matches!(
                primary.as_ref(),
                PreviewError::VisibleWindowsCaptureUnavailable {
                    kind: CaptureUnavailableKind::DeadlineExceeded,
                    ..
                }
            )
    ));
}

#[test]
fn validated_request_owns_the_only_public_bgra_output_boundary() {
    let (_root, policy) = temporary_policy();
    let request = valid_request(&policy, "policy-bound.png");
    let bgra = [0x1e, 0x14, 0x0a, 0xff];

    request
        .write_bgra_png_atomic(1, 1, &bgra)
        .expect("validated request should write its approved output");

    let rgba = image::open(request.output_path())
        .expect("approved output should decode")
        .to_rgba8();
    assert_eq!(rgba.get_pixel(0, 0).0, [0x0a, 0x14, 0x1e, 0xff]);
}

#[test]
fn bgra_png_is_decodable_physical_sized_and_preserves_rgb_alpha_without_temp_residue() {
    let (_root, policy) = temporary_policy();
    let output = policy.output_root().join("encoded.png");
    let bgra = vec![
        0x1e, 0x14, 0x0a, 0xff, 0x3c, 0x32, 0x28, 0x80, 0x5a, 0x50, 0x46, 0x40, 0x78, 0x6e, 0x64,
        0x00,
    ];

    let request = PreviewRequest::validate(write_fixture(&policy), output.clone(), &policy)
        .expect("validated request should own output encoding");
    request
        .write_bgra_png_atomic(2, 2, &bgra)
        .expect("BGRA frame should encode atomically");

    let image = image::open(&output).expect("encoded PNG should decode");
    assert_eq!(image.dimensions(), (2, 2));
    let rgba = image.to_rgba8();
    assert_eq!(
        rgba.pixels().map(|pixel| pixel.0).collect::<Vec<_>>(),
        vec![
            [0x0a, 0x14, 0x1e, 0xff],
            [0x28, 0x32, 0x3c, 0x80],
            [0x46, 0x50, 0x5a, 0x40],
            [0x64, 0x6e, 0x78, 0x00],
        ]
    );
    assert!(fs::read_dir(policy.output_root())
        .expect("output directory")
        .filter_map(Result::ok)
        .all(|entry| entry.path() == output));
}

#[test]
fn asymmetric_bgra_capture_seam_preserves_exact_rgba_channels() {
    let (_root, policy) = temporary_policy();
    let request = valid_request(&policy, "asymmetric.png");
    let bgra = [0xd4, 0x2b, 0x91, 0xe7];

    request
        .write_bgra_png_atomic(1, 1, &bgra)
        .expect("the capture frame conversion seam should encode");

    let rgba = image::open(request.output_path())
        .expect("the converted capture frame should decode")
        .to_rgba8();
    assert_eq!(rgba.get_pixel(0, 0).0, [0x91, 0x2b, 0xd4, 0xe7]);
}

#[cfg(windows)]
#[test]
fn invalid_and_foreign_hwnds_are_rejected_before_capture() {
    use devmanager::ui::preview_capture::{validate_native_window, NativeHwnd};
    use windows::Win32::UI::WindowsAndMessaging::{GetDesktopWindow, GetShellWindow};

    assert!(matches!(
        validate_native_window(NativeHwnd(0)),
        Err(devmanager::ui::preview_capture::PreviewCaptureError::InvalidHwnd)
    ));

    for hwnd in [unsafe { GetDesktopWindow() }, unsafe { GetShellWindow() }] {
        let hwnd = NativeHwnd(hwnd.0 as isize);
        let error = validate_native_window(hwnd).expect_err("foreign windows are not capturable");
        assert!(matches!(
            error,
            devmanager::ui::preview_capture::PreviewCaptureError::ForeignHwnd
                | devmanager::ui::preview_capture::PreviewCaptureError::InvalidWindowState { .. }
        ));
    }
}

#[cfg(windows)]
#[test]
fn visible_capture_uses_isolated_process_and_decodes_exact_sentinel() {
    use devmanager::ui::preview_capture::foreground_hwnd;

    const EXPECTED_SENTINEL_RGBA: [u8; 4] = [0x91, 0x2b, 0xd4, 0xff];
    let output = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join(".devmanager-next/evidence/phase-05/screenshots")
        .join(format!("wgc-sentinel-{}.png", std::process::id()));
    let fixture =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/ui/theme-gallery.json");
    let before = foreground_hwnd();

    let child = Command::new(env!("CARGO_BIN_EXE_devmanager-next"))
        .env("DEVMANAGER_INSTANCE_LABEL", "Next")
        .env("DEVMANAGER_RUNTIME_KIND", "native-next")
        .args([
            "--ui-preview",
            fixture.to_str().expect("fixture path is UTF-8"),
            "--output",
            output.to_str().expect("output path is UTF-8"),
        ])
        .output()
        .expect("isolated devmanager-next --ui-preview must start");
    assert!(
        child.status.success(),
        "isolated preview failed: {}",
        String::from_utf8_lossy(&child.stderr)
    );

    let image = image::open(&output)
        .expect("the isolated process must write a real WGC PNG")
        .to_rgba8();
    assert!(image.pixels().any(|pixel| pixel.0[3] != 0));
    let sentinel_pixels = image
        .pixels()
        .filter(|pixel| pixel.0 == EXPECTED_SENTINEL_RGBA)
        .count();
    assert!(
        sentinel_pixels >= 16,
        "the real WGC frame must contain the exact GPUI sentinel RGBA value, found {sentinel_pixels}"
    );
    fs::remove_file(&output).expect("capture output cleanup");

    assert_eq!(
        foreground_hwnd(),
        before,
        "capture must not change foreground HWND"
    );
    assert_eq!(active_capture_thread_count(), 0);
}

#[cfg(windows)]
#[test]
#[ignore = "manual/VM contract: run at 100/125/150/200 DPI, then repeat occluded and minimized"]
fn manual_vm_visual_capture_matrix_contract() {
    use devmanager::ui::preview_capture::foreground_hwnd;

    let (_root, policy) = temporary_policy();
    let preview = PreviewApplication::load(valid_request(&policy, "manual-vm.png"), &policy)
        .expect("fixture should load");
    let before = foreground_hwnd();
    let result = preview.render_to_output();

    match result {
        Ok(()) => {
            let image = image::open(policy.output_root().join("manual-vm.png"))
                .expect("manual capture must write a decodable PNG");
            assert!(image.width() > 0 && image.height() > 0);
        }
        Err(PreviewError::VisibleWindowsCaptureUnavailable { .. }) => {
            panic!("the manual VM baseline must run on a visible Windows desktop");
        }
        Err(error) => panic!("unexpected manual VM capture error: {error:?}"),
    }

    assert_eq!(foreground_hwnd(), before);
    assert_eq!(active_capture_thread_count(), 0);
    eprintln!(
        "Manual matrix: run this test separately at 100%, 125%, 150%, and 200% DPI; an occluded preview must still capture, while a minimized or closed preview must return VisibleWindowsCaptureUnavailable with no output."
    );
}
