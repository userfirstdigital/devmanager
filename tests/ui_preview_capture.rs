use devmanager::ui::preview::{
    CaptureUnavailableKind, PreviewApplication, PreviewCaptureSetting, PreviewError,
    PreviewPathPolicy, PreviewRequest,
};
use devmanager::ui::preview_capture::{
    active_capture_thread_count, capture_contract, cleanup_output_after_deadline,
    encode_bgra_png_atomic, receive_first_frame, run_cancellable_stage, settle_capture_result,
    settle_capture_result_with_authority, settle_capture_with_cleanup, CaptureCleanupOperation,
    CaptureColorFormat, CaptureDeadline, CaptureGeneration, CaptureOutputAuthority, CaptureReport,
    CaptureSetting, PreviewCaptureError, PublishedOutput, CLEANUP_DIAGNOSTIC_TRUNCATION_MARKER,
    FIRST_FRAME_DEADLINE, MAX_CLEANUP_DIAGNOSTIC_BYTES,
};
use image::GenericImageView;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{mpsc, Arc, Mutex, MutexGuard, OnceLock};
use std::time::Duration;
#[cfg(windows)]
use std::time::Instant;
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

#[cfg(windows)]
fn open_retained_output_handle(path: &Path) -> std::fs::File {
    use std::os::windows::fs::OpenOptionsExt;

    let mut options = std::fs::OpenOptions::new();
    options
        .read(true)
        .write(true)
        .access_mode(0x8000_0000 | 0x4000_0000 | 0x0001_0000)
        .share_mode(0x0000_0001 | 0x0000_0002 | 0x0000_0004);
    options.open(path).expect("retained output handle")
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

static CAPTURE_TEST_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

fn capture_test_guard() -> MutexGuard<'static, ()> {
    CAPTURE_TEST_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
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
fn retained_output_cleanup_is_descendant_handle_relative_and_drop_never_removes_by_path() {
    let source = fs::read_to_string(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/ui/preview_capture.rs"),
    )
    .expect("preview capture source");
    assert!(
        source.contains("parent_relative_to_root") || source.contains("verify_parent_descendant"),
        "retained output authority must bind its parent as a descendant of the retained root"
    );
    assert!(
        source.contains("remove_output_relative") && source.contains("remove_temp_relative"),
        "authorized cleanup must remove entries through the retained parent handle"
    );
    assert!(
        !source.contains("fs::remove_file(&self.path)")
            && !source.contains("fs::remove_file(&self.output)"),
        "TempOutput must not re-resolve either temporary or final output paths during Drop"
    );
    assert!(
        !source.contains("fs::remove_file(output)"),
        "deadline settlement must not re-resolve the final output path"
    );
}

#[test]
fn fixture_authority_keeps_root_ancestor_chain_through_final_open() {
    let source =
        fs::read_to_string(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/ui/preview.rs"))
            .expect("preview source");
    for marker in [
        "FixtureDirectoryAuthority",
        "fixture_ancestor_chain",
        "verify_fixture_containment",
        "open_fixture_relative",
    ] {
        assert!(
            source.contains(marker),
            "fixture authority must retain and revalidate {marker}"
        );
    }
}

#[test]
fn capture_manifest_decodes_ihdr_and_layout_contract_is_exact() {
    let capture_source = fs::read_to_string(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/ui/preview_capture.rs"),
    )
    .expect("preview capture source");
    let script = fs::read_to_string(
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("scripts/native-next/Capture-UiPreviews.ps1"),
    )
    .expect("preview capture script");
    for marker in ["client_width", "frame.width", "PREVIEW_WINDOW_WIDTH"] {
        assert!(
            capture_source.contains(marker),
            "native capture must assert the exact client/frame width contract ({marker})"
        );
    }
    for marker in [
        "Get-PngDimensions -Authority",
        "IHDR",
        "ExpectedWidth",
        "ExpectedHeight",
    ] {
        assert!(
            script.contains(marker),
            "capture manifest must derive and record decoded PNG IHDR dimensions ({marker})"
        );
    }
}

#[test]
fn window_probe_joins_after_bounded_discovery_and_writes_failure_evidence() {
    let script = fs::read_to_string(
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("scripts/native-next/Capture-UiPreviews.ps1"),
    )
    .expect("preview capture script");
    for marker in [
        "MainWindowHandle",
        "WaitForExitAsync",
        "Get-PreviewRemainingMilliseconds",
        "Job",
        "preview.process.join-unconfirmed",
        "probe-failed",
        "HoldEvidence",
        "JoinState",
    ] {
        assert!(
            script.contains(marker),
            "window probe must implement bounded lifecycle/evidence marker {marker}"
        );
    }
    assert!(
        script.contains("if (-not $joined -or") || script.contains("if(-not $joined -or"),
        "ExitCode must only be read after WaitForExit reports completion"
    );
    assert!(
        !script.contains("WaitForExit(1000)") && !script.contains("Kill($true)"),
        "window probe cleanup joins must remain job-owned and deadline-bounded"
    );
}

#[test]
fn gallery_keeps_full_long_text_in_visible_continuation_segments() {
    let source =
        fs::read_to_string(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/ui/preview.rs"))
            .expect("preview source");
    assert!(
        source.contains("samples.long_text") && source.contains("samples.unicode"),
        "gallery must route the real long and Unicode fixture values through rendering"
    );
    assert!(
        source.contains("split_gallery_sample")
            && source.contains("continuation")
            && source.contains("GALLERY_SAMPLE_SEGMENT_COLUMNS"),
        "the long sample must be rendered as explicitly labeled visible continuation segments"
    );
    assert!(
        !source.contains(".max_h(px(32.0))")
            && !source.contains(".overflow_hidden()")
            && !source.contains(".line_clamp(2)"),
        "the required long sample must not hide its remainder behind clipping"
    );
    assert!(
        source.contains("GALLERY_SAMPLE_SEGMENT_WIDTH_PX")
            && source.contains("GALLERY_CONTENT_WIDTH_PX"),
        "segment width must participate in an explicit physical layout bound"
    );
    assert!(
        source.contains("GALLERY_SAMPLE_SEGMENTS_PER_PAGE")
            && source.contains("continuation_start"),
        "long text remainder must be explicitly paged with continuation semantics"
    );
    assert!(
        source.contains("const MAX_LINE_SCALARS: usize = 30"),
        "unicode wrapping must leave enough vertical room for the full continuation row"
    );
    assert!(
        source.contains("GALLERY_SAMPLE_SEGMENT_MAX_LINES"),
        "long text must assert a deterministic vertical line budget per continuation segment"
    );
    assert!(
        source.contains("surfaces.canvas"),
        "gallery canvas must follow the selected theme, including light mode"
    );
}

#[test]
fn gallery_fixture_preserves_decomposed_combining_sequences_alongside_unicode_coverage() {
    let fixture_path =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/ui/component-gallery.json");
    let fixture: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(fixture_path).expect("component gallery fixture"))
            .expect("component gallery JSON");
    let unicode = fixture["root"]["gallery"]["samples"]["unicode"]
        .as_str()
        .expect("unicode gallery sample");
    assert!(
        unicode.contains('\u{0301}'),
        "fixture must include e + U+0301"
    );
    assert!(
        unicode.contains('\u{094d}'),
        "fixture must include a non-Latin combining cluster"
    );
    for expected in ['界', 'م', '🙂'] {
        assert!(
            unicode.contains(expected),
            "fixture must retain the existing CJK/RTL/emoji coverage"
        );
    }
}

#[test]
fn capture_script_never_path_deletes_outputs_and_uses_unique_retry_names() {
    let script = fs::read_to_string(
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("scripts/native-next/Capture-UiPreviews.ps1"),
    )
    .expect("preview capture script");
    for forbidden in [
        "Remove-CaptureOutputBestEffort",
        "Test-Path -LiteralPath $output",
        "Test-Path -LiteralPath $OutputPath",
        "Remove-Item -LiteralPath $output",
        "Remove-Item -LiteralPath $OutputPath",
    ] {
        assert!(
            !script.contains(forbidden),
            "capture script must not path-delete or path-probe output via {forbidden}"
        );
    }
    for required in [
        "attempt-$attempt",
        "OutputEvidence",
        "output-name-unique-per-attempt",
        "BinaryPath",
        "caller-supplied warm binary paths are disabled",
    ] {
        assert!(
            script.contains(required),
            "capture retries must retain unique output evidence via {required}"
        );
    }
}

#[test]
fn preview_script_bounds_every_external_reader_on_one_deadline() {
    let script = fs::read_to_string(
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("scripts/native-next/Capture-UiPreviews.ps1"),
    )
    .expect("preview capture script");
    for marker in [
        "MAX_PREVIEW_ARTIFACT_BYTES",
        "MAX_PREVIEW_RECEIPT_BYTES",
        "MAX_PREVIEW_MANIFEST_BYTES",
        "MAX_PREVIEW_PNG_BYTES",
        "PREVIEW_HASH_CHUNK_BYTES",
        "Assert-PreviewDeadline",
        "TransformBlock",
        "Read-PreviewUtf8Text",
        "Get-PreviewArtifactSha256 -Stream",
        "Get-PreviewEmbeddedBuildIdentity -Stream",
        "Get-PngDimensions -Authority",
    ] {
        assert!(
            script.contains(marker),
            "preview readers must be bounded by the shared deadline and byte caps: {marker}"
        );
    }
    for forbidden in ["ComputeHash($Stream)", "ReadToEnd()", "ReadAllBytes($Path)"] {
        assert!(
            !script.contains(forbidden),
            "preview readers must not use an unbounded {forbidden} path"
        );
    }
}

#[test]
fn preview_script_keeps_retained_tool_handles_without_reopening_them() {
    let script = fs::read_to_string(
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("scripts/native-next/Capture-UiPreviews.ps1"),
    )
    .expect("preview capture script");
    for marker in [
        "Assert-PreviewToolAuthorityStable",
        "-ToolAuthorities $BuildIdentity.ToolAuthorities",
        "Get-PreviewBuildIdentity -Deadline $deadline -ToolAuthorities $BuildIdentity.ToolAuthorities",
    ] {
        assert!(
            script.contains(marker),
            "retained FILE_SHARE_NONE tool authority must be revalidated without reopening: {marker}"
        );
    }
}

#[test]
fn preview_script_pins_rustup_identity_and_fences_toolchain_environment() {
    let script = fs::read_to_string(
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("scripts/native-next/Capture-UiPreviews.ps1"),
    )
    .expect("preview capture script");
    for marker in [
        "$rustupHomePath",
        "$rustupHomeAuthority",
        "RUSTUP_HOME",
        "CARGO_HOME",
        "rustupHomeFileIdentity",
        "Assert-PreviewToolAuthorityStable -Authority $rustupAuthority",
    ] {
        assert!(
            script.contains(marker),
            "rustup provenance must pin and retain {marker}"
        );
    }
}

#[test]
fn preview_script_uses_one_absolute_deadline_for_commands_and_capture_io() {
    let script = fs::read_to_string(
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("scripts/native-next/Capture-UiPreviews.ps1"),
    )
    .expect("preview capture script");
    for marker in [
        "$previewDeadline = New-PreviewIoDeadline",
        "Invoke-PreviewExternalCommand",
        "AbsoluteDeadline",
        "-Deadline $previewDeadline",
    ] {
        assert!(
            script.contains(marker),
            "all external commands/readers must share one absolute deadline: {marker}"
        );
    }
}

#[test]
fn preview_script_retains_output_ancestor_chain_and_publishes_relative_to_handle() {
    let script = fs::read_to_string(
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("scripts/native-next/Capture-UiPreviews.ps1"),
    )
    .expect("preview capture script");
    for marker in [
        "OutputRootAncestorChain",
        "WriteAtomicPreviewReceiptRelative",
        "RootDirectory",
        "PublishRelative",
    ] {
        assert!(
            script.contains(marker),
            "output publication must retain the ancestor chain and stay handle-relative: {marker}"
        );
    }
}

#[test]
fn preview_children_are_created_and_reopened_relative_to_retained_no_follow_handles() {
    let script = fs::read_to_string(
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("scripts/native-next/Capture-UiPreviews.ps1"),
    )
    .expect("preview capture script");
    let capture_source = fs::read_to_string(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/ui/preview_capture.rs"),
    )
    .expect("preview capture source");
    for marker in [
        "CreateFileRelative",
        "NtCreateFile",
        "OpenReadRelativeNoFollow",
        "WriteAtomicPreviewReceiptRelative",
    ] {
        assert!(
            script.contains(marker),
            "PowerShell publication must provide {marker}"
        );
    }
    assert!(
        !script.contains("Path.Combine(directory, temporaryName)"),
        "PowerShell receipt temps must not be created by re-resolving a directory path"
    );
    for marker in [
        "open_temp_output_relative",
        "next_temp_name",
        "FILE_CREATE",
        "ancestor_handles",
        "verify_ancestor_chain",
    ] {
        assert!(
            capture_source.contains(marker),
            "Rust publication must provide {marker}"
        );
    }
    assert!(
        !capture_source.contains("open_temp_output(&temp_path)"),
        "Rust PNG temps must not be opened through a path after authority capture"
    );
}

#[test]
fn preview_publication_reuses_retained_parent_handle_without_path_reopen() {
    let source = fs::read_to_string(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/ui/preview_capture.rs"),
    )
    .expect("preview capture source");
    assert!(
        source.contains("self.parent_handle.try_clone()"),
        "publication must clone the already-retained parent authority"
    );
    assert!(
        !source.contains("open_directory_authority(&self.parent_path)"),
        "publication must not re-resolve the parent path after authority capture"
    );
}

#[test]
fn preview_script_final_cleanup_attempts_every_owned_resource() {
    let script = fs::read_to_string(
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("scripts/native-next/Capture-UiPreviews.ps1"),
    )
    .expect("preview capture script");
    for marker in [
        "Invoke-PreviewFinalCleanup",
        "Invoke-PreviewCleanupStep",
        "Close-PreviewTrackedProcesses -Deadline $Deadline",
        "preview.cleanup.failed",
        "OldBuildIdentity",
    ] {
        assert!(
            script.contains(marker),
            "final cleanup must attempt all resources and retain a fixed failure code: {marker}"
        );
    }
}

#[test]
fn preview_processes_are_job_owned_and_descendant_count_is_join_verified() {
    let script = fs::read_to_string(
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("scripts/native-next/Capture-UiPreviews.ps1"),
    )
    .expect("preview capture script");
    for marker in [
        "CreateJobObjectW",
        "AssignProcessToJobObject",
        "JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE",
        "ActiveProcessCount",
        "StartProcessInJob",
        ".Job",
    ] {
        assert!(
            script.contains(marker),
            "job ownership must provide {marker}"
        );
    }
    assert!(
        !script.contains("Start-Process -FilePath $authority.Path")
            && !script.contains("Kill($true)"),
        "preview process cleanup must use the owned job and one bounded join"
    );
}

#[test]
fn preview_capture_new_processes_are_suspended_before_job_assignment() {
    let script = fs::read_to_string(
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("scripts/native-next/Capture-UiPreviews.ps1"),
    )
    .expect("preview capture script");
    for marker in [
        "CreateProcessW",
        "CREATE_SUSPENDED",
        "AssignProcessToJobObject",
        "ResumeThread",
    ] {
        assert!(
            script.contains(marker),
            "preview launch must provide {marker} before any child thread can run"
        );
    }
    assert!(
        !script.contains("Process.Start(startInfo)"),
        "preview launch must not create a running process before job assignment"
    );
}

#[test]
fn preview_capture_new_external_reader_tasks_are_joined_on_every_exit() {
    let script = fs::read_to_string(
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("scripts/native-next/Capture-UiPreviews.ps1"),
    )
    .expect("preview capture script");
    for marker in [
        "Join-PreviewReaderTasksBounded",
        "preview.command.reader-join-failed",
        "$stdoutTask = $null",
        "$stderrTask = $null",
        "$Launch.Job.Dispose()",
    ] {
        assert!(
            script.contains(marker),
            "external command cleanup must provide {marker}"
        );
    }
    let dispose_start = script
        .find("public void Dispose()")
        .expect("owned process dispose implementation");
    let dispose = &script[dispose_start..];
    assert!(
        dispose.find("Job.Dispose()") < dispose.find("StandardOutput.Dispose()"),
        "owned job must terminate child writers before closing synchronous reader handles"
    );
}

#[test]
fn preview_capture_new_json_serialization_is_preflight_bounded() {
    let script = fs::read_to_string(
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("scripts/native-next/Capture-UiPreviews.ps1"),
    )
    .expect("preview capture script");
    let preflight = script
        .find("Assert-PreviewJsonValueBounded")
        .expect("JSON graph preflight marker");
    let serialization = script
        .find("$json = $Value | ConvertTo-Json")
        .expect("JSON serialization marker");
    assert!(
        preflight < serialization,
        "bounded JSON preflight must run before ConvertTo-Json allocates the result"
    );
    for marker in ["MAX_PREVIEW_JSON_NODES", "preview JSON value exceeded"] {
        assert!(
            script.contains(marker),
            "JSON preflight must enforce {marker}"
        );
    }
    for serialization in [
        "$Value | ConvertTo-Json",
        "$entries | ConvertTo-Json",
        "$contract | ConvertTo-Json",
        "$receipt | ConvertTo-Json",
    ] {
        let serialization_offset = script
            .find(serialization)
            .unwrap_or_else(|| panic!("JSON serialization marker missing: {serialization}"));
        assert!(
            script[..serialization_offset].contains("Assert-PreviewJsonValueBounded"),
            "JSON preflight must precede {serialization}"
        );
    }
}

#[test]
fn preview_capture_new_unix_unlink_and_drop_cleanup_errors_remain_visible() {
    let source = fs::read_to_string(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/ui/preview_capture.rs"),
    )
    .expect("preview capture source");
    assert!(
        source.contains("unlinkat"),
        "Unix publication must retain handle-relative unlink"
    );
    assert!(
        !source.contains("let _ = unsafe { libc::unlinkat")
            && !source.contains("let _ = self.authority.remove_temp_relative")
            && !source.contains("let _ = self.authority.remove_output_relative")
            && !source.contains("let _ = cleanup_authorized_output_after_deadline"),
        "publication and TempOutput cleanup failures must not be discarded"
    );
    for marker in ["record_cleanup_failure", "O_TMPFILE", "AT_EMPTY_PATH"] {
        assert!(
            source.contains(marker),
            "cleanup failure handling must provide {marker}"
        );
    }
}

#[test]
fn preview_capture_new_external_readers_cancel_and_retain_unresolved_ownership() {
    let script = fs::read_to_string(
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("scripts/native-next/Capture-UiPreviews.ps1"),
    )
    .expect("preview capture script");
    for marker in [
        "CancellationToken",
        "Cancel()",
        "::WhenAll",
        "Join-PreviewProcessWaitTaskBounded",
        "PreviewExternalCleanupLedger",
        "activePreviewProcesses.Add($launch)",
        "ProcessWaitTask = $Launch.ProcessWaitTask",
        "preview.cleanup.ownership-retained",
    ] {
        assert!(
            script.contains(marker),
            "external reader ownership must provide {marker}"
        );
    }
    assert!(
        script.contains("Do not clear unresolved external preview ownership"),
        "failed joins must remain in the visible cleanup ledger"
    );
}

#[test]
fn preview_capture_new_external_join_does_not_cancel_readers_before_natural_exit() {
    let script = fs::read_to_string(
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("scripts/native-next/Capture-UiPreviews.ps1"),
    )
    .expect("preview capture script");
    let start = script
        .find("function Join-PreviewExternalLaunchBounded")
        .expect("external join helper");
    let end = script[start..]
        .find("function Invoke-PreviewExternalCommand")
        .map(|offset| start + offset)
        .expect("external join helper end");
    let join = &script[start..end];
    let process = join
        .find("Join-PreviewProcessBounded")
        .expect("process join");
    let cancel = join
        .find("$Launch.ReaderCancellation.Cancel()")
        .expect("reader cancellation fallback");
    assert!(
        process < cancel,
        "reader cancellation must only be a post-termination fallback so normal stdout receipts can drain"
    );
}

#[test]
fn preview_capture_new_inherited_pipe_descendant_regression_is_job_owned() {
    let script = fs::read_to_string(
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("scripts/native-next/Capture-UiPreviews.ps1"),
    )
    .expect("preview capture script");
    for marker in [
        "CreatePipe",
        "SetHandleInformation",
        "ProcThreadAttributeHandleList",
        "CreateInheritableOutputPipe",
        "AssignProcessToJobObject",
        "ReadBoundedUtf8Async",
    ] {
        assert!(
            script.contains(marker),
            "inherited-pipe descendant regression must provide {marker}"
        );
    }
}

#[test]
fn preview_capture_new_linux_temp_publication_never_unlinks_a_replaced_name() {
    let source = fs::read_to_string(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/ui/preview_capture.rs"),
    )
    .expect("preview capture source");
    for marker in [
        "O_TMPFILE",
        "AT_EMPTY_PATH",
        "atomic_publish_temp_linux_rejects_named_temp_replacement",
        "preview temporary inode identity changed",
    ] {
        assert!(
            source.contains(marker),
            "Linux temp publication must provide {marker}"
        );
    }
    assert!(
        !source.contains("unlinkat(parent.as_raw_fd(), temp_name.as_ptr(), 0)"),
        "Linux publication must never unlink a trusted temp name after it can be replaced"
    );
}

#[test]
fn preview_capture_new_json_inputs_are_bounded_before_materialization() {
    let script = fs::read_to_string(
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("scripts/native-next/Capture-UiPreviews.ps1"),
    )
    .expect("preview capture script");
    for marker in [
        "Read-PreviewJsonBounded",
        "preview.json.lexical-limit",
        "preview.json.depth-limit",
        "preview.json.node-limit",
    ] {
        assert!(script.contains(marker), "JSON input must provide {marker}");
    }
    let helper = script
        .find("function Read-PreviewJsonBounded")
        .expect("bounded JSON input helper");
    for conversion in ["ConvertFrom-Json"] {
        let mut offset = 0;
        while let Some(relative) = script[offset..].find(conversion) {
            let absolute = offset + relative;
            assert!(
                helper < absolute,
                "bounded JSON input helper must precede {conversion}"
            );
            offset = absolute + conversion.len();
        }
    }
    assert!(
        script.contains("Read-PreviewJsonBounded -Text $line")
            && script.contains("Read-PreviewJsonBounded -Text $json")
            && script.contains("Read-PreviewJsonBounded -Text $receiptJson"),
        "fixture, cargo, and receipt JSON must all use the bounded parser"
    );
    assert!(
        script.contains("Get-PreviewJsonLinesBounded -Text $buildResult.Output")
            && !script.contains("$buildResult.Output -split"),
        "cargo JSON lines must stream through bounded preflight without split materialization"
    );
}

#[test]
fn preview_capture_new_final_output_identity_is_retained_through_commit_and_cleanup() {
    let source = fs::read_to_string(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/ui/preview_capture.rs"),
    )
    .expect("preview capture source");
    for marker in [
        "PublishedOutput",
        "final_output_handle",
        "final_output_identity",
        "verify_published_output_identity",
        "delete_published_output_by_handle",
        "output residue is unresolved",
    ] {
        assert!(
            source.contains(marker),
            "final output authority must provide {marker}"
        );
    }
    assert!(
        !source.contains("self.authority.remove_output_relative()"),
        "published output cleanup must not unlink a swapped final name"
    );
}

#[test]
fn preview_capture_new_rust_fixture_json_is_lexically_bounded_before_serde() {
    let source =
        fs::read_to_string(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/ui/preview.rs"))
            .expect("preview source");
    for marker in [
        "validate_fixture_json_bytes",
        "MAX_FIXTURE_JSON_NODES",
        "MAX_FIXTURE_JSON_DEPTH",
        "fixture JSON lexical limit",
    ] {
        assert!(
            source.contains(marker),
            "Rust fixture parser must provide {marker}"
        );
    }
    let scanner = source
        .find("validate_fixture_json_bytes")
        .expect("fixture JSON scanner");
    let serde = source
        .find("serde_json::from_slice(&bytes)")
        .expect("fixture serde parse");
    assert!(
        scanner < serde,
        "fixture lexical bounds must run before serde materialization"
    );
}

#[test]
fn preview_capture_new_deadline_cleanup_requires_retained_published_output_handle() {
    let source = fs::read_to_string(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/ui/preview_capture.rs"),
    )
    .expect("preview capture source");
    let start = source
        .find("pub fn cleanup_output_after_deadline")
        .expect("deadline cleanup helper");
    let end = source[start..]
        .find("fn cleanup_authorized_output_after_deadline")
        .map(|offset| start + offset)
        .expect("authorized cleanup helper");
    let cleanup = &source[start..end];
    assert!(
        source.contains("delete_published_output_by_handle")
            && source.contains("published output handle is unavailable")
            && source.contains("output residue is unresolved"),
        "deadline cleanup must retain the attempt-owned output inode or expose unresolved residue"
    );
    assert!(
        !cleanup.contains("remove_output_relative")
            && !cleanup.contains("remove_name_relative")
            && !cleanup.contains("CaptureOutputAuthority::new"),
        "deadline cleanup must never re-resolve and delete by the final output name"
    );
}

#[test]
fn preview_capture_new_unix_publication_fails_closed_before_final_name_commit_barrier() {
    let source = fs::read_to_string(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/ui/preview_capture.rs"),
    )
    .expect("preview capture source");
    let start = source
        .find("fn atomic_publish_temp_unix")
        .expect("Unix publication helper");
    let end = source[start..]
        .find("\n#[cfg(windows)]\nfn atomic_publish_temp_windows")
        .map(|offset| start + offset)
        .expect("Unix publication helper end");
    let unix_publication = &source[start..end];
    for marker in [
        "final-name publication cannot prove ownership",
        "final-name swap barrier",
        "visual capture HOLD",
        "output residue is unresolved",
    ] {
        assert!(
            source.contains(marker),
            "Unix publication must expose {marker}"
        );
    }
    assert!(
        !unix_publication.contains("Ok(AtomicPublishOutcome::Published)"),
        "Unix publication must not report success after a check-then-commit race"
    );
}

#[test]
fn preview_capture_new_script_output_handle_fences_swap_before_manifest() {
    let script = fs::read_to_string(
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("scripts/native-next/Capture-UiPreviews.ps1"),
    )
    .expect("preview capture script");
    let read_start = script
        .find("OpenReadRelativeNoFollow")
        .expect("relative output reader");
    let read_end = script[read_start..]
        .find("public static Task<string>")
        .map(|offset| read_start + offset)
        .expect("relative output reader end");
    let read_helper = &script[read_start..read_end];
    assert!(
        read_helper.contains("FILE_SHARE_READ")
            && !read_helper.contains("FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE"),
        "output reader must exclude write/delete sharing"
    );
    for marker in [
        "Assert-PreviewOutputAuthorityIdentityBeforeManifest",
        "final-name swap cannot replace retained output handle",
    ] {
        assert!(
            script.contains(marker),
            "output swap fence must provide {marker}"
        );
    }
    let capture = script
        .find("$outputAuthority = Open-PreviewOutputAuthority")
        .expect("regular output authority");
    let barrier = script[capture..]
        .find("Assert-PreviewOutputAuthorityIdentityBeforeManifest")
        .map(|offset| capture + offset)
        .expect("manifest identity barrier");
    let manifest = script[capture..]
        .find("[void]$manifest.Add([pscustomobject]@{")
        .map(|offset| capture + offset)
        .expect("manifest publication");
    assert!(
        barrier < manifest,
        "final-name identity must be revalidated immediately before manifest publication"
    );
}

#[test]
fn preview_capture_new_rust_fixture_scanner_covers_exact_boundary_and_8k_continuation() {
    let source =
        fs::read_to_string(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/ui/preview.rs"))
            .expect("preview source");
    for marker in [
        "fixture_json_exact_256k_boundary",
        "fixture_json_token_continuation_across_8k",
        "MAX_FIXTURE_BYTES",
        "8192",
    ] {
        assert!(
            source.contains(marker),
            "fixture scanner adversary must cover {marker}"
        );
    }
}

#[test]
fn preview_capture_new_deadline_settlement_uses_attempt_retained_output_handle() {
    let source = fs::read_to_string(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/ui/preview_capture.rs"),
    )
    .expect("preview capture source");
    for marker in [
        "retain_published_output",
        "take_published_output",
        "cleanup_retained_published_output",
        "DeadlineExceeded",
    ] {
        assert!(
            source.contains(marker),
            "deadline settlement must provide {marker}"
        );
    }
    let cleanup_start = source
        .find("fn cleanup_authorized_output_after_deadline")
        .expect("authorized cleanup helper");
    let cleanup_end = source[cleanup_start..]
        .find("fn preserve_capture_error_after_authorized_cleanup")
        .map(|offset| cleanup_start + offset)
        .expect("authorized cleanup helper end");
    let cleanup = &source[cleanup_start..cleanup_end];
    assert!(
        cleanup.contains("take_published_output")
            && cleanup.contains("cleanup_retained_published_output"),
        "authorized deadline cleanup must settle the attempt-owned handle"
    );
    assert!(
        !cleanup.contains("let _ = (authority, deadline)"),
        "authorized deadline cleanup must not discard its retained authority"
    );
}

#[test]
fn preview_capture_new_script_requires_child_publication_receipt_before_output_open() {
    let script = fs::read_to_string(
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("scripts/native-next/Capture-UiPreviews.ps1"),
    )
    .expect("preview capture script");
    for marker in [
        "PreviewPublicationReceipt",
        "RedirectOutput",
        "Read-PreviewPublicationReceipt",
        "Assert-PreviewOutputMatchesPublicationReceipt",
    ] {
        assert!(
            script.contains(marker),
            "child-to-script output identity handoff must provide {marker}"
        );
    }
    let start = script
        .find("function Invoke-TrustedPreview")
        .expect("regular preview invocation");
    let end = script[start..]
        .find("function Start-TrustedPreview")
        .map(|offset| start + offset)
        .expect("regular preview invocation end");
    let invoke = &script[start..end];
    assert!(
        invoke.contains("Read-PreviewPublicationReceipt")
            && script.contains("Assert-PreviewOutputMatchesPublicationReceipt"),
        "regular capture must consume and later correlate the current child receipt before trusting output"
    );
    let open = script
        .find("$outputAuthority = Open-PreviewOutputAuthority")
        .expect("regular output authority");
    let receipt = script[open..]
        .find("Assert-PreviewOutputMatchesPublicationReceipt")
        .map(|offset| open + offset)
        .expect("output receipt correlation");
    let manifest = script[open..]
        .find("[void]$manifest.Add([pscustomobject]@{")
        .map(|offset| open + offset)
        .expect("manifest publication");
    assert!(
        receipt < manifest,
        "the retained output handle must match the Rust publication receipt before manifest publication"
    );
}

#[test]
fn preview_capture_new_failed_publication_retains_source_handle_until_cleanup() {
    let source = fs::read_to_string(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/ui/preview_capture.rs"),
    )
    .expect("preview capture source");
    let start = source
        .find("let published = PublishedOutput::from_handle(file)?")
        .expect("published output construction");
    let end = source[start..]
        .find("temp.committed = true")
        .map(|offset| start + offset)
        .expect("publication commit");
    let publication = &source[start..end];
    let retained = publication
        .find("temp.final_output = Some(published)")
        .expect("attempt must retain its source handle");
    let publish = publication
        .find("atomic_publish_temp(")
        .expect("publication operation");
    assert!(
        retained < publish,
        "failed publication cleanup must retain the source handle before the publication operation can fail"
    );
    assert!(
        publication.contains("final_output_identity")
            && source.contains("delete_published_output_by_handle"),
        "failed publication cleanup must retain identity and use handle-owned deletion"
    );
    assert!(
        publication.contains("temp.temp_removed = true")
            && !publication.contains("temp.temp_removed = matches!"),
        "after publication is attempted, cleanup must not re-resolve a replaceable temporary name"
    );
}

#[test]
fn preview_capture_new_publication_transfers_one_cleanup_owner_before_receipt_io() {
    let source = fs::read_to_string(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/ui/preview_capture.rs"),
    )
    .expect("preview capture source");
    let start = source
        .find("let published = PublishedOutput::from_handle(file)?")
        .expect("published output construction");
    let end = source[start..]
        .find("temp.committed = true")
        .map(|offset| start + offset)
        .expect("publication commit");
    let publication = &source[start..end];
    let transfer = publication
        .find("temp.final_output.take()")
        .expect("publication must transfer the exact handle to the authority");
    let retain = publication
        .find("authority.try_retain_published_output")
        .expect("publication owner transfer");
    let receipt = publication
        .find("write_preview_publication_receipt(&authority)")
        .expect("receipt must use the retained publication owner");
    assert!(transfer < retain && retain < receipt);
    assert!(
        !publication.contains(".try_clone()"),
        "receipt publication must not leave a second cleanup owner"
    );
    assert!(
        source.contains("has_retained_published_output"),
        "TempOutput drop must recognize an authority-owned publication"
    );
}

#[test]
fn preview_capture_new_retained_cleanup_uses_bounded_owned_reaper() {
    let source = fs::read_to_string(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/ui/preview_capture.rs"),
    )
    .expect("preview capture source");
    let start = source
        .find("fn cleanup_retained_published_output")
        .expect("retained cleanup helper");
    let end = source[start..]
        .find("/// Deadline cleanup for a validated capture request")
        .map(|offset| start + offset)
        .expect("retained cleanup helper end");
    let cleanup = &source[start..end];
    for marker in [
        "spawn_cleanup_worker",
        "wait_for_worker_result",
        "cleanup reaper",
        "retained published output",
    ] {
        assert!(
            cleanup.contains(marker),
            "bounded cleanup must provide {marker}"
        );
    }
    assert!(!cleanup.contains("let _ = deadline"));
    assert!(
        cleanup.find("spawn_cleanup_worker") < cleanup.find("delete_published_output_by_handle"),
        "the handle delete must run only inside the owned reaper"
    );
}

#[test]
fn preview_script_preserves_primary_failure_and_composes_cleanup_failure() {
    let script = fs::read_to_string(
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("scripts/native-next/Capture-UiPreviews.ps1"),
    )
    .expect("preview capture script");
    for marker in [
        "New-PreviewComposedFailure",
        "PreviewPrimaryCode",
        "PreviewCleanupCode",
        "cleanup failure remains secondary",
    ] {
        assert!(
            script.contains(marker),
            "PowerShell failure composition must provide {marker}"
        );
    }
    let convert = script
        .find("function ConvertTo-PreviewSafeDiagnostic")
        .expect("safe diagnostics helper");
    let invoke = script
        .find("function Invoke-PreviewExternalCommand")
        .expect("external command helper");
    assert!(
        script[convert..invoke].contains("PreviewPrimaryCode")
            && script[convert..invoke].contains("PreviewCleanupCode"),
        "safe diagnostics must retain both fixed primary and cleanup codes"
    );
}

#[test]
fn preview_script_registers_launch_owner_before_pipe_readers_and_makes_join_idempotent() {
    let script = fs::read_to_string(
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("scripts/native-next/Capture-UiPreviews.ps1"),
    )
    .expect("preview capture script");
    let start = script
        .find("function Start-TrustedPreview")
        .expect("trusted launch helper");
    let end = script[start..]
        .find("function Get-PngDimensions")
        .map(|offset| start + offset)
        .expect("trusted launch helper end");
    let launch = &script[start..end];
    assert!(
        launch.contains("CleanupOwner = 'active-preview-process'")
            && launch.contains("activePreviewProcesses.Add($launch)"),
        "launch ownership must be established before reader setup"
    );
    assert!(
        launch.find("activePreviewProcesses.Add($launch)")
            < launch
                .find("$launch.StdoutTask =")
                .expect("stdout reader setup"),
        "reader setup failures must remain owned by the active launch"
    );
    let join = script
        .find("function Join-PreviewExternalLaunchBounded")
        .expect("external join helper");
    assert!(
        script[join..].contains("Test-PreviewLaunchSettled")
            && script[join..].contains("CleanupOwner = 'released'"),
        "a second cleanup call must not dispose the same job/readers twice"
    );
}

#[test]
fn preview_script_minimized_transition_requires_is_iconic_readback() {
    let script = fs::read_to_string(
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("scripts/native-next/Capture-UiPreviews.ps1"),
    )
    .expect("preview capture script");
    for marker in [
        "IsIconic",
        "Wait-PreviewWindowMinimized",
        "minimize-readback-unconfirmed",
        "stateTransitionApplied = $true",
    ] {
        assert!(
            script.contains(marker),
            "minimized transition must provide {marker}"
        );
    }
    let wait = script
        .find("function Wait-PreviewWindowMinimized")
        .expect("bounded minimized readback helper");
    let applied = script
        .find("$stateTransitionApplied = $true")
        .expect("minimized state publication marker");
    assert!(
        wait < applied,
        "stateTransitionApplied must only follow the bounded minimized readback helper"
    );
}

#[cfg(windows)]
#[test]
fn preview_script_rejects_deep_high_node_and_oversize_json_fixtures_before_parse() {
    use base64::Engine as _;

    let script_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("scripts/native-next/Capture-UiPreviews.ps1");
    let script_literal = script_path.to_string_lossy().replace('\'', "''");
    let command = r#"
$scriptPath = '__SCRIPT_PATH__'
$source = [IO.File]::ReadAllText($scriptPath)
$tokens = $null
$parseErrors = $null
$ast = [Management.Automation.Language.Parser]::ParseInput($source, [ref]$tokens, [ref]$parseErrors)
$wanted = @('Assert-PreviewJsonLexicalBounded', 'Read-PreviewJsonBounded')
$definitions = @($ast.FindAll({
    param($node)
    $node -is [Management.Automation.Language.FunctionDefinitionAst] -and $wanted -contains $node.Name
}, $true))
if ($definitions.Count -ne $wanted.Count) { throw 'preview-json-test-functions-missing' }
foreach ($definition in $definitions) {
    . ([scriptblock]::Create($definition.Extent.Text))
}
$MAX_PREVIEW_JSON_NODES = 16
$deep = ((('[' * 12) -join '') + '0' + ((']' * 12) -join ''))
$high = '[' + ((('0,' * 32) -join '') + '0') + ']'
$oversize = '{"value":"' + ('x' * 256) + '"}'
$cases = @(
    [pscustomobject]@{ Text = $deep; MaxBytes = 1024; MaxNodes = 100; MaxDepth = 8; Code = 'preview.json.depth-limit' }
    [pscustomobject]@{ Text = $high; MaxBytes = 1024; MaxNodes = 8; MaxDepth = 32; Code = 'preview.json.node-limit' }
    [pscustomobject]@{ Text = $oversize; MaxBytes = 32; MaxNodes = 100; MaxDepth = 8; Code = 'preview.json.lexical-limit' }
)
foreach ($case in $cases) {
    try {
        Read-PreviewJsonBounded -Text $case.Text -MaxBytes $case.MaxBytes -MaxNodes $case.MaxNodes -MaxDepth $case.MaxDepth | Out-Null
        throw "accepted:$($case.Code)"
    } catch {
        if ($_.Exception.Message -ne $case.Code) { throw }
    }
}
Write-Output 'preview-json-adversarial-fixtures-rejected'
"#.replace("__SCRIPT_PATH__", &script_literal);
    let mut utf16 = Vec::with_capacity(command.len() * 2);
    for unit in command.encode_utf16() {
        utf16.extend_from_slice(&unit.to_le_bytes());
    }
    let encoded = base64::engine::general_purpose::STANDARD.encode(utf16);
    let output = Command::new("pwsh")
        .args(["-NoLogo", "-NoProfile", "-NonInteractive"])
        .arg("-EncodedCommand")
        .arg(encoded)
        .output()
        .expect("PowerShell must run JSON adversarial fixtures");
    assert!(
        output.status.success(),
        "bounded JSON fixtures must fail closed:\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stdout)
            .contains("preview-json-adversarial-fixtures-rejected"),
        "PowerShell fixture probe must report all rejection cases"
    );
}

#[test]
fn preview_external_io_is_stream_bounded_and_uses_one_absolute_deadline() {
    let script = fs::read_to_string(
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("scripts/native-next/Capture-UiPreviews.ps1"),
    )
    .expect("preview capture script");
    for marker in [
        "ReadBoundedUtf8Async",
        "ReadAsync",
        "WaitForExitAsync",
        "Get-PreviewRemainingMilliseconds",
        "Wait-PreviewBackoff",
    ] {
        assert!(
            script.contains(marker),
            "bounded process I/O needs {marker}"
        );
    }
    for forbidden in ["ReadToEndAsync", "WaitForExit(1000)", "Start-Sleep"] {
        assert!(
            !script.contains(forbidden),
            "process I/O must not retain unbounded/fixed wait {forbidden}"
        );
    }
}

#[test]
fn preview_diagnostics_are_fixed_code_redacted_and_child_environment_is_allowlisted() {
    let script = fs::read_to_string(
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("scripts/native-next/Capture-UiPreviews.ps1"),
    )
    .expect("preview capture script");
    for marker in [
        "ConvertTo-PreviewSafeDiagnostic",
        "preview.command.exit-nonzero",
        "Environment.Clear",
        "PreviewEnvironmentAllowlist",
    ] {
        assert!(
            script.contains(marker),
            "diagnostics/environment need {marker}"
        );
    }
    for forbidden in [
        "Error = $_.Exception.Message",
        "Error = $stderr",
        "$FilePath $stderr",
    ] {
        assert!(
            !script.contains(forbidden),
            "diagnostics must not expose attacker-controlled data: {forbidden}"
        );
    }
}

#[test]
fn preview_fixture_enumeration_streams_before_cap_then_sorts_the_bounded_set() {
    let script = fs::read_to_string(
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("scripts/native-next/Capture-UiPreviews.ps1"),
    )
    .expect("preview capture script");
    for marker in [
        "Get-PreviewFixtureFilesBounded",
        "MAX_SOURCE_DIGEST_FILES",
        "Sort-Object -Property Name",
        "preview.fixture.enumeration-failed",
    ] {
        assert!(
            script.contains(marker),
            "fixture enumeration needs {marker}"
        );
    }
    assert!(
        !script.contains("foreach ($candidate in (Get-ChildItem"),
        "fixture enumeration must not materialize an unbounded provider result"
    );
}

#[test]
fn minimized_probe_is_deferred_when_the_window_does_not_support_minimize() {
    let script = fs::read_to_string(
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("scripts/native-next/Capture-UiPreviews.ps1"),
    )
    .expect("preview capture script");
    for marker in [
        "CanMinimize",
        "WS_MINIMIZEBOX",
        "window-not-minimizable",
        "Outcome = 'deferred'",
    ] {
        assert!(
            script.contains(marker),
            "minimized proof must expose {marker}"
        );
    }
    assert!(
        script.contains("if ([DevManagerPreviewWindow]::CanMinimize($window))"),
        "ShowWindow(SW_MINIMIZE) must only run after a truthful capability check"
    );
}

#[test]
fn preview_script_joins_every_capture_process_before_manifest_publication() {
    let script = fs::read_to_string(
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("scripts/native-next/Capture-UiPreviews.ps1"),
    )
    .expect("preview capture script");
    for marker in [
        "$activePreviewProcesses",
        "Join-PreviewProcessBounded",
        "Assert-NoLivePreviewProcesses",
        "before manifest publication",
    ] {
        assert!(
            script.contains(marker),
            "regular and probe captures must be killed and joined {marker}"
        );
    }
}

#[test]
fn preview_script_retries_until_window_readiness_handshake() {
    let script = fs::read_to_string(
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("scripts/native-next/Capture-UiPreviews.ps1"),
    )
    .expect("preview capture script");
    for marker in [
        "Wait-PreviewWindowReady",
        "ReadinessAttempt",
        "readiness-retry",
        "ReadinessHandshake",
    ] {
        assert!(
            script.contains(marker),
            "window state capture must use a bounded readiness handshake/retry: {marker}"
        );
    }
}

#[test]
fn preview_script_enumerates_fixtures_sorted_and_fails_visible_on_invalid_input() {
    let script = fs::read_to_string(
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("scripts/native-next/Capture-UiPreviews.ps1"),
    )
    .expect("preview capture script");
    for marker in [
        "Sort-Object -Property Name",
        "FixtureRecord",
        "preview.fixture.enumeration-failed",
        "preview.fixture.unsupported-schema",
    ] {
        assert!(
            script.contains(marker),
            "fixture discovery must be sorted and fail-visible: {marker}"
        );
    }
}

#[test]
fn preview_script_retains_output_authority_through_validation_and_manifest_publication() {
    let script = fs::read_to_string(
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("scripts/native-next/Capture-UiPreviews.ps1"),
    )
    .expect("preview capture script");
    for marker in [
        "Open-PreviewOutputAuthority",
        "Assert-PreviewOutputAuthorityStable",
        "retainedOutputAuthorities",
        "outputRootAuthority",
        "Assert-PreviewDirectoryAuthorityStable",
        "Write-PreviewAtomicJson",
        "manifestSha256",
        "OutputSha256",
    ] {
        assert!(
            script.contains(marker),
            "capture validation/publication must retain and recheck {marker}"
        );
    }
    for forbidden in [
        "Get-Item -LiteralPath $output",
        "Get-Item -LiteralPath $output -ErrorAction Stop",
        "File.Exists($Path)",
        "Set-Content -LiteralPath $manifestPath",
    ] {
        assert!(
            !script.contains(forbidden),
            "capture publication must not re-resolve output paths through {forbidden}"
        );
    }
}

#[test]
fn preview_script_rejects_tool_wrappers_and_target_specific_build_overrides() {
    let script = fs::read_to_string(
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("scripts/native-next/Capture-UiPreviews.ps1"),
    )
    .expect("preview capture script");
    for marker in [
        "RUSTC",
        "RUSTC_WRAPPER",
        "RUSTC_WORKSPACE_WRAPPER",
        "RUSTC_BOOTSTRAP",
        "RUSTDOCFLAGS",
        "CARGO_BUILD_RUSTC",
        "CARGO_BUILD_RUSTC_WRAPPER",
        "CARGO_TARGET_.+_(RUSTFLAGS|RUSTC|LINKER)",
        "Open-PreviewToolAuthority",
        "ToolAuthorities",
    ] {
        assert!(
            script.contains(marker),
            "cold-build provenance must fence {marker}"
        );
    }
}

#[cfg(windows)]
fn run_preview_artifact_validation(binary: &std::path::Path) -> std::process::Output {
    let script = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("scripts/native-next/Capture-UiPreviews.ps1");
    let output_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join(".devmanager-next/evidence/phase-05/screenshots");
    let target_dir = binary
        .parent()
        .expect("temporary binary parent")
        .join("isolated-target");
    Command::new("pwsh")
        .args([
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-File",
        ])
        .arg(script)
        .args(["-ValidateOnly", "-BinaryPath"])
        .arg(binary)
        .args(["-TargetDir"])
        .arg(target_dir)
        .args(["-OutputRoot"])
        .arg(output_root)
        .output()
        .expect("PowerShell must run the preview artifact validator")
}

#[cfg(windows)]
fn run_preview_windows_runtime_probe(body: &str) -> std::process::Output {
    use base64::Engine as _;

    let script_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("scripts/native-next/Capture-UiPreviews.ps1");
    let script_literal = script_path.to_string_lossy().replace('\'', "''");
    let command = format!(
        r#"
$scriptPath = '{script_literal}'
$source = [IO.File]::ReadAllText($scriptPath)
$nativeStart = $source.IndexOf('using System;', [StringComparison]::Ordinal)
$nativeEnd = $source.IndexOf("'@", $nativeStart, [StringComparison]::Ordinal)
if ($nativeStart -lt 0 -or $nativeEnd -le $nativeStart) {{ throw 'preview-native-test-source-missing' }}
Add-Type -TypeDefinition $source.Substring($nativeStart, $nativeEnd - $nativeStart)
$MAX_PREVIEW_RECEIPT_BYTES = 1048576
$MAX_PREVIEW_PNG_BYTES = 134217728
$PREVIEW_HASH_CHUNK_BYTES = 65536
$PREVIEW_IO_DEADLINE_SECONDS = 30
$PreviewEnvironmentAllowlist = @('SystemRoot', 'WINDIR', 'PATH', 'TEMP', 'TMP', 'USERPROFILE')
$tokens = $null
$parseErrors = $null
$ast = [Management.Automation.Language.Parser]::ParseInput($source, [ref]$tokens, [ref]$parseErrors)
$wanted = @(
    'Assert-PreviewDeadline',
    'Assert-PreviewDirectoryAuthorityStable',
    'Close-PreviewDirectoryAuthorityChain',
    'Get-PreviewDirectoryChain',
    'New-PreviewProcessStartInfo',
    'Open-PreviewArtifactRelative',
    'Open-PreviewDirectoryAuthorityChain',
    'Open-PreviewDirectoryNoFollow',
    'Open-PreviewOutputAuthority',
    'Read-PreviewPublicationReceipt',
    'Assert-PreviewOutputMatchesPublicationReceipt'
)
$definitions = @($ast.FindAll({
    param($node)
    $node -is [Management.Automation.Language.FunctionDefinitionAst] -and $wanted -contains $node.Name
}, $true))
if ($definitions.Count -ne $wanted.Count) {{ throw 'preview-runtime-test-functions-missing' }}
foreach ($name in $wanted) {{
    $definition = $definitions | Where-Object Name -eq $name | Select-Object -First 1
    . ([scriptblock]::Create($definition.Extent.Text))
}}
{body}
"#,
        script_literal = script_literal,
        body = body
    );
    let mut utf16 = Vec::with_capacity(command.len() * 2);
    for unit in command.encode_utf16() {
        utf16.extend_from_slice(&unit.to_le_bytes());
    }
    let encoded = base64::engine::general_purpose::STANDARD.encode(utf16);
    Command::new("pwsh")
        .args(["-NoLogo", "-NoProfile", "-NonInteractive"])
        .arg("-EncodedCommand")
        .arg(encoded)
        .output()
        .expect("PowerShell must run the Windows preview runtime probe")
}

#[cfg(windows)]
#[test]
fn preview_script_runtime_rejects_receipt_failure_mismatch_and_final_name_swap() {
    let output = run_preview_windows_runtime_probe(
        r#"
function Expect-PreviewRuntimeCode {
    param([scriptblock]$Action, [string]$Code)
    try {
        & $Action | Out-Null
        throw "accepted:$Code"
    } catch {
        if ($_.Exception.Message -ne $Code) { throw }
    }
}

Expect-PreviewRuntimeCode {
    Read-PreviewPublicationReceipt -Text ''
} 'preview.publication-receipt-missing-or-oversized'
Expect-PreviewRuntimeCode {
    Read-PreviewPublicationReceipt -Text @(
        'DEV_MANAGER_PREVIEW_PUBLICATION_RECEIPT_V1 identity=00000000:0000000000000001',
        'DEV_MANAGER_PREVIEW_PUBLICATION_RECEIPT_V1 identity=00000000:0000000000000002'
    ) -join "`n"
} 'preview.publication-receipt-missing-or-ambiguous'

$root = Join-Path ([IO.Path]::GetTempPath()) ('devmanager-preview-receipt-' + [Guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $root -Force | Out-Null
$directory = $null
$authority = $null
try {
    $directory = Open-PreviewDirectoryAuthorityChain -Path $root
    $outputPath = Join-Path $root 'capture.png'
    [IO.File]::WriteAllBytes($outputPath, [byte[]](1, 2, 3, 4))
    $authority = Open-PreviewOutputAuthority -Path $outputPath -ParentAuthority $directory -Deadline ([DateTime]::UtcNow.AddSeconds(5))
    $identity = ([DevManagerPreviewArtifactNative]::Identity($authority.Stream.SafeFileHandle)).ToLowerInvariant()
    $valid = Read-PreviewPublicationReceipt -Text "DEV_MANAGER_PREVIEW_PUBLICATION_RECEIPT_V1 identity=$identity`n"
    Assert-PreviewOutputMatchesPublicationReceipt -Authority $authority -Receipt $valid -Deadline ([DateTime]::UtcNow.AddSeconds(5))
    $mismatch = [pscustomobject]@{ Identity = '00000000:0000000000000000' }
    Expect-PreviewRuntimeCode {
        Assert-PreviewOutputMatchesPublicationReceipt -Authority $authority -Receipt $mismatch -Deadline ([DateTime]::UtcNow.AddSeconds(5))
    } 'preview.output.publication-identity-mismatch'

    $replacement = Join-Path $root 'replacement.png'
    [IO.File]::WriteAllBytes($replacement, [byte[]](5, 6, 7, 8))
    $swapBlocked = $false
    try {
        [IO.File]::Replace($replacement, $outputPath, $null)
    } catch {
        $swapBlocked = $true
    }
    if (-not $swapBlocked) {
        Expect-PreviewRuntimeCode {
            Assert-PreviewOutputMatchesPublicationReceipt -Authority $authority -Receipt $valid -Deadline ([DateTime]::UtcNow.AddSeconds(5))
        } 'preview.output.final-name-identity-changed'
    }
    Write-Output "preview-receipt-runtime-ok swapBlocked=$swapBlocked"
} finally {
    if ($null -ne $authority -and $null -ne $authority.Stream) { $authority.Stream.Dispose() }
    if ($null -ne $directory) { Close-PreviewDirectoryAuthorityChain -Authority $directory }
    Remove-Item -LiteralPath $root -Recurse -Force -ErrorAction SilentlyContinue
}
"#,
    );
    assert!(
        output.status.success(),
        "receipt runtime authority checks must fail closed:\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("preview-receipt-runtime-ok"),
        "receipt runtime sentinel missing: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[cfg(windows)]
#[test]
fn preview_script_runtime_cancels_and_joins_inherited_pipe_timeout() {
    let output = run_preview_windows_runtime_probe(
        r#"
$startInfo = [Diagnostics.ProcessStartInfo]::new()
$startInfo.FileName = (Get-Command pwsh -ErrorAction Stop).Source
$startInfo.UseShellExecute = $false
$startInfo.CreateNoWindow = $true
$startInfo.RedirectStandardOutput = $true
$startInfo.RedirectStandardError = $true
[void]$startInfo.ArgumentList.Add('-NoLogo')
[void]$startInfo.ArgumentList.Add('-NoProfile')
[void]$startInfo.ArgumentList.Add('-NonInteractive')
[void]$startInfo.ArgumentList.Add('-Command')
[void]$startInfo.ArgumentList.Add("[Console]::Write('inherited-timeout'); Start-Sleep -Seconds 30")
$startInfo.Environment.Clear()
foreach ($name in @('SystemRoot', 'WINDIR', 'PATH', 'TEMP', 'TMP', 'USERPROFILE')) {
    $value = [Environment]::GetEnvironmentVariable($name)
    if ($null -ne $value) { $startInfo.Environment[$name] = $value }
}
$owned = $null
$cancellation = [Threading.CancellationTokenSource]::new()
$stdoutTask = $null
$stderrTask = $null
try {
    $owned = [DevManagerPreviewArtifactNative]::StartProcessInJob($startInfo)
    $childPid = $owned.Process.Id
    $stdoutTask = [DevManagerPreviewArtifactNative]::ReadBoundedUtf8Async($owned.StandardOutput, 1048576, $cancellation.Token)
    $stderrTask = [DevManagerPreviewArtifactNative]::ReadBoundedUtf8Async($owned.StandardError, 1048576, $cancellation.Token)
    $deadline = [DateTime]::UtcNow.AddSeconds(3)
    while (-not $owned.Process.HasExited -and [DateTime]::UtcNow -lt $deadline) {
        Start-Sleep -Milliseconds 10
    }
    $cancellation.Cancel()
    $owned.Terminate()
    while (-not $owned.Process.HasExited -and [DateTime]::UtcNow -lt $deadline) {
        Start-Sleep -Milliseconds 10
    }
    if (-not $owned.Process.HasExited) { throw 'preview.runtime.process-join-deadline' }
    $whenAll = [Threading.Tasks.Task]::WhenAll([Threading.Tasks.Task[]]@($stdoutTask, $stderrTask))
    $remaining = [Math]::Max(1, [int][Math]::Ceiling(($deadline - [DateTime]::UtcNow).TotalMilliseconds))
    $readerJoined = $false
    try { $readerJoined = $whenAll.Wait($remaining) } catch { $readerJoined = $whenAll.IsCompleted }
    if (-not $readerJoined) { throw 'preview.runtime.reader-join-deadline' }
    try { [void]$whenAll.GetAwaiter().GetResult() } catch { }
    $owned.Dispose()
    $owned = $null
    if ($null -ne (Get-Process -Id $childPid -ErrorAction SilentlyContinue)) {
        throw 'preview.runtime.job-descendant-remains'
    }
    Write-Output 'preview-inherited-pipe-timeout-runtime-ok'
} finally {
    try { $cancellation.Cancel() } catch { }
    if ($null -ne $owned) {
        try { if (-not $owned.Process.HasExited) { $owned.Terminate() } } catch { }
        try { $owned.Dispose() } catch { }
    }
    $cancellation.Dispose()
}
"#,
    );
    assert!(
        output.status.success(),
        "inherited pipe timeout must cancel, terminate, and join:\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stdout)
            .contains("preview-inherited-pipe-timeout-runtime-ok"),
        "pipe timeout runtime sentinel missing: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[cfg(windows)]
#[test]
fn preview_script_rejects_an_arbitrary_rust_harness_before_launch() {
    let root = tempdir().expect("arbitrary harness root");
    let renamed_harness = root.path().join("devmanager-next.exe");
    fs::copy(
        std::env::current_exe().expect("current Rust harness"),
        &renamed_harness,
    )
    .expect("copy Rust harness under the canonical-looking name");

    let result = run_preview_artifact_validation(&renamed_harness);
    let combined = format!(
        "{}\n{}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(
        !result.status.success(),
        "arbitrary harness was accepted: {combined}"
    );
    assert!(
        combined.contains("preview artifact identity"),
        "rejection must identify the provenance failure: {combined}"
    );
    assert!(
        !combined.contains("Unrecognized option"),
        "the Rust harness must not be launched before provenance rejection: {combined}"
    );
}

#[cfg(windows)]
#[test]
fn preview_script_rejects_a_renamed_unrelated_exe_before_launch() {
    let root = tempdir().expect("unrelated executable root");
    let unrelated = std::env::var_os("SystemRoot")
        .map(PathBuf::from)
        .expect("SystemRoot")
        .join("System32/where.exe");
    assert!(
        unrelated.is_file(),
        "Windows where.exe must exist for this probe"
    );
    let renamed_unrelated = root.path().join("devmanager-next.exe");
    fs::copy(&unrelated, &renamed_unrelated).expect("copy unrelated executable");

    let result = run_preview_artifact_validation(&renamed_unrelated);
    let combined = format!(
        "{}\n{}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(
        !result.status.success(),
        "renamed unrelated EXE was accepted: {combined}"
    );
    assert!(
        combined.contains("preview artifact identity"),
        "rejection must identify the provenance failure: {combined}"
    );
}

#[cfg(windows)]
#[test]
fn preview_script_rejects_cross_invocation_receipt_even_when_path_matches() {
    let receipt_path =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(".devmanager-next/preview-artifact.json");
    let receipt: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(receipt_path).expect("existing preview receipt"))
            .expect("preview receipt JSON");
    let binary = PathBuf::from(receipt["binaryPath"].as_str().expect("receipt binary path"));
    let result = run_preview_artifact_validation(&binary);
    let combined = format!(
        "{}\n{}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(
        !result.status.success(),
        "cross-invocation receipt was accepted"
    );
    assert!(
        combined.contains("caller-supplied warm binary paths are disabled"),
        "warm receipt trust must be disabled even for a matching path: {combined}"
    );
}

#[cfg(windows)]
#[test]
fn preview_script_rejects_a_wrong_cwd_before_any_build_or_launch() {
    let root = tempdir().expect("wrong cwd root");
    let script = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("scripts/native-next/Capture-UiPreviews.ps1");
    let result = Command::new("pwsh")
        .current_dir(root.path())
        .args([
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-File",
        ])
        .arg(script)
        .args(["-ValidateOnly"])
        .output()
        .expect("PowerShell must run the preview validator");
    let combined = format!(
        "{}\n{}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(
        !result.status.success(),
        "wrong cwd was accepted: {combined}"
    );
    assert!(
        combined.contains("canonical worktree"),
        "wrong cwd must fail closed before Cargo: {combined}"
    );
}

#[test]
fn preview_script_binds_every_warm_launch_to_the_fixed_artifact_receipt() {
    let script = fs::read_to_string(
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("scripts/native-next/Capture-UiPreviews.ps1"),
    )
    .expect("preview capture script");
    for marker in [
        "preview-artifact.json",
        "Get-PreviewSourceRevision",
        "buildContract",
        "binaryFileIdentity",
        "binarySha256",
        "OpenReparsePoint",
        "Open-PreviewArtifactNoFollow",
        "Assert-PreviewArtifactIdentity",
        "Invoke-TrustedPreview",
        "Start-TrustedPreview",
        "ValidateOnly",
        "ShareRead",
    ] {
        assert!(
            script.contains(marker),
            "warm preview launches must enforce {marker}"
        );
    }
    assert!(
        !script.contains("StartsWith($repoPrefix"),
        "a lexical worktree prefix must not be the warm-binary authority"
    );
}

#[test]
fn preview_script_disables_cross_invocation_warm_receipt_trust() {
    let script = fs::read_to_string(
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("scripts/native-next/Capture-UiPreviews.ps1"),
    )
    .expect("preview capture script");
    assert!(
        script.contains("caller-supplied warm binary paths are disabled"),
        "warm mode must build and retain one trusted artifact per invocation"
    );
    assert!(
        script.contains("InMemoryArtifactReceipt") || script.contains("artifactReceipt ="),
        "matrix launches must use the receipt minted by the current invocation"
    );
}

#[test]
fn preview_script_derives_and_rechecks_a_clean_source_build_digest() {
    let script = fs::read_to_string(
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("scripts/native-next/Capture-UiPreviews.ps1"),
    )
    .expect("preview capture script");
    for marker in [
        "git status --porcelain",
        "HEAD^{tree}",
        "Get-PreviewSourceContentDigest",
        "sourceContentDigest",
        "MAX_SOURCE_DIGEST_FILES",
        "MAX_SOURCE_DIGEST_DIRECTORIES",
        "MAX_SOURCE_DIGEST_BYTES",
        "BuildIdentityDigest",
        "source tree changed during the isolated build",
    ] {
        assert!(
            script.contains(marker),
            "source/build identity must derive and recheck {marker}"
        );
    }
}

#[test]
fn preview_script_pins_cargo_manifest_target_profile_and_features() {
    let script = fs::read_to_string(
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("scripts/native-next/Capture-UiPreviews.ps1"),
    )
    .expect("preview capture script");
    for marker in [
        "--manifest-path",
        "--target",
        "--profile",
        "--no-default-features",
        "--message-format=json-render-diagnostics",
        "features = @()",
    ] {
        assert!(
            script.contains(marker),
            "isolated cargo build must pin {marker}"
        );
    }
}

#[test]
fn preview_script_retains_no_follow_directory_authority_through_launch() {
    let script = fs::read_to_string(
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("scripts/native-next/Capture-UiPreviews.ps1"),
    )
    .expect("preview capture script");
    for marker in [
        "OpenDirectoryNoFollow",
        "FILE_SHARE_NONE",
        "Open-PreviewLaunchAuthority",
        "Assert-PreviewLaunchAuthorityStable",
        "Authority = $authority",
    ] {
        assert!(
            script.contains(marker),
            "every launch must retain and recheck {marker}"
        );
    }
    assert!(
        !script.contains("& $opened.Path")
            && !script.contains("Start-Process -FilePath $opened.Path"),
        "launch must not rely on an unprotected path after validation"
    );
}

#[test]
fn preview_receipt_publication_is_atomic_and_handle_verified() {
    let script = fs::read_to_string(
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("scripts/native-next/Capture-UiPreviews.ps1"),
    )
    .expect("preview capture script");
    for marker in [
        "WriteAtomicPreviewReceiptRelative",
        "SetFileInformationByHandle",
        "RootDirectory",
        "FlushFileBuffers",
        "receipt handle remains held",
        "Assert-PreviewReceiptSchema",
        "embeddedBuildIdentity",
        "BuildIdentityDigest",
    ] {
        assert!(
            script.contains(marker),
            "receipt publication must provide {marker}"
        );
    }
    assert!(
        !script.contains("MoveFileExW") && !script.contains("DeleteFileW"),
        "receipt publication must not retain a path-based move or cleanup fallback"
    );
}

#[test]
fn preview_forged_receipts_and_caller_build_overrides_cannot_authorize_warm_launch() {
    let script = fs::read_to_string(
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("scripts/native-next/Capture-UiPreviews.ps1"),
    )
    .expect("preview capture script");
    for marker in [
        "caller-supplied warm binary paths are disabled",
        "preview.identity.caller-build-override",
        "Assert-PreviewReceiptSchema",
        "binaryFileIdentity",
        "binarySha256",
        "Read-PreviewArtifactReceipt",
        "Open-PreviewDirectoryNoFollow",
        "Get-PreviewDirectoryChain",
        "Get-PreviewToolPath",
        "cargoSha256",
        "rustcSha256",
        "CargoPath",
        "rustup which",
        "canonicalRustupPath",
        "globalCargoConfigSha256",
    ] {
        assert!(
            script.contains(marker),
            "untrusted receipt/build metadata must fail closed at {marker}"
        );
    }
}

#[test]
fn preview_binary_exposes_a_build_identity_marker_without_runtime_execution() {
    let build_script =
        fs::read_to_string(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("build.rs"))
            .expect("build script");
    let preview_source =
        fs::read_to_string(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/ui/preview.rs"))
            .expect("preview source");
    assert!(
        build_script.contains("DEV_MANAGER_PREVIEW_BUILD_IDENTITY"),
        "build identity must be injected by the trusted build"
    );
    assert!(
        preview_source.contains("DEV_MANAGER_PREVIEW_BUILD_IDENTITY_MARKER"),
        "the executable must expose the build identity as readable bytes"
    );
}

#[test]
fn preview_cli_raii_shutdown_joins_capture_executor_on_every_exit() {
    let source =
        fs::read_to_string(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/ui/preview.rs"))
            .expect("preview source");
    for marker in [
        "CaptureExecutorShutdownGuard",
        "shutdown_capture_executor",
        "finish()",
        "run_cli",
    ] {
        assert!(
            source.contains(marker),
            "preview CLI must route every success/failure exit through {marker}"
        );
    }
    assert!(
        source.contains("workers_leaked") && source.contains("CaptureCleanupFailed"),
        "executor shutdown leaks must remain typed and visible rather than detached"
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
    for deferred in [
        "os-monitor-dpi-deferred",
        "external-desktop-occlusion-deferred",
        "disposable-vm-required-for-physical-monitor-dpi",
        "disposable-vm-required-for-external-occlusion-race",
    ] {
        assert!(
            script.contains(deferred),
            "unavailable physical matrix evidence must remain visibly deferred: {deferred}"
        );
    }
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
    let _capture_guard = capture_test_guard();
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
    let _capture_guard = capture_test_guard();
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
    let _capture_guard = capture_test_guard();
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

#[cfg(windows)]
#[test]
fn output_cleanup_refuses_a_parent_junction_substitution() {
    let _capture_guard = capture_test_guard();
    let (_root, policy) = temporary_policy();
    let parent = policy.output_root().join("cleanup-parent");
    fs::create_dir_all(&parent).expect("cleanup parent");
    let output = parent.join("cleanup.png");
    fs::write(&output, b"must remain in the moved directory").expect("cleanup output");
    let moved_parent = policy.output_root().join("cleanup-parent-before-swap");
    fs::rename(&parent, &moved_parent).expect("move cleanup parent");
    create_directory_junction(&moved_parent, &parent);

    let error = cleanup_output_after_deadline(
        &output,
        PreviewCaptureError::DeadlineExceeded,
        CaptureDeadline::from_now(Duration::ZERO),
    );
    assert!(matches!(
        error,
        PreviewCaptureError::CleanupFailed(context)
            if matches!(context.secondary(), PreviewCaptureError::OutputFailed(_))
    ));
    assert!(
        moved_parent.join("cleanup.png").exists(),
        "junction substitution must not delete the moved output"
    );

    remove_directory_junction(&parent);
    fs::rename(&moved_parent, &parent).expect("restore cleanup parent");
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
    let _capture_guard = capture_test_guard();
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
    let _capture_guard = capture_test_guard();
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
    let _capture_guard = capture_test_guard();
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
    let _capture_guard = capture_test_guard();
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
    let _capture_guard = capture_test_guard();
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

#[cfg(windows)]
#[test]
fn late_output_cleanup_is_owned_and_leaves_no_residue() {
    let _capture_guard = capture_test_guard();
    let (_root, policy) = temporary_policy();
    let output = policy.output_root().join("late-output.png");
    fs::write(&output, b"late output").expect("late output fixture");
    let authority = Arc::new(
        CaptureOutputAuthority::new(&output, policy.output_root())
            .expect("output authority should open"),
    );
    let published = PublishedOutput::from_handle_for_authority(
        open_retained_output_handle(&output),
        &authority,
    )
    .expect("published output handle should bind to authority");
    authority
        .retain_published_output(published)
        .expect("published output cleanup ownership");

    let error = settle_capture_result_with_authority(
        authority,
        Err(PreviewCaptureError::DeadlineExceeded),
        CaptureDeadline::from_now(Duration::ZERO),
    )
    .expect_err("an expired attempt must preserve its primary deadline error");

    let primary_is_deadline = match &error {
        PreviewCaptureError::DeadlineExceeded => true,
        PreviewCaptureError::CleanupFailed(context) => {
            matches!(context.primary(), PreviewCaptureError::DeadlineExceeded)
        }
        _ => false,
    };
    assert!(
        primary_is_deadline,
        "deadline error was not preserved: {error}"
    );
    let cleanup_deadline = Instant::now() + Duration::from_secs(2);
    while output.exists() && Instant::now() < cleanup_deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(
        !output.exists(),
        "retained handle cleanup left a published file"
    );
}

#[cfg(not(windows))]
#[test]
fn late_output_cleanup_without_retained_handle_is_explicitly_unresolved() {
    let _capture_guard = capture_test_guard();
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
    assert!(
        output.exists(),
        "path-only cleanup must leave unresolved residue visible"
    );
}

#[cfg(windows)]
#[test]
fn final_capture_settlement_fences_a_late_success_and_cleans_output() {
    let _capture_guard = capture_test_guard();
    let (_root, policy) = temporary_policy();
    let output = policy.output_root().join("late-success.png");
    fs::write(&output, b"late success").expect("late output fixture");
    let authority = Arc::new(
        CaptureOutputAuthority::new(&output, policy.output_root())
            .expect("output authority should open"),
    );
    let published = PublishedOutput::from_handle_for_authority(
        open_retained_output_handle(&output),
        &authority,
    )
    .expect("published output handle should bind to authority");
    authority
        .retain_published_output(published)
        .expect("published output cleanup ownership");
    let report = CaptureReport {
        width: 1,
        height: 1,
        foreground_before: 1,
        foreground_after: 1,
    };

    let deadline = CaptureDeadline::from_now(Duration::from_millis(1));
    std::thread::sleep(Duration::from_millis(5));
    let error = settle_capture_result_with_authority(authority, Ok(report), deadline)
        .expect_err("a success crossing the deadline must be rejected");

    let primary_is_deadline = match &error {
        PreviewCaptureError::DeadlineExceeded => true,
        PreviewCaptureError::CleanupFailed(context) => {
            matches!(context.primary(), PreviewCaptureError::DeadlineExceeded)
        }
        _ => false,
    };
    assert!(
        primary_is_deadline,
        "deadline error was not preserved: {error}"
    );
    let cleanup_deadline = Instant::now() + Duration::from_secs(2);
    while output.exists() && Instant::now() < cleanup_deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(
        !output.exists(),
        "late success left a published file behind"
    );
}

#[cfg(not(windows))]
#[test]
fn final_capture_settlement_without_retained_handle_is_explicitly_unresolved() {
    let _capture_guard = capture_test_guard();
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
                && matches!(context.secondary(), PreviewCaptureError::OutputFailed(_))
    ));
    assert!(
        output.exists(),
        "path-only settlement must leave unresolved residue visible"
    );
}

#[test]
fn expired_png_encoding_is_bounded_and_leaves_no_temp_residue() {
    let _capture_guard = capture_test_guard();
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
    let _capture_guard = capture_test_guard();
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
    let _capture_guard = capture_test_guard();
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
    let _capture_guard = capture_test_guard();
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
    let _capture_guard = capture_test_guard();
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

    let _capture_guard = capture_test_guard();
    const EXPECTED_SENTINEL_RGBA: [u8; 4] = [0x91, 0x2b, 0xd4, 0xff];
    let output = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join(".devmanager-next/evidence/phase-05/screenshots")
        .join(format!("wgc-sentinel-{}.png", std::process::id()));
    let fixture =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/ui/theme-gallery.json");
    let before = foreground_hwnd();

    let child = Command::new(env!("CARGO_BIN_EXE_devmanager"))
        .env("DEVMANAGER_INSTANCE_LABEL", "Next")
        .env("DEVMANAGER_RUNTIME_KIND", "native-next")
        .args([
            "--ui-preview",
            fixture.to_str().expect("fixture path is UTF-8"),
            "--output",
            output.to_str().expect("output path is UTF-8"),
        ])
        .output()
        .expect("isolated devmanager --ui-preview must start");
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
