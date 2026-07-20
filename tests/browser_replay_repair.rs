use devmanager::browser::{
    BrowserElementRef, BrowserError, BrowserLocator, BrowserReplayLocatorSlot,
    BrowserReplayRepairCandidate, BrowserReplayRepairInstance, BrowserReplayRepairPhase,
    BrowserReplayRepairProjection, BrowserResourceLimits, BrowserResourceStore, BrowserRevision,
};
use static_assertions::{assert_impl_all, assert_not_impl_any};
#[cfg(target_os = "windows")]
use std::path::PathBuf;
#[cfg(target_os = "windows")]
use std::process::Command;
#[cfg(target_os = "windows")]
use std::time::{SystemTime, UNIX_EPOCH};

assert_impl_all!(BrowserResourceStore: Clone, Send, Sync);
assert_impl_all!(BrowserReplayLocatorSlot: Clone, Send, Sync, Eq, std::fmt::Debug, serde::Serialize);
assert_impl_all!(BrowserReplayRepairInstance: Clone, Send, Sync, Eq);
assert_not_impl_any!(BrowserReplayRepairInstance: std::fmt::Debug, serde::Serialize);
assert_impl_all!(BrowserReplayRepairCandidate: Clone, Send, Sync, Eq);
assert_not_impl_any!(BrowserReplayRepairCandidate: std::fmt::Debug, serde::Serialize);
assert_impl_all!(BrowserReplayRepairPhase: Clone, Send, Sync, Eq, std::fmt::Debug, serde::Serialize);
assert_impl_all!(BrowserReplayRepairProjection: Clone, Send, Sync, Eq, std::fmt::Debug, serde::Serialize);

fn normalized_source(source: &str) -> String {
    source.replace("\r\n", "\n")
}

#[test]
fn repair_apply_is_two_phase_preserves_applied_outcomes_and_seeds_later_repairs_from_override() {
    let commands = normalized_source(include_str!("../src/browser/commands.rs"));
    let replay = normalized_source(include_str!("../src/browser/replay.rs"));
    let repair = normalized_source(include_str!("../src/browser/replay_repair.rs"));

    assert!(repair.contains("enum BrowserReplayRepairApplyStage"));
    assert!(repair.contains("PreCommit"));
    assert!(repair.contains("PostCommit"));
    assert!(commands.contains("request_replay_repair_apply"));
    assert!(commands.contains("reserve_locator_repair_post_commit_validation"));
    assert!(commands.contains("complete_locator_repair_post_commit_validation"));
    assert!(commands.contains("&mut commit"));
    assert!(commands.contains("commit.replay = projection"));
    assert!(commands.contains("Ok(commit)"));
    assert!(!commands.contains(
        "complete_locator_repair_post_commit_validation(\n            post_acknowledgement,\n            commit,"
    ));

    let reserve_start = replay
        .find("pub(crate) fn reserve_locator_repair_capture")
        .unwrap();
    let reserve_end = replay[reserve_start..]
        .find("pub(crate) fn issue_locator_repair_capture_authority")
        .unwrap()
        + reserve_start;
    let reserve = &replay[reserve_start..reserve_end];
    assert!(reserve.contains("_locator_overrides"));
    assert!(reserve.contains(".get(&(step_index, locator_slot))"));
    assert!(reserve.contains(".unwrap_or(plan_locator)"));

    let publish_start = replay.find("pub(crate) fn publish_locator_repair").unwrap();
    let publish_end = replay[publish_start..].find("pub fn complete(").unwrap() + publish_start;
    let publish = &replay[publish_start..publish_end];
    assert!(publish.contains("_locator_overrides"));
    assert!(publish.contains(".or(plan_old_locator)"));
    assert!(publish.contains("exact_old_locator.as_ref()"));

    let post_start = replay
        .find("pub(crate) fn complete_locator_repair_post_commit_validation")
        .unwrap();
    let post_end = replay[post_start..]
        .find("pub(crate) fn locator_repair_status")
        .unwrap()
        + post_start;
    let post = &replay[post_start..post_end];
    assert!(post.contains("commit: &mut BrowserReplayRepairApplyCommit"));
    assert!(post.contains("previous.applied_preview_fresh = false"));
    assert!(!post.contains("previous.applied_preview_fresh = !resume"));
}

