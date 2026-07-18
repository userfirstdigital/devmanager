use devmanager::browser::{BrowserError, BrowserResourceLimits, BrowserResourceStore};
use static_assertions::assert_impl_all;
#[cfg(target_os = "windows")]
use std::path::PathBuf;
#[cfg(target_os = "windows")]
use std::process::Command;
#[cfg(target_os = "windows")]
use std::time::{SystemTime, UNIX_EPOCH};

assert_impl_all!(BrowserResourceStore: Clone, Send, Sync);

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
