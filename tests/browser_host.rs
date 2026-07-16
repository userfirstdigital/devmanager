use devmanager::browser::{
    browser_command_channel, browser_user_input_initialization_script, unique_download_path,
    unsupported_host_status, unsupported_platform_error, validate_browser_url, BrowserCommand,
    BrowserCommandBridge, BrowserCommandRequest, BrowserDiagnosticLevel, BrowserDownloadState,
    BrowserError, BrowserHostEvent, BrowserHostState, BrowserHostStatus, BrowserMemoryTarget,
    BrowserPageLoadState, BrowserResponse, BrowserStorageLayout, BrowserTabSnapshot,
    BrowserUserInputKind, BrowserViewport, BrowserWebViewHost, BrowserWorkspaceKey,
    BrowserWorkspaceSnapshot,
};
use static_assertions::{assert_impl_all, assert_not_impl_any};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

fn workspace(project: &str, conversation: &str) -> BrowserWorkspaceKey {
    BrowserWorkspaceKey::new(project, conversation).expect("valid browser workspace key")
}

async fn wait_for_pending_count(bridge: &BrowserCommandBridge, expected: usize) {
    tokio::time::timeout(Duration::from_millis(100), async {
        while bridge.pending_work_count() != expected {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("pending work count should settle");
}

struct TestDir(PathBuf);

impl TestDir {
    fn new(label: &str) -> Self {
        static NEXT_ID: AtomicU64 = AtomicU64::new(1);
        let path = std::env::temp_dir().join(format!(
            "devmanager-browser-host-{label}-{}-{}",
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

#[tokio::test]
async fn command_requests_stay_bound_and_typed_results_round_trip() {
    let key = workspace("project-a", "conversation-a");
    let (bridge, mut inbox) = browser_command_channel(4);
    let controller = bridge.bind(key.clone(), Duration::from_secs(1));

    let response_task = tokio::spawn({
        let controller = controller.clone();
        async move { controller.request(BrowserCommand::Status).await }
    });
    let request = inbox.recv().await.expect("status request");
    assert_eq!(request.workspace_key(), &key);
    assert_eq!(request.command(), &BrowserCommand::Status);
    let expected = BrowserResponse::Status {
        status: BrowserHostStatus {
            available: true,
            platform: "windows".to_string(),
            version: Some("123.0.0".to_string()),
            diagnostic: None,
        },
    };
    request.respond(Ok(expected.clone()));
    assert_eq!(response_task.await.expect("request task"), Ok(expected));

    let error_task = tokio::spawn(async move { controller.request(BrowserCommand::Status).await });
    let request = inbox.recv().await.expect("error request");
    let expected_error = BrowserError::CrashedView {
        message: "renderer exited".to_string(),
    };
    request.respond(Err(expected_error.clone()));
    assert_eq!(error_task.await.expect("error task"), Err(expected_error));
}

#[tokio::test]
async fn controller_requests_return_a_typed_timeout() {
    let key = workspace("project-a", "conversation-a");
    let (bridge, mut inbox) = browser_command_channel(4);
    let controller = bridge.bind(key, Duration::from_millis(10));

    let request_task =
        tokio::spawn(async move { controller.request(BrowserCommand::Status).await });
    let _unanswered = inbox.recv().await.expect("status request");

    let result = tokio::time::timeout(Duration::from_millis(100), request_task)
        .await
        .expect("controller should bound its own request")
        .expect("timeout task");
    assert_eq!(
        result,
        Err(BrowserError::Timeout {
            operation: "status".to_string(),
        })
    );
}

#[tokio::test]
async fn controller_timeout_also_bounds_a_saturated_inbox() {
    let key = workspace("project-a", "conversation-a");
    let (bridge, _inbox) = browser_command_channel(1);
    let controller = bridge.bind(key, Duration::from_millis(10));
    controller
        .notify(BrowserCommand::Status)
        .await
        .expect("fill bounded inbox");

    let result = tokio::time::timeout(
        Duration::from_millis(100),
        controller.request(BrowserCommand::Status),
    )
    .await
    .expect("request enqueue should be bounded");
    assert_eq!(
        result,
        Err(BrowserError::Timeout {
            operation: "status".to_string(),
        })
    );
}

#[tokio::test]
async fn pending_work_is_observable_until_receive_without_cancel_or_timeout_leaks() {
    let key = workspace("project-a", "conversation-a");
    let (bridge, mut inbox) = browser_command_channel(1);
    let controller = bridge.bind(key.clone(), Duration::from_secs(1));

    let response_task = tokio::spawn({
        let controller = controller.clone();
        async move { controller.request(BrowserCommand::Status).await }
    });
    wait_for_pending_count(&bridge, 1).await;
    assert_eq!(inbox.pending_work_count(), 1);
    let request = inbox.recv().await.expect("pending status request");
    assert_eq!(bridge.pending_work_count(), 0);
    assert_eq!(inbox.pending_work_count(), 0);
    request.respond(Ok(BrowserResponse::Acknowledged));
    assert_eq!(
        response_task.await.expect("status request task"),
        Ok(BrowserResponse::Acknowledged)
    );

    controller
        .notify(BrowserCommand::Status)
        .await
        .expect("fill bounded inbox");
    assert_eq!(bridge.pending_work_count(), 1);

    let cancelled_task = tokio::spawn({
        let controller = controller.clone();
        async move { controller.request(BrowserCommand::Status).await }
    });
    wait_for_pending_count(&bridge, 2).await;
    cancelled_task.abort();
    assert!(cancelled_task
        .await
        .expect_err("request task should be cancelled")
        .is_cancelled());
    wait_for_pending_count(&bridge, 1).await;

    let short_controller = bridge.bind(key, Duration::from_millis(10));
    assert_eq!(
        short_controller.request(BrowserCommand::Status).await,
        Err(BrowserError::Timeout {
            operation: "status".to_string(),
        })
    );
    assert_eq!(bridge.pending_work_count(), 1);

    let _queued_request = inbox.recv().await.expect("queued notification");
    assert_eq!(bridge.pending_work_count(), 0);
    assert_eq!(inbox.pending_work_count(), 0);
}

#[tokio::test]
async fn stop_and_user_input_interrupt_outstanding_tab_operations() {
    let key = workspace("project-a", "conversation-a");
    let (bridge, mut inbox) = browser_command_channel(8);
    let controller = bridge.bind(key.clone(), Duration::from_secs(1));

    let stopped_task = tokio::spawn({
        let controller = controller.clone();
        async move {
            controller
                .request(BrowserCommand::Navigate {
                    tab_id: "tab-a".to_string(),
                    url: "https://example.test/first".to_string(),
                })
                .await
        }
    });
    let _stopped_request = inbox.recv().await.expect("navigate request");
    controller
        .notify(BrowserCommand::Stop {
            tab_id: Some("tab-a".to_string()),
        })
        .await
        .expect("queue stop");
    assert_eq!(
        stopped_task.await.expect("stopped task"),
        Err(BrowserError::Interrupted)
    );
    let _stop_request = inbox.recv().await.expect("stop notification");

    let user_interrupted_task = tokio::spawn({
        let controller = controller.clone();
        async move {
            controller
                .request(BrowserCommand::Navigate {
                    tab_id: "tab-a".to_string(),
                    url: "https://example.test/second".to_string(),
                })
                .await
        }
    });
    let _user_interrupted_request = inbox.recv().await.expect("second navigate request");
    inbox.interrupt_tab(&key, "tab-a");
    assert_eq!(
        user_interrupted_task.await.expect("user interrupted task"),
        Err(BrowserError::Interrupted)
    );
}

#[tokio::test]
async fn routed_user_input_events_interrupt_the_matching_controller_tab() {
    let key = workspace("project-a", "conversation-a");
    let (bridge, mut inbox) = browser_command_channel(4);
    let controller = bridge.bind(key.clone(), Duration::from_secs(1));
    let request_task = tokio::spawn(async move {
        controller
            .request(BrowserCommand::Reload {
                tab_id: "tab-a".to_string(),
            })
            .await
    });
    let _request = inbox.recv().await.expect("reload request");

    bridge.observe_host_event(&BrowserHostEvent::UserInput {
        workspace_key: key,
        tab_id: "tab-a".to_string(),
        kind: BrowserUserInputKind::Keyboard,
    });
    assert_eq!(
        request_task.await.expect("interrupted request"),
        Err(BrowserError::Interrupted)
    );
}

#[test]
fn host_state_creates_isolated_blank_tabs_and_restores_the_selected_tab() {
    let temp = TestDir::new("workspace-state");
    let mut host = BrowserHostState::new(temp.path());
    let first_key = workspace("project-a", "conversation-a");
    let second_key = workspace("project-a", "conversation-b");

    let first = host
        .ensure_workspace(first_key.clone(), BrowserWorkspaceSnapshot::default())
        .expect("ensure first workspace");
    let second = host
        .ensure_workspace(second_key.clone(), BrowserWorkspaceSnapshot::default())
        .expect("ensure second workspace");
    assert_eq!(first.snapshot.tabs.len(), 1);
    assert_eq!(first.snapshot.tabs[0].url, "about:blank");
    assert_eq!(
        first.snapshot.selected_tab_id.as_deref(),
        Some(first.snapshot.tabs[0].id.as_str())
    );
    assert_eq!(second.snapshot.tabs.len(), 1);
    assert_ne!(first.snapshot.tabs[0].id, second.snapshot.tabs[0].id);

    host.create_tab(&first_key, "https://example.test/extra")
        .expect("create first-workspace tab");
    assert_eq!(host.workspace(&first_key).unwrap().tabs.len(), 2);
    assert_eq!(host.workspace(&second_key).unwrap().tabs.len(), 1);

    let restored_key = workspace("project-b", "conversation-c");
    let restored = BrowserWorkspaceSnapshot {
        pane_open: true,
        tabs: vec![
            BrowserTabSnapshot {
                id: "persisted-one".to_string(),
                title: "One".to_string(),
                url: "https://example.test/one".to_string(),
                viewport: BrowserViewport::default(),
            },
            BrowserTabSnapshot {
                id: "persisted-two".to_string(),
                title: "Two".to_string(),
                url: "https://example.test/two".to_string(),
                viewport: BrowserViewport::default(),
            },
        ],
        selected_tab_id: Some("persisted-two".to_string()),
        ..BrowserWorkspaceSnapshot::default()
    };
    let mutation = host
        .ensure_workspace(restored_key.clone(), restored.clone())
        .expect("restore workspace");
    assert_eq!(mutation.snapshot, restored);
    let selected = host
        .selected_view_plan(&restored_key)
        .expect("selected restored view");
    assert_eq!(selected.tab_id, "persisted-two");
    assert_eq!(selected.url, "https://example.test/two");
}

#[test]
fn ensure_workspace_never_replaces_newer_live_state_with_a_launch_snapshot() {
    let temp = TestDir::new("idempotent-ensure");
    let mut host = BrowserHostState::new(temp.path());
    let key = workspace("project-a", "conversation-a");
    let launch_snapshot = BrowserWorkspaceSnapshot {
        revision: devmanager::browser::BrowserRevision(3),
        tabs: vec![BrowserTabSnapshot {
            id: "launch-tab".to_string(),
            title: "Launch".to_string(),
            url: "https://example.test/launch".to_string(),
            viewport: BrowserViewport::default(),
        }],
        selected_tab_id: Some("launch-tab".to_string()),
        ..BrowserWorkspaceSnapshot::default()
    };
    host.ensure_workspace(key.clone(), launch_snapshot.clone())
        .expect("initial ensure");
    let live = host
        .create_tab(&key, "https://example.test/live")
        .expect("mutate live workspace")
        .snapshot;
    assert!(live.revision > launch_snapshot.revision);

    let ensured = host
        .ensure_workspace(key.clone(), launch_snapshot)
        .expect("repeat ensure");

    assert_eq!(ensured.snapshot, live);
    assert_eq!(host.workspace(&key), Some(&live));
}

#[test]
fn project_context_planning_reuses_only_same_project_profiles() {
    let temp = TestDir::new("project-context");
    let host = BrowserHostState::new(temp.path());
    let conversation_a = workspace("project-a", "conversation-a");
    let conversation_b = workspace("project-a", "conversation-b");
    let other_project = workspace("project-b", "conversation-a");

    let first = host.project_context_key(&conversation_a);
    let same = host.project_context_key(&conversation_b);
    let other = host.project_context_key(&other_project);
    assert_eq!(first, same);
    assert_ne!(first, other);
    assert_eq!(
        first.profile_dir,
        BrowserStorageLayout::new(temp.path(), "project-a").profile_dir
    );
}

#[test]
fn visibility_planning_shows_one_selected_view_and_suspends_every_other_view() {
    let temp = TestDir::new("visibility");
    let mut host = BrowserHostState::new(temp.path());
    let first_key = workspace("project-a", "conversation-a");
    let second_key = workspace("project-a", "conversation-b");

    host.ensure_workspace(first_key.clone(), BrowserWorkspaceSnapshot::default())
        .unwrap();
    let first_selected = host
        .create_tab(&first_key, "https://example.test/first")
        .unwrap()
        .snapshot
        .selected_tab_id
        .unwrap();
    host.ensure_workspace(second_key.clone(), BrowserWorkspaceSnapshot::default())
        .unwrap();
    host.create_tab(&second_key, "https://example.test/second")
        .unwrap();
    host.set_pane_open(&first_key, true).unwrap();
    host.set_pane_open(&second_key, true).unwrap();

    host.set_active_workspace(Some(first_key.clone()));
    let plans = host.visibility_plan();
    assert_eq!(plans.len(), 4);
    let visible: Vec<_> = plans.iter().filter(|plan| plan.visible).collect();
    assert_eq!(visible.len(), 1);
    assert_eq!(visible[0].workspace_key, first_key);
    assert_eq!(visible[0].tab_id, first_selected);
    assert_eq!(visible[0].memory_target, BrowserMemoryTarget::Normal);
    assert!(plans
        .iter()
        .filter(|plan| !plan.visible)
        .all(|plan| plan.memory_target == BrowserMemoryTarget::Low));

    host.set_active_workspace(Some(second_key.clone()));
    let plans = host.visibility_plan();
    assert_eq!(plans.iter().filter(|plan| plan.visible).count(), 1);
    assert!(plans
        .iter()
        .find(|plan| plan.visible)
        .is_some_and(|plan| plan.workspace_key == second_key));

    host.set_pane_open(&second_key, false).unwrap();
    assert!(host
        .visibility_plan()
        .iter()
        .all(|plan| { !plan.visible && plan.memory_target == BrowserMemoryTarget::Low }));
}

#[test]
fn browser_url_validation_rejects_dangerous_and_malformed_schemes() {
    for allowed in [
        "about:blank",
        "https://example.test/path?q=1",
        "http://127.0.0.1:4000/",
    ] {
        assert_eq!(validate_browser_url(allowed), Ok(allowed.to_string()));
    }

    for rejected in [
        "javascript:alert(1)",
        "data:text/html,secret",
        "file:///C:/private.txt",
        "ftp://example.test/file",
        "edge://settings",
        "example.test/no-scheme",
        "https://",
        " https://example.test",
    ] {
        assert!(matches!(
            validate_browser_url(rejected),
            Err(BrowserError::NavigationFailure { url, .. }) if url == rejected
        ));
    }

    let temp = TestDir::new("url-validation");
    let mut host = BrowserHostState::new(temp.path());
    let key = workspace("project-a", "conversation-a");
    host.ensure_workspace(key.clone(), BrowserWorkspaceSnapshot::default())
        .unwrap();
    assert!(matches!(
        host.create_tab(&key, "javascript:alert(document.cookie)"),
        Err(BrowserError::NavigationFailure { .. })
    ));
}

#[test]
fn profile_clear_planning_is_confined_to_the_exact_hashed_profile_directory() {
    let temp = TestDir::new("profile-clear");
    let host = BrowserHostState::new(temp.path());
    let key = workspace("private/project:id", "conversation-a");
    let layout = BrowserStorageLayout::new(temp.path(), &key.project_id);

    let plan = host
        .profile_clear_plan(&key, &layout.profile_dir)
        .expect("exact profile clear plan");
    assert_eq!(plan.profile_dir, layout.profile_dir);
    assert_eq!(plan.paths(), [layout.profile_dir.as_path()]);
    assert!(!plan.paths().contains(&layout.downloads_dir.as_path()));
    assert!(!plan.paths().contains(&layout.resources_dir.as_path()));

    let hash = layout.profile_dir.file_name().unwrap();
    let rejected = [
        layout.downloads_dir.clone(),
        layout.resources_dir.clone(),
        layout.profile_dir.parent().unwrap().to_path_buf(),
        layout
            .profile_dir
            .join("..")
            .join("..")
            .join("downloads")
            .join(hash),
        temp.path().join("alternate-root").join(hash),
    ];
    for candidate in rejected {
        assert!(matches!(
            host.profile_clear_plan(&key, &candidate),
            Err(BrowserError::OutsideWorkspace { path }) if path == candidate
        ));
    }
}

#[test]
fn browser_command_response_and_event_json_names_are_stable_camel_case() {
    let snapshot = BrowserWorkspaceSnapshot::default();
    let viewport = BrowserViewport::default();
    let commands = vec![
        (BrowserCommand::Status, "status"),
        (
            BrowserCommand::Ensure {
                snapshot: snapshot.clone(),
            },
            "ensure",
        ),
        (BrowserCommand::SetPaneOpen { open: true }, "setPaneOpen"),
        (BrowserCommand::ListTabs, "listTabs"),
        (
            BrowserCommand::CreateTab {
                url: Some("https://example.test".to_string()),
            },
            "createTab",
        ),
        (
            BrowserCommand::SelectTab {
                tab_id: "tab-a".to_string(),
            },
            "selectTab",
        ),
        (
            BrowserCommand::CloseTab {
                tab_id: "tab-a".to_string(),
            },
            "closeTab",
        ),
        (
            BrowserCommand::Navigate {
                tab_id: "tab-a".to_string(),
                url: "https://example.test".to_string(),
            },
            "navigate",
        ),
        (
            BrowserCommand::Back {
                tab_id: "tab-a".to_string(),
            },
            "back",
        ),
        (
            BrowserCommand::Forward {
                tab_id: "tab-a".to_string(),
            },
            "forward",
        ),
        (
            BrowserCommand::Reload {
                tab_id: "tab-a".to_string(),
            },
            "reload",
        ),
        (
            BrowserCommand::UpdateViewport {
                tab_id: "tab-a".to_string(),
                viewport,
            },
            "updateViewport",
        ),
        (
            BrowserCommand::OpenDevTools {
                tab_id: "tab-a".to_string(),
            },
            "openDevTools",
        ),
        (
            BrowserCommand::Stop {
                tab_id: Some("tab-a".to_string()),
            },
            "stop",
        ),
        (BrowserCommand::ResetWorkspace, "resetWorkspace"),
        (BrowserCommand::ClearProjectProfile, "clearProjectProfile"),
        (BrowserCommand::DownloadDirectory, "downloadDirectory"),
    ];
    for (command, expected_type) in commands {
        let value = serde_json::to_value(&command).expect("serialize command");
        assert_eq!(value["type"], expected_type);
        assert!(value.get("tab_id").is_none());
        let round_trip: BrowserCommand = serde_json::from_value(value).expect("round-trip command");
        assert_eq!(round_trip, command);
    }

    let response = BrowserResponse::DownloadDirectory {
        path: PathBuf::from("C:/downloads/project-a"),
    };
    let value = serde_json::to_value(&response).expect("serialize response");
    assert_eq!(value["type"], "downloadDirectory");
    assert_eq!(
        serde_json::from_value::<BrowserResponse>(value).unwrap(),
        response
    );

    let key = workspace("project-a", "conversation-a");
    let events = vec![
        BrowserHostEvent::UrlChanged {
            workspace_key: key.clone(),
            tab_id: "tab-a".to_string(),
            url: "https://example.test".to_string(),
        },
        BrowserHostEvent::TitleChanged {
            workspace_key: key.clone(),
            tab_id: "tab-a".to_string(),
            title: "Example".to_string(),
        },
        BrowserHostEvent::PageLoad {
            workspace_key: key.clone(),
            tab_id: "tab-a".to_string(),
            state: BrowserPageLoadState::Finished,
            url: "https://example.test".to_string(),
        },
        BrowserHostEvent::UserInput {
            workspace_key: key.clone(),
            tab_id: "tab-a".to_string(),
            kind: BrowserUserInputKind::Pointer,
        },
        BrowserHostEvent::NewWindow {
            workspace_key: key.clone(),
            tab_id: "tab-a".to_string(),
            url: "https://example.test/popup".to_string(),
        },
        BrowserHostEvent::Download {
            workspace_key: key.clone(),
            tab_id: "tab-a".to_string(),
            state: BrowserDownloadState::Started,
            url: "https://example.test/report.pdf".to_string(),
            path: PathBuf::from("C:/downloads/report.pdf"),
        },
        BrowserHostEvent::Diagnostic {
            workspace_key: key,
            tab_id: "tab-a".to_string(),
            level: BrowserDiagnosticLevel::Error,
            message: "renderer exited".to_string(),
        },
    ];
    let expected_types = [
        "urlChanged",
        "titleChanged",
        "pageLoad",
        "userInput",
        "newWindow",
        "download",
        "diagnostic",
    ];
    for (event, expected_type) in events.into_iter().zip(expected_types) {
        let value = serde_json::to_value(&event).expect("serialize event");
        assert_eq!(value["type"], expected_type);
        assert!(value.get("workspaceKey").is_some());
        assert!(value.get("tabId").is_some());
        assert!(value.get("workspace_key").is_none());
        assert_eq!(
            serde_json::from_value::<BrowserHostEvent>(value).unwrap(),
            event
        );
    }
}

#[test]
fn unsupported_adapter_helpers_return_the_typed_platform_error() {
    let status = unsupported_host_status("macos");
    assert_eq!(
        status,
        BrowserHostStatus {
            available: false,
            platform: "macos".to_string(),
            version: None,
            diagnostic: Some("embedded browser support is unavailable on macos".to_string()),
        }
    );
    assert_eq!(
        unsupported_platform_error("macos"),
        BrowserError::UnavailablePlatform {
            platform: "macos".to_string(),
        }
    );
}

assert_impl_all!(devmanager::browser::BrowserController: Send, Sync, Clone);
assert_not_impl_any!(BrowserWebViewHost: Send, Sync);

#[test]
fn windows_host_construction_reports_availability_without_opening_a_view() {
    let temp = TestDir::new("windows-host-construction");
    let host = BrowserWebViewHost::new(temp.path());
    let status = host.status();
    assert_eq!(status.platform, std::env::consts::OS);
    if status.available {
        assert!(status
            .version
            .as_ref()
            .is_some_and(|value| !value.is_empty()));
        assert!(status.diagnostic.is_none());
    } else {
        assert!(status.version.is_none());
        assert!(status
            .diagnostic
            .as_ref()
            .is_some_and(|value| !value.is_empty()));
    }
}

#[test]
fn initialization_script_reports_only_trusted_input_metadata() {
    let script = browser_user_input_initialization_script();
    assert!(script.contains("event.isTrusted"));
    assert!(script.contains("pointerdown"));
    assert!(script.contains("keydown"));
    assert!(script.contains("input"));
    assert!(script.contains("window.ipc.postMessage"));
    assert!(script.contains("userInput"));
    assert!(!script.contains("target.value"));
    assert!(!script.contains("textContent"));
    assert!(!script.contains("innerHTML"));
}

#[test]
fn download_paths_stay_in_project_directory_and_never_overwrite() {
    let temp = TestDir::new("download-paths");
    let downloads = temp.path().join("downloads");
    std::fs::create_dir_all(&downloads).unwrap();
    std::fs::write(downloads.join("report.pdf"), b"existing").unwrap();
    std::fs::write(downloads.join("report (1).pdf"), b"existing").unwrap();

    assert_eq!(
        unique_download_path(&downloads, Path::new("report.pdf")).unwrap(),
        downloads.join("report (2).pdf")
    );
    assert_eq!(
        unique_download_path(&downloads, Path::new("../escape.txt")).unwrap(),
        downloads.join("escape.txt")
    );
    assert_eq!(
        unique_download_path(&downloads, Path::new(".")).unwrap(),
        downloads.join("download")
    );
}

#[test]
fn host_tab_and_page_mutations_advance_the_existing_snapshot_revision() {
    let temp = TestDir::new("host-mutations");
    let mut host = BrowserHostState::new(temp.path());
    let key = workspace("project-a", "conversation-a");
    let ensured = host
        .ensure_workspace(key.clone(), BrowserWorkspaceSnapshot::default())
        .unwrap();
    let first_tab = ensured.snapshot.tabs[0].id.clone();
    let first_revision = ensured.revision;

    let navigated = host
        .navigate_tab(&key, &first_tab, "https://example.test/one")
        .unwrap();
    assert_eq!(navigated.revision.0, first_revision.0 + 1);
    assert_eq!(navigated.snapshot.tabs[0].url, "https://example.test/one");

    let viewport = BrowserViewport {
        width: 1440,
        height: 900,
        scale_percent: 125,
    };
    let updated = host
        .update_viewport(&key, &first_tab, viewport.clone())
        .unwrap();
    assert_eq!(updated.snapshot.tabs[0].viewport, viewport);

    let created = host.create_tab(&key, "https://example.test/two").unwrap();
    let second_tab = created.snapshot.selected_tab_id.clone().unwrap();
    assert_eq!(
        host.select_tab(&key, &first_tab)
            .unwrap()
            .snapshot
            .selected_tab_id
            .as_deref(),
        Some(first_tab.as_str())
    );
    host.close_tab(&key, &first_tab).unwrap();
    let after_last_close = host.close_tab(&key, &second_tab).unwrap();
    assert_eq!(after_last_close.snapshot.tabs.len(), 1);
    assert_eq!(after_last_close.snapshot.tabs[0].url, "about:blank");

    let replacement = after_last_close.snapshot.tabs[0].id.clone();
    let title = host
        .apply_title_change(&key, &replacement, "Blank page")
        .unwrap();
    assert_eq!(title.snapshot.tabs[0].title, "Blank page");
    let user_input = host.apply_user_input(&key, &replacement).unwrap();
    assert_eq!(user_input.revision.0, title.revision.0 + 1);
    let loaded = host
        .apply_page_load(&key, &replacement, "https://example.test/final")
        .unwrap();
    assert_eq!(loaded.snapshot.tabs[0].url, "https://example.test/final");

    let mut saturated = loaded.snapshot;
    saturated.revision = devmanager::browser::BrowserRevision(u64::MAX);
    host.reset_workspace(&key);
    host.ensure_workspace(key.clone(), saturated).unwrap();
    assert_eq!(
        host.apply_page_load(&key, &replacement, "https://example.test/max")
            .unwrap()
            .revision
            .0,
        u64::MAX
    );

    host.reset_workspace(&key);
    assert!(host.workspace(&key).is_none());
}

#[allow(dead_code)]
fn browser_webview_host_exposes_the_main_thread_mounting_seam(
    host: &mut BrowserWebViewHost,
    window: &gpui::Window,
    workspace_key: &BrowserWorkspaceKey,
    request: BrowserCommandRequest,
) {
    let _: Result<BrowserResponse, BrowserError> =
        host.handle_command(window, workspace_key, BrowserCommand::Status);
    host.handle_request(window, request);
    let _: Result<(), BrowserError> = host.set_active_workspace(Some(workspace_key.clone()));
    let _: Result<(), BrowserError> = host.set_bounds(devmanager::browser::BrowserBounds {
        x: 0,
        y: 0,
        width: 800,
        height: 600,
    });
    let _: Vec<BrowserHostEvent> = host.drain_events();
    let _: Option<&BrowserWorkspaceSnapshot> = host.workspace_snapshot(workspace_key);
}