#[test]
fn repair_candidate_carries_an_exact_element_reference_without_a_public_locator_projection() {
    let element_ref = BrowserElementRef {
        revision: BrowserRevision(17),
        locator: BrowserLocator {
            accessibility_role: Some("button".to_string()),
            accessibility_name: Some("Submit".to_string()),
            test_id: Some("submit".to_string()),
            css_selectors: vec!["button[type=submit]".to_string()],
        },
        backend_node_id: Some(42),
    };
    let candidate = BrowserReplayRepairCandidate::new(element_ref.clone());
    assert_eq!(candidate.element_ref(), &element_ref);

    let replay = normalized_source(include_str!("../src/browser/replay.rs"));
    let repair = normalized_source(include_str!("../src/browser/replay_repair.rs"));
    for source in [&replay, &repair] {
        assert!(!source
            .contains("derive(Debug, Clone, Serialize)\npub struct BrowserReplayRepairCandidate"));
        assert!(!source.contains("candidate_locator: BrowserRecipeLocator\n    pub"));
    }
}

#[test]
fn repair_preview_private_authority_uses_checked_generation_receipts_and_value_free_state() {
    let commands = normalized_source(include_str!("../src/browser/commands.rs"));
    let replay = normalized_source(include_str!("../src/browser/replay.rs"));
    let repair = normalized_source(include_str!("../src/browser/replay_repair.rs"));
    let windows = normalized_source(include_str!("../src/browser/host/windows.rs"));
    let unsupported = normalized_source(include_str!("../src/browser/host/unsupported.rs"));

    assert!(replay.contains("next_preview_id"));
    assert!(replay.contains("checked_add(1)"));
    assert!(replay.contains("NonZeroU64::new"));
    assert!(replay.contains("reserve_locator_repair_preview"));
    assert!(replay.contains("commit_locator_repair_preview"));
    assert!(replay.contains("BrowserReplayPrivateRepairPhase::Previewing"));
    assert!(replay.contains("BrowserReplayRepairPhase::Previewed"));
    assert!(commands.contains("BrowserReplayRepairPreviewReceipt"));
    assert!(commands.contains("request_replay_repair_preview"));
    assert!(commands.contains("replay_repair_preview_sidecar"));
    assert!(commands.contains("self.replay_repair_preview_sidecar.is_none()"));
    assert!(windows.contains("BrowserCommand::RepairHighlight"));
    assert!(windows.contains("BrowserCommand::RepairClearHighlight"));
    assert!(windows.contains("BrowserAsyncPhase::RepairHighlight"));
    assert!(windows.contains("validate_repair_preview_sidecar"));
    assert!(windows.contains("cancellation_is_current"));
    assert!(windows.contains("document_generation"));
    assert!(windows.contains("acknowledge_repair_highlight_clear"));
    assert!(unsupported.contains("validate_repair_preview_sidecar"));

    for forbidden in ["candidate", "element_ref", "wire_token", "preview_token"] {
        let projection_start = repair
            .find("pub struct BrowserReplayRepairProjection")
            .unwrap();
        let projection_end = repair[projection_start..].find("}\n\n").unwrap() + projection_start;
        assert!(!repair[projection_start..projection_end].contains(forbidden));
    }
}

#[test]
fn replay_recipe_digest_and_canonical_root_binding_stay_private_and_exact_once() {
    let replay = normalized_source(include_str!("../src/browser/replay.rs"));
    let repair = normalized_source(include_str!("../src/browser/replay_repair.rs"));

    assert!(replay.contains("recipe_digest: BrowserRecipeDigestV1"));
    assert!(replay.contains("canonical_recipe_root: Arc<OnceLock<PathBuf>>"));
    assert!(replay.contains("fn bind_canonical_recipe_root"));
    assert!(replay.contains(".set(canonical)"));
    assert!(!replay.contains("pub recipe_digest:"));
    assert!(!replay.contains("pub canonical_recipe_root:"));
    assert!(replay.contains("recipe_target: BrowserReplayRecipeLocatorTarget"));
    assert!(repair.contains("step_id: String"));
    assert!(repair.contains("old_locator: BrowserRecipeLocator"));
    assert!(!repair.contains(
        "derive(Debug, Clone, Serialize)\npub(crate) struct BrowserReplayRecipeLocatorTarget"
    ));
}

