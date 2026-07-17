use devmanager::browser::{
    build_semantic_snapshot, effective_browser_risk, effective_browser_risk_for_targets,
    redact_browser_resource_bytes, redact_browser_text, BrowserAction, BrowserActionTarget,
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
fn user_input_wins_a_same_pump_completion_race_and_never_starts_queued_work() {
    let target =
        BrowserOperationTarget::new(workspace("project-a", "conversation-a"), "tab-a").unwrap();
    let mut queue = BrowserOperationQueue::default();
    assert_eq!(
        queue.enqueue(target.clone(), "active", "active-side-effect"),
        Some("active-side-effect")
    );
    assert_eq!(
        queue.enqueue(target.clone(), "queued", "queued-side-effect"),
        None
    );

    // The GPUI pump applies the user-input lane before consuming completion callbacks.
    let interrupted = queue.cancel_tab(&target);
    assert_eq!(interrupted.active_operation_id.as_deref(), Some("active"));
    assert_eq!(interrupted.queued, vec!["queued-side-effect"]);

    // A callback already posted by WebView2 is stale after cancellation and cannot
    // commit the active side effect or promote the queued operation.
    assert_eq!(queue.complete(&target, "active"), None);
    assert!(queue.is_empty());
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
fn unlabeled_password_values_never_become_semantic_names_labels_or_text() {
    let secret = "password-value-that-must-never-escape";
    let snapshot = build_semantic_snapshot(
        BrowserRevision(9),
        "https://fixture.test/login",
        "Login",
        vec![BrowserRawSemanticElement {
            role: Some("textbox".to_string()),
            name: Some(secret.to_string()),
            label: Some(secret.to_string()),
            text: Some(secret.to_string()),
            value: Some(secret.to_string()),
            input_type: Some("PASSWORD".to_string()),
            interactive: true,
            ..BrowserRawSemanticElement::default()
        }],
    );

    let element = &snapshot.elements[0];
    assert_eq!(element.value.as_deref(), Some("[redacted]"));
    assert_eq!(element.name, None);
    assert_eq!(element.label, None);
    assert_eq!(element.text, None);
    assert_eq!(element.element_ref.locator.accessibility_name, None);
    assert!(!serde_json::to_string(&snapshot).unwrap().contains(secret));
}

#[test]
fn rust_text_journal_and_resource_redaction_cover_json_and_basic_credentials() {
    let payload = r#"{"token":"json-token","nested":{"password":"json-password","apiKey":"json-api-key","accessToken":"access-token","refresh_token":"refresh-token","clientSecret":"client-secret","sessionCookie":"session-cookie"},"authorization":"Basic dXNlcjpzZWNyZXQ=","safe":"keep"}"#;
    let redacted_text = redact_browser_text(payload);
    for secret in [
        "json-token",
        "json-password",
        "json-api-key",
        "access-token",
        "refresh-token",
        "client-secret",
        "session-cookie",
        "dXNlcjpzZWNyZXQ=",
    ] {
        assert!(!redacted_text.contains(secret), "leaked {secret}");
    }
    assert!(redacted_text.contains("[redacted]"));

    let prefixed = r#"response {"accessToken":"prefixed-access","refresh_token":"prefixed-refresh","clientSecret":"prefixed-client","sessionCookie":"prefixed-cookie","api-key":"prefixed-api","privateKey":"prefixed-private","access.token":"prefixed-dot","client secret":"prefixed-space"}"#;
    let prefixed_redacted = redact_browser_text(prefixed);
    for secret in [
        "prefixed-access",
        "prefixed-refresh",
        "prefixed-client",
        "prefixed-cookie",
        "prefixed-api",
        "prefixed-private",
        "prefixed-dot",
        "prefixed-space",
    ] {
        assert!(
            !prefixed_redacted.contains(secret),
            "prefixed leak {secret}"
        );
    }

    let redacted_bytes = redact_browser_resource_bytes("application/json", payload.as_bytes());
    let redacted_json: serde_json::Value = serde_json::from_slice(&redacted_bytes).unwrap();
    assert_eq!(redacted_json["token"], "[redacted]");
    assert_eq!(redacted_json["nested"]["password"], "[redacted]");
    assert_eq!(redacted_json["nested"]["apiKey"], "[redacted]");
    assert_eq!(redacted_json["nested"]["accessToken"], "[redacted]");
    assert_eq!(redacted_json["nested"]["refresh_token"], "[redacted]");
    assert_eq!(redacted_json["nested"]["clientSecret"], "[redacted]");
    assert_eq!(redacted_json["nested"]["sessionCookie"], "[redacted]");
    assert_eq!(redacted_json["authorization"], "[redacted]");
    assert_eq!(redacted_json["safe"], "keep");

    let binary = [0_u8, 159, 146, 150];
    assert_eq!(redact_browser_resource_bytes("image/png", &binary), binary);

    let small_cdp = br#"{"result":{"access.token":"inline-secret","authorization":"Bearer inline-bearer","safe":"keep"}}"#;
    let small_redacted = redact_browser_resource_bytes("application/json", small_cdp);
    assert!(!String::from_utf8_lossy(&small_redacted).contains("inline-secret"));
    assert!(!String::from_utf8_lossy(&small_redacted).contains("inline-bearer"));
    let mut large_value: serde_json::Value = serde_json::from_slice(small_cdp).unwrap();
    large_value["result"]["padding"] = serde_json::Value::String("x".repeat(70_000));
    let large_cdp = serde_json::to_vec(&large_value).unwrap();
    let large_redacted = redact_browser_resource_bytes("application/json", &large_cdp);
    assert!(large_redacted.len() > 64 * 1024);
    assert!(!String::from_utf8_lossy(&large_redacted).contains("inline-secret"));
    assert!(!String::from_utf8_lossy(&large_redacted).contains("inline-bearer"));

    let mut workspace = BrowserWorkspaceSnapshot::default();
    workspace.append_journal_entry(BrowserJournalEntry {
        id: "structured-secret".to_string(),
        actor: BrowserJournalActor::Agent,
        intent: payload.to_string(),
        url: "https://fixture.test".to_string(),
        started_at: "2026-07-16T00:00:00Z".to_string(),
        duration_ms: 1,
        result: "Authorization: Basic YWRtaW46c2VjcmV0".to_string(),
        resource_ids: Vec::new(),
    });
    let journal = serde_json::to_string(workspace.journal_entries.last().unwrap()).unwrap();
    assert!(!journal.contains("json-token"));
    assert!(!journal.contains("YWRtaW46c2VjcmV0"));
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
        autocomplete: None,
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
fn enter_form_and_drag_destination_runtime_targets_escalate_before_actions_run() {
    let enter_active_form = BrowserRuntimeTarget {
        origin_url: "https://fixture.test".to_string(),
        role: Some("textbox".to_string()),
        name: Some("Confirmation".to_string()),
        input_type: Some("text".to_string()),
        autocomplete: None,
        form_action: Some("https://fixture.test/delete-account-permanently".to_string()),
        permission: None,
    };
    assert_eq!(
        effective_browser_risk_for_targets(
            BrowserRisk::Normal,
            std::slice::from_ref(&enter_active_form),
            None,
        ),
        BrowserRisk::Destructive
    );

    let harmless_drag_source = BrowserRuntimeTarget {
        origin_url: "https://fixture.test".to_string(),
        role: Some("listitem".to_string()),
        name: Some("Draft item".to_string()),
        ..BrowserRuntimeTarget::default()
    };
    let risky_drag_destination = BrowserRuntimeTarget {
        origin_url: "https://fixture.test".to_string(),
        role: Some("region".to_string()),
        name: Some("Delete permanently".to_string()),
        ..BrowserRuntimeTarget::default()
    };
    assert_eq!(
        effective_browser_risk_for_targets(
            BrowserRisk::Normal,
            &[harmless_drag_source, risky_drag_destination],
            None,
        ),
        BrowserRisk::Destructive
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
