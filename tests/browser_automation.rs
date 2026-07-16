use devmanager::browser::{
    build_semantic_snapshot, effective_browser_risk, BrowserAction, BrowserActionTarget,
    BrowserBounds, BrowserDownloadStore, BrowserElementRef, BrowserError, BrowserJournalActor,
    BrowserJournalEntry, BrowserLocator, BrowserLocatorStrategy, BrowserOperationQueue,
    BrowserOperationTarget, BrowserRawSemanticElement, BrowserResourceKind, BrowserResourceLimits,
    BrowserResourceStore, BrowserRevision, BrowserRisk, BrowserRuntimeTarget,
    BrowserTelemetryBuffer, BrowserWorkspaceKey, BrowserWorkspaceSnapshot,
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

#[test]
fn semantic_snapshot_is_revision_bound_prefers_semantics_and_redacts_passwords() {
    let revision = BrowserRevision(42);
    let snapshot = build_semantic_snapshot(
        revision,
        "https://fixture.test/form",
        "Fixture form",
        vec![
            BrowserRawSemanticElement {
                role: Some("button".to_string()),
                name: Some("Save profile".to_string()),
                label: None,
                text: Some("Save".to_string()),
                test_id: Some("save-profile".to_string()),
                css_selectors: vec!["#save".to_string()],
                bounds: BrowserBounds {
                    x: 10,
                    y: 20,
                    width: 100,
                    height: 30,
                },
                enabled: true,
                checked: None,
                value: None,
                input_type: None,
                interactive: true,
            },
            BrowserRawSemanticElement {
                role: Some("textbox".to_string()),
                name: Some("Password".to_string()),
                label: Some("Password".to_string()),
                text: None,
                test_id: None,
                css_selectors: vec!["#password".to_string()],
                bounds: BrowserBounds {
                    x: 10,
                    y: 60,
                    width: 200,
                    height: 30,
                },
                enabled: true,
                checked: None,
                value: Some("top-secret-value".to_string()),
                input_type: Some("password".to_string()),
                interactive: true,
            },
        ],
    );

    assert_eq!(snapshot.revision, revision);
    assert_eq!(snapshot.elements[0].element_ref.revision, revision);
    assert_eq!(snapshot.elements[1].value.as_deref(), Some("[redacted]"));
    assert!(!serde_json::to_string(&snapshot)
        .unwrap()
        .contains("top-secret-value"));
    assert_eq!(
        BrowserActionTarget::from_element_ref(snapshot.elements[0].element_ref.clone())
            .resolution_order(),
        vec![
            BrowserLocatorStrategy::TestId("save-profile".to_string()),
            BrowserLocatorStrategy::Accessibility {
                role: "button".to_string(),
                name: "Save profile".to_string(),
            },
            BrowserLocatorStrategy::Css("#save".to_string()),
        ]
    );
}

#[test]
fn action_diagnostics_are_secret_free_and_runtime_risk_cannot_be_lowered() {
    let target = BrowserActionTarget::from_element_ref(BrowserElementRef {
        revision: BrowserRevision(7),
        locator: BrowserLocator {
            accessibility_role: Some("textbox".to_string()),
            accessibility_name: Some("Password".to_string()),
            test_id: Some("password".to_string()),
            css_selectors: vec!["#password".to_string()],
        },
        backend_node_id: None,
    });
    let action = BrowserAction::Type {
        target,
        text: "never-log-this-secret".to_string(),
    };
    assert_eq!(action.redacted_summary(), "type into password");
    assert!(!format!("{:?}", action.redacted_for_diagnostics()).contains("never-log-this-secret"));

    let destructive = BrowserRuntimeTarget {
        origin_url: "https://fixture.test/settings".to_string(),
        role: Some("button".to_string()),
        name: Some("Delete account permanently".to_string()),
        input_type: None,
        form_action: None,
        permission: None,
    };
    assert_eq!(
        effective_browser_risk(BrowserRisk::Normal, Some(&destructive), None),
        BrowserRisk::Destructive
    );
    assert_eq!(
        effective_browser_risk(BrowserRisk::AccountSecurity, None, None),
        BrowserRisk::AccountSecurity
    );
}

#[test]
fn telemetry_and_workspace_journal_are_bounded_oldest_first() {
    let mut telemetry = BrowserTelemetryBuffer::new(2);
    telemetry.push("first".to_string());
    telemetry.push("second".to_string());
    telemetry.push("third".to_string());
    assert_eq!(telemetry.to_vec(), ["second", "third"]);

    let mut snapshot = BrowserWorkspaceSnapshot::default();
    for index in 0..105 {
        snapshot.append_journal_entry(BrowserJournalEntry {
            id: format!("entry-{index}"),
            actor: BrowserJournalActor::Agent,
            intent: format!("inspect item {index}"),
            url: "https://fixture.test".to_string(),
            started_at: "2026-07-16T00:00:00Z".to_string(),
            duration_ms: 1,
            result: "ok".to_string(),
            resource_ids: Vec::new(),
        });
    }
    assert_eq!(snapshot.journal_entries.len(), 100);
    assert_eq!(snapshot.journal_entries.first().unwrap().id, "entry-5");
    assert_eq!(snapshot.journal_entries.last().unwrap().id, "entry-104");

    snapshot.append_journal_entry(BrowserJournalEntry {
        id: "secret-entry".to_string(),
        actor: BrowserJournalActor::Agent,
        intent: "submit token=never-store-this Bearer also-secret".to_string(),
        url: "https://fixture.test/?password=hidden".to_string(),
        started_at: "2026-07-16T00:00:00Z".to_string(),
        duration_ms: 1,
        result: "blocked token=result-secret".to_string(),
        resource_ids: Vec::new(),
    });
    let encoded = serde_json::to_string(snapshot.journal_entries.last().unwrap()).unwrap();
    assert!(!encoded.contains("never-store-this"));
    assert!(!encoded.contains("also-secret"));
    assert!(!encoded.contains("hidden"));
    assert!(!encoded.contains("result-secret"));
}

#[test]
fn download_store_lists_and_deletes_only_verified_direct_regular_files() {
    let temp = TestDir::new("download-store");
    let root = temp.path().join("downloads");
    std::fs::create_dir_all(root.join("nested")).unwrap();
    std::fs::write(root.join("report.txt"), b"report").unwrap();
    std::fs::write(root.join("nested").join("hidden.txt"), b"hidden").unwrap();

    let store = BrowserDownloadStore::open(&root).unwrap();
    let downloads = store.list().unwrap();
    assert_eq!(downloads.len(), 1);
    assert_eq!(downloads[0].file_name, "report.txt");
    assert!(downloads[0].id.starts_with("download-"));
    assert!(!downloads[0].id.contains("report"));

    let verified = store.resolve(&downloads[0].id).unwrap();
    assert_eq!(verified, root.join("report.txt").canonicalize().unwrap());
    store.delete(&downloads[0].id).unwrap();
    assert!(!root.join("report.txt").exists());
    assert!(root.join("nested").join("hidden.txt").exists());
}