#[test]
fn repair_preview_cleanup_uses_a_bounded_route_independent_host_lane() {
    let commands = include_str!("../src/browser/commands.rs");
    let app = include_str!("../src/app/mod.rs");
    let windows = include_str!("../src/browser/host/windows.rs");

    assert!(commands.contains("MAX_BROWSER_REPAIR_HIGHLIGHT_CLEANUPS: usize = 64"));
    assert!(commands.contains("repair_cleanups: Mutex<VecDeque<BrowserReplayRepairCleanupWork>>"));
    assert!(commands.contains("try_admit_repair_cleanup"));
    assert!(commands.contains("with_locked_host_work_and_repair_cleanups"));
    assert!(!commands.contains("tokio::runtime::Handle"));
    assert!(!commands.contains("runtime.spawn"));
    assert!(!commands.contains("enqueue_repair_highlight_clear"));
    assert!(!commands.contains("BrowserReplayRepairPreviewSidecar::Clear"));

    let barrier = &app[app.find("fn with_browser_host_control_barrier").unwrap()..];
    let controls = barrier.find("browser_host.handle_control").unwrap();
    let cleanup = barrier
        .find("browser_host.handle_repair_highlight_cleanup")
        .unwrap();
    let route_filtered = barrier.find("for request in lifecycle_requests").unwrap();
    assert!(controls < cleanup && cleanup < route_filtered);

    assert!(windows.contains("enum BrowserQueuedWork"));
    assert!(windows.contains("RepairCleanup(BrowserReplayRepairCleanupWork)"));
    assert!(windows.contains("active_repair_cleanups"));
    assert!(windows.contains("repair_highlight_rollback: true"));
    assert!(windows.contains("acknowledge_repair_highlight_clear"));
    assert!(windows.contains("REPAIR_HIGHLIGHT_CLEANUP_TIMEOUT"));
    assert!(windows.contains("quarantine_repair_highlight_cleanup"));

    let completion_start = windows.find("struct BrowserAsyncCompletion").unwrap();
    let completion_end = windows[completion_start..]
        .find("struct ActiveRepairCleanup")
        .unwrap()
        + completion_start;
    assert!(
        !windows[completion_start..completion_end].contains("BrowserReplayRepairCleanupWork"),
        "an async callback must not retain the cleanup admission lease"
    );

    let pump_start = windows.find("fn pump_repair_highlight_cleanups").unwrap();
    let pump_end = windows[pump_start..]
        .find("fn finish_repair_highlight_cleanup")
        .unwrap()
        + pump_start;
    let pump = &windows[pump_start..pump_end];
    assert!(windows.contains("cleanup.enqueued_at()"));
    assert!(pump.contains("active.deadline"));
    assert!(pump.contains("quarantine_repair_highlight_cleanup"));

    let journal_start = windows
        .find("fn append_repair_highlight_cleanup_journal")
        .unwrap();
    let journal_end = windows[journal_start..]
        .find("pub fn is_pending_approval")
        .unwrap()
        + journal_start;
    assert!(windows[journal_start..journal_end].contains("reconcile_annotation_pins"));
}

#[cfg(target_os = "windows")]
struct TestDir(PathBuf);

#[cfg(target_os = "windows")]
impl TestDir {
    fn new(label: &str) -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "devmanager-browser-repair-{label}-{}-{nanos:x}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    fn path(&self) -> &std::path::Path {
        &self.0
    }
}

#[cfg(target_os = "windows")]
impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[cfg(target_os = "windows")]
fn limits(count: usize) -> BrowserResourceLimits {
    BrowserResourceLimits {
        max_temporary_count: count,
        max_temporary_bytes: 1024 * 1024,
        max_resource_bytes: 1024 * 1024,
    }
}

#[cfg(target_os = "windows")]
fn open_error(result: Result<BrowserResourceStore, BrowserError>) -> BrowserError {
    match result {
        Ok(_) => panic!("resource store open unexpectedly succeeded"),
        Err(error) => error,
    }
}

