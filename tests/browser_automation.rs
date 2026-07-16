use devmanager::browser::{
    BrowserError, BrowserOperationQueue, BrowserOperationTarget, BrowserResourceKind,
    BrowserResourceLimits, BrowserResourceStore, BrowserWorkspaceKey,
};
use static_assertions::assert_impl_all;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

fn workspace(project: &str, conversation: &str) -> BrowserWorkspaceKey {
    BrowserWorkspaceKey::new(project, conversation).expect("valid browser workspace key")
}

struct TestDir(PathBuf);

impl TestDir {
    fn new(label: &str) -> Self {
        static NEXT_ID: AtomicU64 = AtomicU64::new(1);
        let path = std::env::temp_dir().join(format!(
            "devmanager-browser-automation-{label}-{}-{}",
            std::process::id(),
            NEXT_ID.fetch_add(1, Ordering::Relaxed)
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

#[test]
fn per_tab_queue_is_fifo_cross_tab_independent_and_fences_late_completions() {
    let key = workspace("project-a", "conversation-a");
    let tab_a = BrowserOperationTarget::new(key.clone(), "tab-a").unwrap();
    let tab_b = BrowserOperationTarget::new(key, "tab-b").unwrap();
    let mut queue = BrowserOperationQueue::default();

    assert_eq!(queue.enqueue(tab_a.clone(), "op-a1", "a1"), Some("a1"));
    assert_eq!(queue.enqueue(tab_a.clone(), "op-a2", "a2"), None);
    assert_eq!(queue.enqueue(tab_a.clone(), "op-a3", "a3"), None);
    assert_eq!(queue.enqueue(tab_b.clone(), "op-b1", "b1"), Some("b1"));
    assert_eq!(queue.active_operation_id(&tab_a), Some("op-a1"));
    assert_eq!(queue.active_operation_id(&tab_b), Some("op-b1"));

    assert_eq!(queue.complete(&tab_a, "late-or-wrong"), None);
    assert_eq!(queue.active_operation_id(&tab_a), Some("op-a1"));
    assert_eq!(queue.complete(&tab_a, "op-a1"), Some("a2"));
    assert_eq!(queue.active_operation_id(&tab_a), Some("op-a2"));
    assert_eq!(queue.complete(&tab_a, "op-a2"), Some("a3"));
    assert_eq!(queue.complete(&tab_b, "op-b1"), None);
    assert_eq!(queue.active_operation_id(&tab_b), None);
}

#[test]
fn per_tab_cancel_drops_active_and_returns_queued_work_in_fifo_order() {
    let target =
        BrowserOperationTarget::new(workspace("project-a", "conversation-a"), "tab-a").unwrap();
    let mut queue = BrowserOperationQueue::default();
    assert_eq!(queue.enqueue(target.clone(), "op-1", 1), Some(1));
    assert_eq!(queue.enqueue(target.clone(), "op-2", 2), None);
    assert_eq!(queue.enqueue(target.clone(), "op-3", 3), None);

    let cancelled = queue.cancel_tab(&target);

    assert_eq!(cancelled.active_operation_id.as_deref(), Some("op-1"));
    assert_eq!(cancelled.queued, vec![2, 3]);
    assert_eq!(queue.active_operation_id(&target), None);
    assert_eq!(queue.complete(&target, "op-1"), None);
}

#[test]
fn resource_store_enforces_owner_and_cleans_oldest_unpinned_resources() {
    assert_impl_all!(BrowserResourceStore: Clone, Send, Sync);
    let temp = TestDir::new("resource-cleanup");
    let store = BrowserResourceStore::open(
        temp.path(),
        BrowserResourceLimits {
            max_temporary_count: 2,
            max_temporary_bytes: 1024,
            max_resource_bytes: 512,
        },
    )
    .expect("open resource store");
    let owner = workspace("project-a", "conversation-a");
    let other = workspace("project-a", "conversation-b");

    let first = store
        .put(
            &owner,
            BrowserResourceKind::DomSnapshot,
            "application/json",
            br#"{"first":true}"#,
            false,
        )
        .expect("store first");
    let pinned = store
        .put(
            &owner,
            BrowserResourceKind::Screenshot,
            "image/png",
            b"png-pinned",
            true,
        )
        .expect("store pinned");
    let second = store
        .put(
            &owner,
            BrowserResourceKind::CdpResult,
            "application/json",
            br#"{"second":true}"#,
            false,
        )
        .expect("store second");
    let third = store
        .put(
            &owner,
            BrowserResourceKind::NetworkBody,
            "text/plain",
            b"third",
            false,
        )
        .expect("store third and clean oldest");

    assert!(matches!(
        store.read(&owner, &first.id),
        Err(BrowserError::MissingResource { .. })
    ));
    assert_eq!(store.read(&owner, &pinned.id).unwrap().bytes, b"png-pinned");
    assert_eq!(
        store.read(&owner, &second.id).unwrap().metadata.kind,
        BrowserResourceKind::CdpResult
    );
    assert_eq!(store.read(&owner, &third.id).unwrap().bytes, b"third");
    assert!(matches!(
        store.read(&other, &third.id),
        Err(BrowserError::BlockedPermission { .. })
    ));
    assert_eq!(
        third.uri,
        format!("devmanager-browser://resource/{}", third.id.0)
    );
    assert!(!third.uri.contains("project-a"));
    assert!(!third.uri.contains("conversation-a"));
}

#[test]
fn resource_store_ignores_corrupt_metadata_and_rejects_traversal_ids() {
    let temp = TestDir::new("resource-corrupt");
    std::fs::write(temp.path().join("corrupt.json"), b"not json").unwrap();
    std::fs::write(temp.path().join("untracked.bin"), b"keep me").unwrap();
    let store = BrowserResourceStore::open(temp.path(), BrowserResourceLimits::default())
        .expect("corrupt metadata must not poison the store");
    let owner = workspace("project-a", "conversation-a");
    let traversal = devmanager::browser::BrowserResourceId("../outside".to_string());

    assert!(store.list(&owner).unwrap().is_empty());
    assert!(matches!(
        store.read(&owner, &traversal),
        Err(BrowserError::BlockedPermission { .. })
    ));
    assert_eq!(
        std::fs::read(temp.path().join("untracked.bin")).unwrap(),
        b"keep me"
    );
}
