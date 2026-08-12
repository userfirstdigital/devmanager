use devmanager::ui::preview::{
    CaptureUnavailableKind, PreviewApplication, PreviewCaptureSetting, PreviewError,
    PreviewPathPolicy, PreviewRequest,
};
use devmanager::ui::preview_capture::{
    active_capture_thread_count, capture_contract, receive_first_frame,
    settle_capture_with_cleanup, CaptureCleanupOperation, CaptureColorFormat, CaptureDeadline,
    CaptureSetting, PreviewCaptureError, CLEANUP_DIAGNOSTIC_TRUNCATION_MARKER,
    FIRST_FRAME_DEADLINE, MAX_CLEANUP_DIAGNOSTIC_BYTES,
};
use image::GenericImageView;
use std::fs;
use std::path::PathBuf;
use std::process::Command;
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
fn task_cockpit_fixture_describes_the_actual_native_shell_surface() {
    let (_root, policy) = temporary_policy();
    let path = policy.fixture_root().join("task-cockpit.json");
    fs::write(
        &path,
        r#"{
          "schema": "devmanager.ui.preview/v1",
          "id": "task-cockpit",
          "title": "Task Cockpit",
          "capture": { "cursor": "excluded", "border": "excluded" },
          "root": { "kind": "task-cockpit", "label": "Task Cockpit" }
        }"#,
    )
    .expect("task cockpit fixture");
    let request = PreviewRequest::validate(
        &path,
        policy.output_root().join("task-cockpit.png"),
        &policy,
    )
    .expect("valid task cockpit request");
    let preview = PreviewApplication::load(request, &policy).expect("fixture should load");
    let body = &preview.root_snapshot().body;
    for section in [
        "Task Cockpit",
        "Header",
        "Task Inbox",
        "Context Dock",
        "Disconnected",
    ] {
        assert!(body.contains(section), "missing {section} in {body}");
    }
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
            && matches!(
                context.secondary(),
                PreviewCaptureError::CaptureFailed(message) if message.contains("deadline")
            )
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
        .env("DEVMANAGER_PROFILE", "native-next-dev")
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