#[test]
#[cfg(target_os = "windows")]
fn resource_root_lock_child_helper() {
    let Ok(mode) = std::env::var("DEVMANAGER_REPAIR_RESOURCE_CHILD") else {
        return;
    };
    let root = PathBuf::from(std::env::var_os("DEVMANAGER_REPAIR_RESOURCE_ROOT").unwrap());
    if mode == "busy" {
        assert!(matches!(
            BrowserResourceStore::open(root, limits(8)),
            Err(BrowserError::ResourceRootBusy)
        ));
    } else {
        drop(BrowserResourceStore::open(root, limits(8)).unwrap());
    }
}

#[cfg(target_os = "windows")]
#[test]
fn live_store_holds_an_os_exclusive_root_lock_without_disabling_the_parent() {
    let root = TestDir::new("busy");
    let parent_store = BrowserResourceStore::open(root.path(), limits(8)).unwrap();
    let status = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "resource_root_lock_child_helper", "--nocapture"])
        .env("DEVMANAGER_REPAIR_RESOURCE_CHILD", "busy")
        .env("DEVMANAGER_REPAIR_RESOURCE_ROOT", root.path())
        .status()
        .unwrap();
    assert!(status.success());
    assert!(parent_store
        .list(&devmanager::browser::BrowserWorkspaceKey::new("parent", "live").unwrap())
        .is_ok());
}

#[cfg(target_os = "windows")]
#[test]
fn simultaneous_same_root_opens_require_identical_runtime_limits() {
    let root = TestDir::new("limit-identity");
    let strict = BrowserResourceStore::open(root.path(), limits(1)).unwrap();
    let mismatch = open_error(BrowserResourceStore::open(root.path(), limits(8)));
    assert_eq!(mismatch, BrowserError::ResourceRootUnavailable);
    drop(strict);

    let lenient = BrowserResourceStore::open(root.path(), limits(8)).unwrap();
    let reverse = open_error(BrowserResourceStore::open(root.path(), limits(1)));
    assert_eq!(reverse, BrowserError::ResourceRootUnavailable);
    assert!(BrowserResourceStore::open(root.path(), limits(8)).is_ok());
    drop(lenient);
}

#[cfg(target_os = "windows")]
#[test]
fn lock_hardlink_is_rejected_with_a_fixed_path_free_error() {
    let root = TestDir::new("lock-hardlink");
    let outside = TestDir::new("lock-hardlink-outside");
    let sentinel = outside.path().join("outside-sentinel");
    std::fs::write(&sentinel, b"outside-contents").unwrap();
    std::fs::hard_link(
        &sentinel,
        root.path().join(".devmanager-browser-resources.lock"),
    )
    .unwrap();

    let error = open_error(BrowserResourceStore::open(root.path(), limits(8)));
    assert_eq!(error, BrowserError::ResourceRootUnavailable);
    assert_eq!(std::fs::read(&sentinel).unwrap(), b"outside-contents");
    let encoded = serde_json::to_string(&error).unwrap();
    let display = error.to_string();
    let debug = format!("{error:?}");
    for forbidden in [
        root.path().to_string_lossy().as_ref(),
        outside.path().to_string_lossy().as_ref(),
        "lock-hardlink",
        "outside-sentinel",
    ] {
        assert!(!encoded.contains(forbidden));
        assert!(!display.contains(forbidden));
        assert!(!debug.contains(forbidden));
    }
}

#[cfg(target_os = "windows")]
#[test]
fn concurrent_final_drop_and_reopen_never_reports_false_busy() {
    use std::sync::{Arc, Barrier};

    let root = TestDir::new("concurrent-reopen");
    for _ in 0..64 {
        let store = BrowserResourceStore::open(root.path(), limits(8)).unwrap();
        let barrier = Arc::new(Barrier::new(2));
        let thread_root = root.path().to_path_buf();
        let thread_barrier = Arc::clone(&barrier);
        let opener = std::thread::spawn(move || {
            thread_barrier.wait();
            BrowserResourceStore::open(thread_root, limits(8))
        });
        barrier.wait();
        drop(store);
        drop(opener.join().unwrap().unwrap());
    }
}
