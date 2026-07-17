use devmanager::browser::{
    acknowledge_attachment_projection_and_reconcile_pins, browser_command_channel,
    browser_lifecycle_control, browser_request_preempts_operation_queue,
    browser_response_resource_ids, browser_user_input_initialization_script, crop_annotation_png,
    effective_browser_annotation_risk, parse_browser_annotation_ipc_message,
    parse_browser_page_ipc_message, prepare_verified_download_root, prepare_verified_profile_root,
    remove_verified_profile, route_browser_request, unique_download_path, unsupported_host_status,
    unsupported_platform_error, validate_annotation_candidate_context, validate_browser_url,
    BrowserAction, BrowserActionTarget, BrowserAnnotation, BrowserAnnotationCandidate,
    BrowserAnnotationCleanupLedger, BrowserAnnotationDraft, BrowserAnnotationKind,
    BrowserAnnotationLifecycle, BrowserAnnotationOperation, BrowserAnnotationRoute,
    BrowserAttachmentBroker, BrowserAttachmentRevision, BrowserBounds, BrowserCommand,
    BrowserCommandBridge, BrowserCommandRequest, BrowserConsoleOperation, BrowserDiagnosticLevel,
    BrowserDownloadOperation, BrowserDownloadState, BrowserElementRef, BrowserError,
    BrowserHostControl, BrowserHostEvent, BrowserHostState, BrowserHostStatus,
    BrowserInvocationActor, BrowserInvocationContext, BrowserJournalActor, BrowserJournalEntry,
    BrowserLocator, BrowserMemoryTarget, BrowserNetworkOperation, BrowserOperationQueue,
    BrowserOperationTarget, BrowserPageIpcMessage, BrowserPageLoadState,
    BrowserPerformanceOperation, BrowserResourceId, BrowserResourceKind, BrowserResourceLimits,
    BrowserResourceStore, BrowserResponse, BrowserRevision, BrowserRisk, BrowserScreenshotMode,
    BrowserStorageLayout, BrowserTabSnapshot, BrowserUserInputKind, BrowserViewport,
    BrowserWaitCondition, BrowserWaitResult, BrowserWebViewHost, BrowserWorkspaceKey,
    BrowserWorkspaceMutation, BrowserWorkspaceSnapshot, MAX_BROWSER_JOURNAL_ENTRIES,
};
use static_assertions::{assert_impl_all, assert_not_impl_any};
use std::collections::{BTreeMap, BTreeSet};
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

fn workspace(project: &str, conversation: &str) -> BrowserWorkspaceKey {
    BrowserWorkspaceKey::new(project, conversation).expect("valid browser workspace key")
}

#[test]
fn attachment_acknowledgement_preserves_host_workspace_and_concurrent_additions() {
    let key = workspace("project", "conversation");
    let mut state = BrowserHostState::new(".");
    let page_revision = BrowserRevision(41);
    let selected_tab_id = Some("tab-b".to_string());
    state
        .ensure_workspace(
            key.clone(),
            BrowserWorkspaceSnapshot {
                revision: page_revision,
                tabs: vec![
                    BrowserTabSnapshot {
                        id: "tab-a".to_string(),
                        title: "A".to_string(),
                        url: "https://a.test".to_string(),
                        viewport: BrowserViewport::default(),
                    },
                    BrowserTabSnapshot {
                        id: "tab-b".to_string(),
                        title: "B".to_string(),
                        url: "https://b.test".to_string(),
                        viewport: BrowserViewport::default(),
                    },
                ],
                selected_tab_id: selected_tab_id.clone(),
                ..BrowserWorkspaceSnapshot::default()
            },
        )
        .unwrap();
    state
        .save_annotation(
            &key,
            stored_annotation(
                "ann-delivered",
                BrowserResourceId("shot-old".to_string()),
                "old",
            ),
        )
        .unwrap();
    state
        .save_annotation(
            &key,
            stored_annotation(
                "ann-concurrent",
                BrowserResourceId("shot-new".to_string()),
                "new",
            ),
        )
        .unwrap();
    state
        .set_annotation_resolved(&key, "ann-delivered", true)
        .unwrap();
    let saved_annotations = state.workspace(&key).unwrap().annotations.clone();

    let mutation = state
        .acknowledge_attachment_projection(
            &key,
            BrowserAttachmentRevision(9),
            &[],
            &["ann-delivered".to_string()],
        )
        .unwrap();

    assert_eq!(mutation.revision, page_revision);
    assert_eq!(mutation.snapshot.revision, page_revision);
    assert_eq!(mutation.snapshot.tabs.len(), 2);
    assert_eq!(mutation.snapshot.selected_tab_id, selected_tab_id);
    assert_eq!(mutation.snapshot.annotations, saved_annotations);
    assert_eq!(
        mutation.snapshot.pending_annotation_ids,
        vec!["ann-concurrent"]
    );
    assert_eq!(
        mutation.snapshot.pending_annotation_revision,
        BrowserAttachmentRevision(9)
    );
    assert_eq!(
        mutation.snapshot.pinned_annotation_resource_ids(),
        BTreeSet::from([BrowserResourceId("shot-new".to_string())])
    );
    assert!(
        mutation.snapshot.annotation("ann-concurrent").is_ok(),
        "unresolved saved annotation context remains available"
    );
}

#[test]
fn windows_attachment_acknowledgement_reconciles_resource_pins_after_state_mutation() {
    let source = include_str!("../src/browser/host/windows.rs");
    let start = source
        .find("pub fn acknowledge_attachment_projection(")
        .expect("Windows host attachment acknowledgement");
    let end = source[start..]
        .find("\n    fn ")
        .map(|offset| start + offset)
        .expect("next Windows host method");
    let body = &source[start..end];

    assert!(body.contains("draft_resource_ids_for_workspace"));
    assert!(body.contains("BrowserResourceStore::open_verified"));
    assert!(body.contains("acknowledge_attachment_projection_and_reconcile_pins"));
}

#[test]
fn attachment_acknowledgement_behavior_unpins_resolved_delivery_and_keeps_pending_pin() {
    let temp = TestDir::new("attachment-ack-pins");
    let key = workspace("project", "conversation");
    let store = BrowserResourceStore::open_verified(
        temp.path(),
        &key.project_id,
        BrowserResourceLimits::default(),
    )
    .unwrap();
    let screenshot_a = store
        .put(
            &key,
            BrowserResourceKind::AnnotationScreenshot,
            "image/png",
            b"a",
            true,
        )
        .unwrap();
    let screenshot_b = store
        .put(
            &key,
            BrowserResourceKind::AnnotationScreenshot,
            "image/png",
            b"b",
            true,
        )
        .unwrap();
    let mut state = BrowserHostState::new(temp.path());
    state
        .ensure_workspace(
            key.clone(),
            BrowserWorkspaceSnapshot {
                tabs: vec![BrowserTabSnapshot {
                    id: "tab-a".to_string(),
                    title: "A".to_string(),
                    url: "https://example.test".to_string(),
                    viewport: BrowserViewport::default(),
                }],
                selected_tab_id: Some("tab-a".to_string()),
                ..BrowserWorkspaceSnapshot::default()
            },
        )
        .unwrap();
    state
        .save_annotation(
            &key,
            stored_annotation("ann-a", screenshot_a.id.clone(), "resolved delivery"),
        )
        .unwrap();
    state.set_annotation_resolved(&key, "ann-a", true).unwrap();
    state
        .save_annotation(
            &key,
            stored_annotation("ann-b", screenshot_b.id.clone(), "pending"),
        )
        .unwrap();

    let snapshot = state.workspace(&key).unwrap().clone();
    let broker = BrowserAttachmentBroker::default();
    broker.observe_workspace(key.clone(), &snapshot);
    let initial = broker.dirty_projections().pop().unwrap();
    broker.acknowledge_dirty_projection(&initial);
    broker.detach(&key, "ann-a");
    let projection = broker.dirty_projections().pop().unwrap();

    let result = acknowledge_attachment_projection_and_reconcile_pins(
        &mut state,
        &store,
        &projection,
        BTreeSet::new(),
    )
    .unwrap();

    assert_eq!(result.pending_annotation_ids, vec!["ann-b"]);
    assert!(!store.handle(&key, &screenshot_a.id).unwrap().pinned);
    assert!(store.handle(&key, &screenshot_b.id).unwrap().pinned);
}

fn stored_annotation(
    id: &str,
    screenshot_resource: BrowserResourceId,
    comment: &str,
) -> BrowserAnnotation {
    BrowserAnnotation {
        id: id.to_string(),
        kind: BrowserAnnotationKind::Element,
        tab_id: "tab-a".to_string(),
        anchor_revision: BrowserRevision(1),
        comment: comment.to_string(),
        url: "https://example.test/form?authorization=super-secret".to_string(),
        locator: BrowserLocator {
            accessibility_role: Some("button".to_string()),
            accessibility_name: Some("Save".to_string()),
            test_id: Some("save".to_string()),
            css_selectors: vec!["[data-testid=save]".to_string()],
        },
        bounds: BrowserBounds {
            x: 10,
            y: 20,
            width: 120,
            height: 32,
        },
        viewport: BrowserViewport::default(),
        screenshot_resource,
        computed_styles: BTreeMap::from([("display".to_string(), "block".to_string())]),
        resolved: false,
    }
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

    fn new_relative(label: &str) -> Self {
        static NEXT_ID: AtomicU64 = AtomicU64::new(1);
        let path = PathBuf::from("target").join(format!(
            "devmanager-browser-host-{label}-{}-{}",
            std::process::id(),
            NEXT_ID.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&path).expect("create relative test directory");
        Self(path)
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[cfg(target_os = "windows")]
fn create_directory_redirect(target: &Path, link: &Path) -> std::io::Result<()> {
    let status = std::process::Command::new("cmd.exe")
        .args(["/c", "mklink", "/J"])
        .arg(link)
        .arg(target)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?;
    if status.success() {
        Ok(())
    } else {
        Err(std::io::Error::other("mklink /J failed"))
    }
}

#[cfg(not(target_os = "windows"))]
fn create_directory_redirect(target: &Path, link: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}

#[cfg(target_os = "windows")]
fn remove_directory_redirect(link: &Path) {
    let _ = std::fs::remove_dir(link);
}

#[cfg(not(target_os = "windows"))]
fn remove_directory_redirect(link: &Path) {
    let _ = std::fs::remove_file(link);
}

#[cfg(target_os = "windows")]
fn create_file_redirect(target: &Path, link: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(target)?;
    create_directory_redirect(target, link)?;
    std::fs::remove_dir(target)
}

#[cfg(not(target_os = "windows"))]
fn create_file_redirect(target: &Path, link: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}

#[cfg(target_os = "windows")]
fn remove_file_redirect(link: &Path) {
    remove_directory_redirect(link);
}

#[cfg(not(target_os = "windows"))]
fn remove_file_redirect(link: &Path) {
    let _ = std::fs::remove_file(link);
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
async fn command_requests_preserve_validated_invocation_context() {
    let key = workspace("project-a", "conversation-a");
    let (bridge, mut inbox) = browser_command_channel(4);
    let controller = bridge.bind(key, Duration::from_secs(1));
    let context = BrowserInvocationContext::agent(
        "Inspect the active page before submitting",
        BrowserRisk::Financial,
    )
    .expect("valid agent invocation");

    let response_task = tokio::spawn({
        let controller = controller.clone();
        let context = context.clone();
        async move {
            controller
                .request_with_context(BrowserCommand::Status, context)
                .await
        }
    });
    let request = inbox.recv().await.expect("context-bearing request");

    assert_eq!(request.context(), &context);
    assert_eq!(request.context().actor, BrowserInvocationActor::Agent);
    assert_eq!(request.context().declared_risk, BrowserRisk::Financial);
    assert!(!request.context().operation_id.trim().is_empty());
    request.respond(Ok(BrowserResponse::Acknowledged));
    assert_eq!(
        response_task.await.expect("context request task"),
        Ok(BrowserResponse::Acknowledged)
    );

    assert!(matches!(
        BrowserInvocationContext::agent("  ", BrowserRisk::Normal),
        Err(BrowserError::InvalidInvocation { field }) if field == "intent"
    ));
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
async fn wait_timeout_extends_the_controller_transport_deadline() {
    let key = workspace("project-a", "conversation-a");
    let (bridge, mut inbox) = browser_command_channel(4);
    let controller = bridge.bind(key, Duration::from_millis(20));

    let request_task = tokio::spawn(async move {
        controller
            .request(BrowserCommand::Wait {
                tab_id: "tab-a".to_string(),
                condition: BrowserWaitCondition::Duration { duration_ms: 1 },
                timeout_ms: 200,
            })
            .await
    });
    let request = inbox.recv().await.expect("wait request");
    tokio::time::sleep(Duration::from_millis(50)).await;
    let expected = BrowserResponse::Wait {
        result: BrowserWaitResult {
            matched: true,
            elapsed_ms: 50,
            revision: BrowserRevision(1),
        },
    };
    request.respond(Ok(expected.clone()));

    assert_eq!(request_task.await.expect("wait request task"), Ok(expected));
}

#[tokio::test]
async fn production_request_router_dispatches_open_agent_automation_and_leaves_it_pending() {
    let key = workspace("project-a", "conversation-a");
    let (bridge, mut inbox) = browser_command_channel(4);
    let controller = bridge.bind(key, Duration::from_secs(1));
    let context = BrowserInvocationContext::agent("capture fixture", BrowserRisk::Normal).unwrap();
    let request_task = tokio::spawn(async move {
        controller
            .request_with_context(
                BrowserCommand::Screenshot {
                    tab_id: "tab-a".to_string(),
                    mode: BrowserScreenshotMode::Viewport,
                },
                context,
            )
            .await
    });
    let request = inbox.recv().await.expect("automation request");
    let mut dispatched = None;

    route_browser_request(true, request, |request| dispatched = Some(request))
        .expect("open route dispatches to the host");
    tokio::task::yield_now().await;
    assert!(!request_task.is_finished());
    let request = dispatched.expect("host receives the original request");
    assert!(matches!(
        request.command(),
        BrowserCommand::Screenshot {
            mode: BrowserScreenshotMode::Viewport,
            ..
        }
    ));
    request.respond(Ok(BrowserResponse::Acknowledged));
    assert_eq!(
        request_task.await.expect("automation request task"),
        Ok(BrowserResponse::Acknowledged)
    );
}

#[tokio::test]
async fn production_request_router_rejects_closed_routes_without_dispatching() {
    let key = workspace("project-a", "conversation-a");
    let (bridge, mut inbox) = browser_command_channel(4);
    let controller = bridge.bind(key, Duration::from_secs(1));
    let request_task = tokio::spawn(async move {
        controller
            .request(BrowserCommand::Snapshot {
                tab_id: "tab-a".to_string(),
            })
            .await
    });
    let request = inbox.recv().await.expect("closed-route request");

    let error = route_browser_request(false, request, |_| panic!("closed route dispatched"))
        .expect_err("closed route is rejected");
    assert!(matches!(error, BrowserError::CrashedView { .. }));
    assert_eq!(request_task.await.expect("closed-route task"), Err(error));
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
async fn blocked_send_and_delayed_response_share_one_total_transport_deadline() {
    let key = workspace("project-a", "conversation-a");
    let (bridge, mut inbox) = browser_command_channel(1);
    let controller = bridge.bind(key, Duration::from_millis(80));
    controller
        .notify(BrowserCommand::Status)
        .await
        .expect("fill transport");
    let request = tokio::spawn({
        let controller = controller.clone();
        async move { controller.request(BrowserCommand::Status).await }
    });
    wait_for_pending_count(&bridge, 2).await;

    tokio::time::sleep(Duration::from_millis(50)).await;
    drop(inbox.recv().await.expect("release blocked send"));
    let delayed = inbox.recv().await.expect("request reaches host");
    tokio::time::sleep(Duration::from_millis(50)).await;
    delayed.respond(Ok(BrowserResponse::Acknowledged));

    assert_eq!(
        request.await.unwrap(),
        Err(BrowserError::Timeout {
            operation: "status".to_string(),
        })
    );
}

#[tokio::test]
async fn workspace_revoke_interrupts_a_request_blocked_on_full_transport() {
    let key = workspace("project-a", "conversation-a");
    let (bridge, inbox) = browser_command_channel(1);
    let controller = bridge.bind(key.clone(), Duration::from_secs(5));
    controller
        .notify(BrowserCommand::Status)
        .await
        .expect("fill transport");
    let blocked = tokio::spawn({
        let controller = controller.clone();
        async move { controller.request(BrowserCommand::Status).await }
    });
    wait_for_pending_count(&bridge, 2).await;

    bridge.interrupt_workspace(&key);
    assert_eq!(
        tokio::time::timeout(Duration::from_millis(100), blocked)
            .await
            .expect("revocation must beat the transport timeout")
            .unwrap(),
        Err(BrowserError::Interrupted)
    );
    assert_eq!(
        bridge.drain_host_controls(),
        vec![BrowserHostControl::InterruptWorkspace { workspace_key: key }]
    );
    drop(inbox);
    wait_for_pending_count(&bridge, 0).await;
}

#[test]
fn cancellation_ticket_and_watch_subscriptions_share_one_ordering_lock() {
    let source = include_str!("../src/browser/commands.rs");
    let start = source.find("fn cancellation_state_for_command(").unwrap();
    let end = source[start..].find("\n    }\n}").unwrap() + start;
    let state = &source[start..end];
    let lock = state.find("host_controls.with_locked").unwrap();
    let ticket = state.find(".ticket(").unwrap();
    let subscribe = state.find(".subscribe(").unwrap();
    assert!(lock < ticket && ticket < subscribe);
}

#[tokio::test]
async fn pending_work_is_observable_until_response_without_cancel_or_timeout_leaks() {
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
    assert_eq!(bridge.pending_work_count(), 1);
    assert_eq!(inbox.pending_work_count(), 1);
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
    assert_eq!(bridge.pending_work_count(), 1);
    assert_eq!(inbox.pending_work_count(), 1);
    drop(_queued_request);
    wait_for_pending_count(&bridge, 0).await;
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
    let stop_request = bridge.with_locked_host_work(|controls, mut lifecycle_requests| {
        assert!(controls.is_empty());
        assert_eq!(lifecycle_requests.len(), 1);
        lifecycle_requests.remove(0)
    });
    assert_eq!(
        stop_request.command(),
        &BrowserCommand::Stop {
            tab_id: Some("tab-a".to_string()),
        }
    );
    stop_request.respond(Ok(BrowserResponse::Acknowledged));

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
async fn stale_buffered_envelope_is_rejected_after_control_before_fresh_work_enters() {
    let key = workspace("project-a", "conversation-a");
    let (bridge, mut inbox) = browser_command_channel(8);
    let controller = bridge.bind(key.clone(), Duration::from_secs(1));
    let stale = tokio::spawn({
        let controller = controller.clone();
        async move {
            controller
                .request_with_context(
                    BrowserCommand::Status,
                    BrowserInvocationContext::new(
                        BrowserInvocationActor::Agent,
                        "stale buffered request",
                        BrowserRisk::Normal,
                        "stale-buffered",
                    )
                    .unwrap(),
                )
                .await
        }
    });
    wait_for_pending_count(&bridge, 1).await;

    bridge.interrupt_workspace(&key);
    assert_eq!(
        bridge.drain_host_controls(),
        vec![BrowserHostControl::InterruptWorkspace {
            workspace_key: key.clone(),
        }]
    );
    let fresh = tokio::spawn({
        let controller = controller.clone();
        async move {
            controller
                .request_with_context(
                    BrowserCommand::Status,
                    BrowserInvocationContext::new(
                        BrowserInvocationActor::Agent,
                        "fresh request",
                        BrowserRisk::Normal,
                        "fresh-after-interrupt",
                    )
                    .unwrap(),
                )
                .await
        }
    });

    let request = inbox.recv().await.expect("fresh request survives");
    assert_eq!(request.context().operation_id, "fresh-after-interrupt");
    assert!(request.cancellation_is_current());
    request.respond(Ok(BrowserResponse::Acknowledged));
    assert_eq!(stale.await.unwrap(), Err(BrowserError::Interrupted));
    assert_eq!(fresh.await.unwrap(), Ok(BrowserResponse::Acknowledged));
}

#[tokio::test]
async fn clear_profile_invalidates_buffered_project_envelopes_but_not_later_requests() {
    let first_key = workspace("project-a", "conversation-a");
    let second_key = workspace("project-a", "conversation-b");
    let (bridge, mut inbox) = browser_command_channel(8);
    let first = bridge.bind(first_key, Duration::from_secs(1));
    let second = bridge.bind(second_key, Duration::from_secs(1));
    let stale_first = tokio::spawn({
        let first = first.clone();
        async move { first.request(BrowserCommand::Status).await }
    });
    let stale_second = tokio::spawn({
        let second = second.clone();
        async move { second.request(BrowserCommand::Status).await }
    });
    wait_for_pending_count(&bridge, 2).await;

    first
        .notify(BrowserCommand::ClearProjectProfile)
        .await
        .unwrap();
    let fresh = tokio::spawn({
        let second = second.clone();
        async move {
            second
                .request_with_context(
                    BrowserCommand::Status,
                    BrowserInvocationContext::new(
                        BrowserInvocationActor::Agent,
                        "fresh after clear",
                        BrowserRisk::Normal,
                        "fresh-after-project-clear",
                    )
                    .unwrap(),
                )
                .await
        }
    });

    let clear = bridge.with_locked_host_work(|controls, mut lifecycle_requests| {
        assert!(controls.is_empty());
        assert_eq!(lifecycle_requests.len(), 1);
        lifecycle_requests.remove(0)
    });
    assert_eq!(clear.command(), &BrowserCommand::ClearProjectProfile);
    assert!(clear.cancellation_is_current());
    clear.respond(Ok(BrowserResponse::Acknowledged));
    let fresh_request = inbox
        .recv()
        .await
        .expect("post-clear request remains current");
    assert_eq!(
        fresh_request.context().operation_id,
        "fresh-after-project-clear"
    );
    fresh_request.respond(Ok(BrowserResponse::Acknowledged));

    assert_eq!(stale_first.await.unwrap(), Err(BrowserError::Interrupted));
    assert_eq!(stale_second.await.unwrap(), Err(BrowserError::Interrupted));
    assert_eq!(fresh.await.unwrap(), Ok(BrowserResponse::Acknowledged));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn direct_lifecycle_epoch_and_host_entry_are_one_atomic_ticket_barrier() {
    let key = workspace("project-a", "conversation-a");
    let (bridge, mut inbox) = browser_command_channel(8);
    let controller = bridge.bind(key.clone(), Duration::from_secs(1));
    let (entered_tx, entered_rx) = std::sync::mpsc::sync_channel(0);
    let (release_tx, release_rx) = std::sync::mpsc::sync_channel(0);
    let locked_bridge = bridge.clone();
    let locked_key = key.clone();
    let lifecycle = tokio::task::spawn_blocking(move || {
        locked_bridge.with_locked_host_controls_for_command(
            &locked_key,
            &BrowserCommand::ClearProjectProfile,
            |_, _| {
                entered_tx.send(()).unwrap();
                release_rx.recv().unwrap();
            },
        );
    });
    entered_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("lifecycle host entry holds the ticket barrier");

    let (attempted_tx, attempted_rx) = std::sync::mpsc::sync_channel(0);
    let fresh = tokio::spawn(async move {
        attempted_tx.send(()).unwrap();
        controller.request(BrowserCommand::Status).await
    });
    attempted_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("fresh request attempts ticket capture");
    assert!(
        tokio::time::timeout(Duration::from_millis(25), inbox.recv())
            .await
            .is_err()
    );
    assert_eq!(bridge.pending_work_count(), 0);

    release_tx.send(()).unwrap();
    lifecycle.await.unwrap();
    let request = inbox
        .recv()
        .await
        .expect("fresh request enters after lifecycle");
    assert!(request.cancellation_is_current());
    request.respond(Ok(BrowserResponse::Acknowledged));
    assert_eq!(fresh.await.unwrap(), Ok(BrowserResponse::Acknowledged));
}

#[test]
fn bridge_interrupts_publish_priority_host_controls() {
    let key = workspace("project-a", "conversation-a");
    let (bridge, _inbox) = browser_command_channel(4);

    bridge.interrupt_tab(&key, "tab-a");
    bridge.interrupt_workspace(&key);

    assert_eq!(
        bridge.drain_host_controls(),
        vec![
            BrowserHostControl::InterruptTab {
                workspace_key: key.clone(),
                tab_id: "tab-a".to_string(),
            },
            BrowserHostControl::InterruptWorkspace { workspace_key: key },
        ]
    );
}

#[test]
fn queued_workspace_interrupt_wins_the_locked_approval_resume_barrier() {
    let key = workspace("project-a", "conversation-a");
    let target = BrowserOperationTarget::new(key.clone(), "tab-a").unwrap();
    let (bridge, _inbox) = browser_command_channel(4);
    let mut queue = BrowserOperationQueue::default();
    assert_eq!(
        queue.enqueue(target.clone(), "approval-op", "active"),
        Some("active")
    );
    assert_eq!(queue.enqueue(target.clone(), "queued-op", "queued"), None);
    bridge.interrupt_workspace(&key);

    let can_resume = bridge.with_locked_host_controls(|controls| {
        for control in controls {
            match control {
                BrowserHostControl::InterruptProject { project_id } => {
                    let _ = queue.cancel_project(&project_id);
                }
                BrowserHostControl::InterruptWorkspace { workspace_key } => {
                    let _ = queue.cancel_workspace(&workspace_key);
                }
                BrowserHostControl::InterruptTab {
                    workspace_key,
                    tab_id,
                } => {
                    let target = BrowserOperationTarget::new(workspace_key, tab_id).unwrap();
                    let _ = queue.cancel_tab(&target);
                }
            }
        }
        queue.active_operation_id(&target) == Some("approval-op")
    });

    assert!(!can_resume);
    assert!(queue.is_empty());
    assert_eq!(queue.complete(&target, "approval-op"), None);
}

#[test]
fn queued_interrupt_is_consumed_before_a_later_host_entry_starts() {
    let key = workspace("project-a", "conversation-a");
    let target = BrowserOperationTarget::new(key.clone(), "tab-a").unwrap();
    let (bridge, _inbox) = browser_command_channel(4);
    let mut queue = BrowserOperationQueue::default();
    assert_eq!(queue.enqueue(target.clone(), "old-op", "old"), Some("old"));
    bridge.interrupt_workspace(&key);

    let started = bridge.with_locked_host_controls(|controls| {
        for control in controls {
            match control {
                BrowserHostControl::InterruptProject { project_id } => {
                    let _ = queue.cancel_project(&project_id);
                }
                BrowserHostControl::InterruptWorkspace { workspace_key } => {
                    let _ = queue.cancel_workspace(&workspace_key);
                }
                BrowserHostControl::InterruptTab {
                    workspace_key,
                    tab_id,
                } => {
                    let target = BrowserOperationTarget::new(workspace_key, tab_id).unwrap();
                    let _ = queue.cancel_tab(&target);
                }
            }
        }
        queue.enqueue(target.clone(), "new-op", "new")
    });

    assert_eq!(started, Some("new"));
    assert_eq!(queue.active_operation_id(&target), Some("new-op"));
    assert!(bridge.drain_host_controls().is_empty());
}

#[test]
fn every_destructive_lifecycle_command_maps_to_host_queue_cancellation() {
    let key = workspace("project-a", "conversation-a");
    for command in [
        BrowserCommand::Stop {
            tab_id: Some("tab-a".to_string()),
        },
        BrowserCommand::CloseTab {
            tab_id: "tab-a".to_string(),
        },
    ] {
        assert_eq!(
            browser_lifecycle_control(&key, &command),
            Some(BrowserHostControl::InterruptTab {
                workspace_key: key.clone(),
                tab_id: "tab-a".to_string(),
            })
        );
    }
    for command in [
        BrowserCommand::Stop { tab_id: None },
        BrowserCommand::ResetWorkspace,
    ] {
        assert_eq!(
            browser_lifecycle_control(&key, &command),
            Some(BrowserHostControl::InterruptWorkspace {
                workspace_key: key.clone(),
            })
        );
    }
    assert_eq!(
        browser_lifecycle_control(&key, &BrowserCommand::ClearProjectProfile),
        Some(BrowserHostControl::InterruptProject {
            project_id: key.project_id.clone(),
        })
    );
    assert_eq!(
        browser_lifecycle_control(&key, &BrowserCommand::Status),
        None
    );
}

#[test]
fn project_profile_clear_cancels_every_conversation_and_fences_late_completions() {
    let first =
        BrowserOperationTarget::new(workspace("project-a", "conversation-a"), "tab-a").unwrap();
    let second =
        BrowserOperationTarget::new(workspace("project-a", "conversation-b"), "tab-b").unwrap();
    let other =
        BrowserOperationTarget::new(workspace("project-b", "conversation-c"), "tab-c").unwrap();
    let mut queue = BrowserOperationQueue::default();
    assert_eq!(
        queue.enqueue(first.clone(), "first-active", "first-active"),
        Some("first-active")
    );
    assert_eq!(
        queue.enqueue(first.clone(), "first-queued", "first-queued"),
        None
    );
    assert_eq!(
        queue.enqueue(second.clone(), "second-active", "second-active"),
        Some("second-active")
    );
    assert_eq!(
        queue.enqueue(second.clone(), "second-queued", "second-queued"),
        None
    );
    assert_eq!(
        queue.enqueue(other.clone(), "other-active", "other-active"),
        Some("other-active")
    );

    let canceled = queue.cancel_project("project-a");
    assert_eq!(canceled.len(), 2);
    assert!(canceled
        .iter()
        .all(|(_, cancellation)| cancellation.queued.len() == 1));
    assert_eq!(queue.complete(&first, "first-active"), None);
    assert_eq!(queue.complete(&second, "second-active"), None);
    assert_eq!(queue.active_operation_id(&other), Some("other-active"));
}

#[test]
fn routed_stop_preempts_active_and_queued_host_work_and_fences_late_completion() {
    let key = workspace("project-a", "conversation-a");
    let target = BrowserOperationTarget::new(key.clone(), "tab-a").unwrap();
    let stop = BrowserCommand::Stop {
        tab_id: Some("tab-a".to_string()),
    };
    assert!(browser_request_preempts_operation_queue(&stop));
    let source = include_str!("../src/browser/host/windows.rs");
    let start = source.find("pub fn handle_request(").unwrap();
    let end = source[start..]
        .find("pub fn pump_async_completions")
        .unwrap()
        + start;
    let routed = &source[start..end];
    assert!(
        routed
            .find("browser_request_preempts_operation_queue")
            .unwrap()
            < routed.find("operation_queue").unwrap()
    );
    let mut queue = BrowserOperationQueue::default();
    assert_eq!(
        queue.enqueue(target.clone(), "active", "active"),
        Some("active")
    );
    assert_eq!(queue.enqueue(target.clone(), "queued", "queued"), None);

    let BrowserHostControl::InterruptTab {
        workspace_key,
        tab_id,
    } = browser_lifecycle_control(&key, &stop).unwrap()
    else {
        panic!("tab-scoped Stop must synchronously interrupt its host target");
    };
    let canceled = queue.cancel_tab(&BrowserOperationTarget::new(workspace_key, tab_id).unwrap());
    assert_eq!(canceled.active_operation_id.as_deref(), Some("active"));
    assert_eq!(canceled.queued, vec!["queued"]);
    assert_eq!(queue.complete(&target, "active"), None);
}

#[test]
fn synchronous_host_command_path_applies_the_shared_lifecycle_cancellation() {
    let source = include_str!("../src/browser/host/windows.rs");
    let start = source.find("pub fn handle_command(").unwrap();
    let end = source[start..]
        .find("pub fn handle_request(")
        .map(|offset| start + offset)
        .unwrap();
    let handler = &source[start..end];
    assert!(handler.contains("browser_lifecycle_control"));
    assert!(handler.contains("handle_control"));
}

#[test]
fn pending_approval_validation_requires_the_live_operation_and_approval_phase() {
    let source = include_str!("../src/browser/host/windows.rs");
    let start = source.find("pub fn is_pending_approval(").unwrap();
    let end = source[start..].find("pub fn resolve_approval(").unwrap() + start;
    let validation = &source[start..end];
    assert!(validation.contains("active_operation_id"));
    assert!(validation.contains("BrowserAsyncPhase::Approval"));
    assert!(validation.contains("cancellation_is_current"));
    assert!(validation.contains("cancel_tab_operations"));
}

#[test]
fn annotation_delete_enters_destructive_approval_before_mutation_and_resumes_distinctly() {
    assert_eq!(
        effective_browser_annotation_risk(BrowserRisk::Normal, BrowserAnnotationOperation::Delete,),
        BrowserRisk::Destructive
    );
    assert_eq!(
        effective_browser_annotation_risk(
            BrowserRisk::Financial,
            BrowserAnnotationOperation::Resolve,
        ),
        BrowserRisk::Financial
    );
    let source = include_str!("../src/browser/host/windows.rs");
    let start = source.find("fn begin_annotation_request(").unwrap();
    let end = source[start..].find("fn await_approval(").unwrap() + start;
    let gate = &source[start..end];
    let delete_risk = gate
        .find("effective_browser_annotation_risk")
        .expect("delete has a runtime risk override");
    let approval = gate
        .find("requires_confirmation")
        .expect("annotation operation checks approval policy");
    let pending = gate
        .find("BrowserApprovalResume::Annotation")
        .expect("annotation approval has a distinct resume path");
    let mutation = gate
        .find("self.handle_command(")
        .expect("approved annotation command reaches host mutation");
    assert!(delete_risk < approval && approval < pending && pending < mutation);

    let resolve_start = source.find("pub fn resolve_approval(").unwrap();
    let resolve_end = source[resolve_start..]
        .find("fn continue_actions(")
        .unwrap()
        + resolve_start;
    let resolution = &source[resolve_start..resolve_end];
    assert!(
        resolution.find("if !approved").unwrap()
            < resolution
                .find("BrowserApprovalResume::Annotation")
                .unwrap()
    );
    assert!(resolution.contains("BrowserError::BlockedPermission"));
    assert!(resolution.contains("begin_annotation_request(window"));
    assert!(resolution.contains("Some(risk)"));

    let response_start = source.find("fn respond_request(").unwrap();
    let response_end = source[response_start..]
        .find("fn cancel_tab_operations(")
        .unwrap()
        + response_start;
    let response = &source[response_start..response_end];
    assert!(response.contains("browser_command_is_journaled"));
    assert!(response.contains("append_journal_entry"));
    assert!(response.contains("reconcile_annotation_pins"));
    assert!(response.contains("if annotation_command"));
    assert!(response.contains("else if agent_journaled"));
    assert!(
        response.find("append_journal_entry").unwrap()
            < response
                .find("finalize_annotation_command_resources")
                .unwrap()
    );
}

#[test]
fn direct_annotation_commands_finalize_resource_pins_before_returning() {
    let source = include_str!("../src/browser/host/windows.rs");
    let start = source.find("pub fn handle_command(").unwrap();
    let end = source[start..].find("pub fn handle_control(").unwrap() + start;
    let handler = &source[start..end];
    let classify = handler
        .find("let annotation_command = matches!(&command, BrowserCommand::Annotations")
        .expect("direct command path classifies annotation commands");
    let dispatch = handler
        .find("self.handle_command_inner(")
        .expect("direct command path invokes the host command");
    let finalize = handler
        .find("self.finalize_annotation_command_resources(")
        .expect("direct command path finalizes annotation resources");
    assert!(classify < dispatch && dispatch < finalize);
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
        (BrowserCommand::WorkspaceState, "workspaceState"),
        (
            BrowserCommand::Ensure {
                snapshot: snapshot.clone(),
            },
            "ensure",
        ),
        (BrowserCommand::SetPaneOpen { open: true }, "setPaneOpen"),
        (
            BrowserCommand::Annotations {
                operation: BrowserAnnotationOperation::Resolve,
                annotation_id: Some("ann-a".to_string()),
            },
            "annotations",
        ),
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

    let response = BrowserResponse::WorkspaceState {
        snapshot: snapshot.clone(),
    };
    let value = serde_json::to_value(&response).expect("serialize workspace state response");
    assert_eq!(value["type"], "workspaceState");
    assert_eq!(
        serde_json::from_value::<BrowserResponse>(value).unwrap(),
        response
    );

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
fn automation_commands_are_typed_and_use_stable_group_names() {
    let target = BrowserActionTarget::default();
    let commands = vec![
        (
            BrowserCommand::Snapshot {
                tab_id: "tab-a".into(),
            },
            "snapshot",
        ),
        (
            BrowserCommand::Screenshot {
                tab_id: "tab-a".into(),
                mode: BrowserScreenshotMode::FullPage,
            },
            "screenshot",
        ),
        (
            BrowserCommand::Wait {
                tab_id: "tab-a".into(),
                condition: BrowserWaitCondition::Load,
                timeout_ms: 1_000,
            },
            "wait",
        ),
        (
            BrowserCommand::Act {
                tab_id: "tab-a".into(),
                actions: vec![BrowserAction::Click {
                    target: target.clone(),
                }],
            },
            "act",
        ),
        (
            BrowserCommand::Console {
                tab_id: "tab-a".into(),
                operation: BrowserConsoleOperation::List,
            },
            "console",
        ),
        (
            BrowserCommand::Network {
                tab_id: "tab-a".into(),
                operation: BrowserNetworkOperation::Body,
                request_id: Some("request-a".into()),
            },
            "network",
        ),
        (
            BrowserCommand::Performance {
                tab_id: "tab-a".into(),
                operation: BrowserPerformanceOperation::Snapshot,
            },
            "performance",
        ),
        (
            BrowserCommand::Upload {
                tab_id: "tab-a".into(),
                target,
                paths: vec![PathBuf::from("fixture.txt")],
            },
            "upload",
        ),
        (
            BrowserCommand::Downloads {
                tab_id: "tab-a".into(),
                operation: BrowserDownloadOperation::List,
                download_id: None,
            },
            "downloads",
        ),
        (
            BrowserCommand::Cdp {
                tab_id: "tab-a".into(),
                method: "Runtime.evaluate".into(),
                params: serde_json::json!({"expression": "1 + 1"}),
            },
            "cdp",
        ),
    ];

    for (command, expected_type) in commands {
        let value = serde_json::to_value(&command).expect("serialize automation command");
        assert_eq!(value["type"], expected_type);
        assert_eq!(value["tabId"], "tab-a");
        assert_eq!(
            serde_json::from_value::<BrowserCommand>(value).unwrap(),
            command
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
fn initialization_script_has_self_cleaning_element_and_region_annotation_overlay() {
    let script = browser_user_input_initialization_script();

    assert!(script.contains("annotationCandidate"));
    assert!(script.contains("annotation: {"));
    assert!(script.contains("setPointerCapture"));
    assert!(script.contains("elementFromPoint"));
    assert!(script.contains("kind: \"element\""));
    assert!(script.contains("kind: \"region\""));
    assert!(script.contains("removeEventListener"));
    assert!(script.contains("annotationOwnedMutation"));
    assert!(script.contains("const annotationOwnedNodes = new WeakSet()"));
    assert!(!script.contains("mutationObserver.takeRecords()"));
    assert!(script.contains("data-devmanager-annotation-overlay"));
    assert!(script.contains("computedStyle"));
    assert!(script.contains("fontSize"));
    assert!(script.contains("borderRadius"));
}

#[test]
fn annotation_overlay_emits_element_and_region_candidates_then_removes_every_owned_hook() {
    let harness = [
        r#"
const messages = [];
const windowListeners = new Map();
let activeObserver = null;
class FakeMutationObserver {
  constructor(callback) { this.callback = callback; this.records = []; activeObserver = this; }
  observe() {}
  takeRecords() { const records = this.records; this.records = []; return records; }
  record(record) { this.records.push(record); }
  flush() { if (this.records.length) { const records = this.takeRecords(); this.callback(records); } }
}
class FakeElement {
  constructor(tag) {
    this.tagName = tag.toUpperCase(); this.children = []; this.parentElement = null;
    const style = {};
    this.style = new Proxy(style, { set: (target, key, value) => { target[key] = value; activeObserver?.record({ target: this, attributeName: "style" }); return true; } });
    this.dataset = {}; this.listeners = new Map(); this.attributes = new Map();
    this.innerText = ""; this.id = "";
  }
  setAttribute(key, value) { this.attributes.set(key, String(value)); }
  getAttribute(key) { return this.attributes.has(key) ? this.attributes.get(key) : null; }
  hasAttribute(key) { return this.attributes.has(key); }
  addEventListener(type, handler) { this.listeners.set(type, handler); }
  removeEventListener(type, handler) { if (this.listeners.get(type) === handler) this.listeners.delete(type); }
  appendChild(child) { child.parentElement = this; this.children.push(child); activeObserver?.record({ target: this, addedNodes: [child], removedNodes: [] }); return child; }
  remove() { if (!this.parentElement) return; const parent = this.parentElement; parent.children = parent.children.filter((child) => child !== this); this.parentElement = null; activeObserver?.record({ target: parent, addedNodes: [], removedNodes: [this] }); }
  setPointerCapture() {}
  getBoundingClientRect() { return this.bounds || { x: 10, y: 12, width: 30, height: 20 }; }
  matches() { return false; }
  closest() { return null; }
  dispatch(type, values) { this.listeners.get(type)?.({ isTrusted: true, preventDefault() {}, stopPropagation() {}, pointerId: 1, ...values }); }
}
globalThis.Element = FakeElement;
globalThis.MutationObserver = FakeMutationObserver;
globalThis.PerformanceObserver = class { observe() {} };
globalThis.XMLHttpRequest = class {};
XMLHttpRequest.prototype.open = function() {};
XMLHttpRequest.prototype.send = function() {};
globalThis.CSS = { escape: (value) => String(value) };
globalThis.location = new URL("https://example.test/form");
globalThis.performance = { now: () => 0, getEntriesByType: () => [] };
globalThis.getComputedStyle = () => ({ display: "block", visibility: "visible", opacity: "1", position: "static", color: "rgb(0, 0, 0)", backgroundColor: "rgb(255, 255, 255)", fontFamily: "sans-serif", fontSize: "14px", fontWeight: "400", border: "0px none", borderRadius: "4px", padding: "2px", margin: "0px" });
const body = new FakeElement("body");
const documentElement = new FakeElement("html");
const target = new FakeElement("button"); target.innerText = "Save"; target.setAttribute("role", "button"); target.setAttribute("data-testid", "");
target.bounds = { x: -20, y: -5, width: 140, height: 70 };
const spoofedPageNode = new FakeElement("section"); spoofedPageNode.setAttribute("data-devmanager-annotation-overlay", "true");
globalThis.document = {
  body, documentElement,
  createElement: (tag) => new FakeElement(tag),
  querySelector: () => null,
  querySelectorAll: () => [],
  elementFromPoint: () => target,
  activeElement: target,
};
globalThis.window = {
  innerWidth: 100, innerHeight: 50, devicePixelRatio: 2,
  ipc: { postMessage: (message) => messages.push(JSON.parse(message)) },
  addEventListener(type, handler) { const values = windowListeners.get(type) || []; values.push(handler); windowListeners.set(type, values); },
  removeEventListener(type, handler) { windowListeners.set(type, (windowListeners.get(type) || []).filter((value) => value !== handler)); },
};
"#,
        browser_user_input_initialization_script(),
        r#"
(async () => {
const annotation = window.__devmanagerBrowser.annotation;
documentElement.appendChild(spoofedPageNode);
annotation.start({ url: "https://example.test/form", revision: 7 });
activeObserver.flush();
await new Promise((resolve) => setTimeout(resolve, 75));
if (messages.filter((message) => message.type === "domMutation").length !== 1) throw new Error("spoofed page marker mutation was suppressed");
let overlay = body.children[0];
overlay.dispatch("pointerdown", { clientX: 12, clientY: 14 });
overlay.dispatch("pointerup", { clientX: 12, clientY: 14 });
if (body.children.length !== 0) throw new Error("element overlay leaked");
if ((windowListeners.get("keydown") || []).length !== 1) throw new Error("base key listener count changed");

annotation.start({ url: "https://example.test/form", revision: 7 });
overlay = body.children[0];
overlay.dispatch("pointerdown", { clientX: 4, clientY: 5 });
activeObserver.record({ target, attributeName: "data-real-during-overlay" });
overlay.dispatch("pointermove", { clientX: 44, clientY: 25 });
overlay.dispatch("pointerup", { clientX: 44, clientY: 25 });
activeObserver.flush();
await new Promise((resolve) => setTimeout(resolve, 75));
if (messages.filter((message) => message.type === "domMutation").length !== 2) throw new Error("real mutation during overlay was suppressed or duplicated");
if (body.children.length !== 0) throw new Error("region overlay leaked");

annotation.start({ url: "https://example.test/form", revision: 7 });
for (const handler of [...(windowListeners.get("keydown") || [])]) handler({ isTrusted: true, key: "Escape", preventDefault() {}, stopPropagation() {} });
activeObserver.flush();
if (body.children.length !== 0) throw new Error("escape overlay leaked");
await new Promise((resolve) => setTimeout(resolve, 75));
const mutations = messages.filter((message) => message.type === "domMutation");
if (mutations.length !== 2) throw new Error(`overlay-only mutation escaped filtering: ${mutations.length}`);
const candidates = messages.filter((message) => message.type === "annotationCandidate").map((message) => message.candidate);
if (candidates.length !== 2 || candidates[0].kind !== "element" || candidates[1].kind !== "region") throw new Error("candidate kinds missing");
if (candidates[0].locator.testId !== null || candidates[0].locator.accessibilityName !== "Save" || candidates[0].computedStyles.fontSize !== "14px") throw new Error("semantic metadata normalization missing");
if (JSON.stringify(candidates[0].bounds) !== JSON.stringify({ x: 0, y: 0, width: 100, height: 50 })) throw new Error("element bounds were not viewport-clamped");
if (!messages.some((message) => message.type === "annotationCanceled")) throw new Error("escape cancellation missing");
process.stdout.write(JSON.stringify({ candidates: candidates.length, children: body.children.length }));
})().catch((error) => { console.error(error); process.exitCode = 1; });
"#,
    ]
    .concat();
    let temp = TestDir::new("annotation-overlay-node");
    let harness_path = temp.path().join("harness.js");
    std::fs::write(&harness_path, harness).expect("write annotation overlay harness");
    let output = Command::new("node")
        .arg(&harness_path)
        .output()
        .expect("execute annotation overlay in Node");
    assert!(
        output.status.success(),
        "Node harness failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        r#"{"candidates":2,"children":0}"#
    );
}

fn annotation_candidate() -> BrowserAnnotationCandidate {
    BrowserAnnotationCandidate {
        kind: BrowserAnnotationKind::Element,
        url: "https://example.test/form".to_string(),
        revision: BrowserRevision(7),
        locator: BrowserLocator {
            accessibility_role: Some("button".to_string()),
            accessibility_name: Some("Save".to_string()),
            test_id: Some("save".to_string()),
            css_selectors: vec!["[data-testid=save]".to_string()],
        },
        bounds: BrowserBounds {
            x: 10,
            y: 12,
            width: 30,
            height: 20,
        },
        viewport: BrowserViewport {
            width: 100,
            height: 50,
            scale_percent: 200,
        },
        computed_styles: BTreeMap::from([
            ("display".to_string(), "block".to_string()),
            ("fontSize".to_string(), "14px".to_string()),
        ]),
    }
}

#[test]
fn annotation_candidate_validation_is_strict_and_style_allowlisted() {
    annotation_candidate().validate().unwrap();

    let mut unknown_style = annotation_candidate();
    unknown_style
        .computed_styles
        .insert("backgroundImage".to_string(), "url(secret)".to_string());
    assert!(matches!(
        unknown_style.validate(),
        Err(BrowserError::InvalidAnnotation { field, .. }) if field == "computedStyles"
    ));

    let mut outside_viewport = annotation_candidate();
    outside_viewport.bounds.x = 101;
    assert!(matches!(
        outside_viewport.validate(),
        Err(BrowserError::InvalidAnnotation { field, .. }) if field == "bounds"
    ));

    let encoded = serde_json::to_value(annotation_candidate()).unwrap();
    let mut unknown = encoded.as_object().unwrap().clone();
    unknown.insert("pageHtml".to_string(), serde_json::json!("<secret>"));
    assert!(serde_json::from_value::<BrowserAnnotationCandidate>(unknown.into()).is_err());

    for nested in ["locator", "bounds", "viewport"] {
        let mut encoded = serde_json::to_value(annotation_candidate()).unwrap();
        encoded[nested]
            .as_object_mut()
            .unwrap()
            .insert("untrustedExtra".to_string(), serde_json::json!(true));
        assert!(
            serde_json::from_value::<BrowserAnnotationCandidate>(encoded.clone()).is_err(),
            "accepted unknown nested {nested} field"
        );
        let body = serde_json::json!({
            "type": "annotationCandidate",
            "candidate": encoded,
        })
        .to_string();
        assert!(
            parse_browser_annotation_ipc_message(&body).is_err(),
            "IPC accepted unknown nested {nested} field"
        );
    }

    for bounds in [
        BrowserBounds {
            x: -1,
            y: 0,
            width: 1,
            height: 1,
        },
        BrowserBounds {
            x: 0,
            y: 0,
            width: 0,
            height: 1,
        },
        BrowserBounds {
            x: i32::MAX,
            y: 0,
            width: 2,
            height: 1,
        },
        BrowserBounds {
            x: 90,
            y: 0,
            width: 20,
            height: 1,
        },
        BrowserBounds {
            x: 0,
            y: 45,
            width: 1,
            height: 10,
        },
    ] {
        let mut invalid = annotation_candidate();
        invalid.bounds = bounds;
        assert!(invalid.validate().is_err(), "accepted bounds {bounds:?}");
    }

    let mut oversized = annotation_candidate();
    oversized.locator.accessibility_name = Some("n".repeat(1_001));
    assert!(oversized.validate().is_err());
    let mut too_many_selectors = annotation_candidate();
    too_many_selectors.locator.css_selectors = vec!["button".to_string(); 9];
    assert!(too_many_selectors.validate().is_err());
    let mut oversized_selector = annotation_candidate();
    oversized_selector.locator.css_selectors = vec!["x".repeat(513)];
    assert!(oversized_selector.validate().is_err());
    let mut oversized_style = annotation_candidate();
    oversized_style
        .computed_styles
        .insert("display".to_string(), "x".repeat(257));
    assert!(oversized_style.validate().is_err());
}

#[test]
fn annotation_ipc_is_bounded_before_deserialize_and_context_is_host_verified() {
    let candidate = annotation_candidate();
    let body = serde_json::json!({
        "type": "annotationCandidate",
        "candidate": candidate,
    })
    .to_string();
    let parsed = parse_browser_annotation_ipc_message(&body).unwrap();
    validate_annotation_candidate_context(&parsed, "https://example.test/form", BrowserRevision(7))
        .unwrap();

    assert!(matches!(
        validate_annotation_candidate_context(
            &parsed,
            "https://example.test/other",
            BrowserRevision(7)
        ),
        Err(BrowserError::InvalidAnnotation { field, .. }) if field == "url"
    ));
    assert!(matches!(
        validate_annotation_candidate_context(
            &parsed,
            "https://example.test/form",
            BrowserRevision(8)
        ),
        Err(BrowserError::StaleReference { .. })
    ));
    assert!(matches!(
        parse_browser_annotation_ipc_message(&"x".repeat(32_769)),
        Err(BrowserError::InvalidAnnotation { field, .. }) if field == "ipcBody"
    ));
}

#[test]
fn annotation_lifecycle_is_route_owned_and_cancels_modes_and_drafts() {
    let workspace_a = workspace("project-a", "conversation-a");
    let workspace_b = workspace("project-a", "conversation-b");
    let route_a = BrowserAnnotationRoute::new(workspace_a.clone(), "tab-a").unwrap();
    let route_b = BrowserAnnotationRoute::new(workspace_b.clone(), "tab-b").unwrap();
    let mut lifecycle = BrowserAnnotationLifecycle::default();
    lifecycle.activate(
        route_a.clone(),
        "https://example.test/form",
        BrowserRevision(7),
    );
    assert!(lifecycle.is_active(&route_a));
    assert!(lifecycle
        .accept_candidate(&route_b, annotation_candidate())
        .is_err());
    assert!(lifecycle.is_active(&route_a));
    lifecycle
        .accept_candidate(&route_a, annotation_candidate())
        .unwrap();
    assert!(!lifecycle.is_active(&route_a));

    let draft = BrowserAnnotationDraft::new(
        "tab-a",
        annotation_candidate(),
        devmanager::browser::BrowserResourceId(format!("res-{}", "0".repeat(32))),
    )
    .unwrap();
    let draft_id = draft.id.clone();
    assert!(matches!(
        draft.clone().into_annotation(" \n "),
        Err(BrowserError::InvalidAnnotation { field, .. }) if field == "comment"
    ));
    let saved = draft
        .clone()
        .into_annotation("Review the Save button")
        .unwrap();
    assert!(saved.id.starts_with("ann-"));
    assert_eq!(saved.tab_id, "tab-a");
    assert_eq!(saved.anchor_revision, BrowserRevision(7));
    lifecycle.store_draft(route_a.clone(), draft).unwrap();
    assert!(matches!(
        lifecycle.take_draft(&workspace_b, &draft_id),
        Err(BrowserError::BlockedPermission { .. })
    ));
    assert_eq!(lifecycle.cancel_route(&route_a).len(), 1);
    assert!(lifecycle.take_draft(&workspace_a, &draft_id).is_err());

    lifecycle.activate(
        route_a.clone(),
        "https://example.test/form",
        BrowserRevision(7),
    );
    lifecycle.activate(
        route_b.clone(),
        "https://example.test/form",
        BrowserRevision(7),
    );
    lifecycle.cancel_workspace(&workspace_a);
    assert!(!lifecycle.is_active(&route_a));
    assert!(lifecycle.is_active(&route_b));
    lifecycle.cancel_project("project-a");
    assert!(!lifecycle.is_active(&route_b));
}

#[test]
fn annotation_cleanup_ledger_retries_failed_unpins_without_crossing_owners() {
    let workspace_a = workspace("project-a", "conversation-a");
    let workspace_b = workspace("project-a", "conversation-b");
    let route_a = BrowserAnnotationRoute::new(workspace_a.clone(), "tab-a").unwrap();
    let resource = BrowserResourceId(format!("res-{}", "a".repeat(32)));
    let mut ledger = BrowserAnnotationCleanupLedger::default();
    assert!(ledger.enqueue(route_a.clone(), resource.clone()));
    assert!(!ledger.enqueue(route_a.clone(), resource.clone()));

    let mut wrong_owner_attempts = 0;
    assert!(ledger
        .retry_workspace(&workspace_b, |_| {
            wrong_owner_attempts += 1;
            Ok(())
        })
        .is_empty());
    assert_eq!(wrong_owner_attempts, 0);
    assert_eq!(ledger.pending_for_workspace(&workspace_a).len(), 1);

    let failures = ledger.retry_workspace(&workspace_a, |cleanup| {
        assert_eq!(cleanup.route, route_a);
        assert_eq!(cleanup.resource_id, resource);
        Err(BrowserError::Io {
            operation: "unpin annotation screenshot".to_string(),
            path: PathBuf::from("locked-metadata.json"),
            message: "sharing violation".to_string(),
        })
    });
    assert_eq!(failures.len(), 1);
    assert_eq!(ledger.pending_for_workspace(&workspace_a).len(), 1);

    assert!(ledger.retry_workspace(&workspace_a, |_| Ok(())).is_empty());
    assert!(ledger.pending_for_workspace(&workspace_a).is_empty());

    assert!(ledger.enqueue(route_a, resource));
    assert!(ledger
        .retry_workspace(&workspace_a, |cleanup| {
            Err(BrowserError::MissingResource {
                id: cleanup.resource_id.clone(),
            })
        })
        .is_empty());
    assert!(ledger.pending_for_workspace(&workspace_a).is_empty());
}

#[test]
fn live_draft_screenshot_survives_unrelated_journal_reconciliation_and_quota_cleanup() {
    let source = include_str!("../src/browser/host/windows.rs");
    let start = source.find("fn reconcile_annotation_pins(").unwrap();
    let end = source[start..]
        .find("fn refresh_annotation_response_handles(")
        .unwrap()
        + start;
    let reconciliation = &source[start..end];
    assert!(reconciliation.contains("annotation_lifecycle"));
    assert!(reconciliation.contains("draft_resource_ids_for_workspace"));

    let temp = TestDir::new("annotation-live-draft-pin");
    let store = BrowserResourceStore::open(
        temp.path(),
        BrowserResourceLimits {
            max_temporary_count: 1,
            max_temporary_bytes: 1024,
            max_resource_bytes: 1024,
        },
    )
    .unwrap();
    let key = workspace("project-a", "conversation-a");
    let screenshot = store
        .put(
            &key,
            BrowserResourceKind::AnnotationScreenshot,
            "image/png",
            b"draft screenshot",
            true,
        )
        .unwrap();
    let route = BrowserAnnotationRoute::new(key.clone(), "tab-a").unwrap();
    let draft = BrowserAnnotationDraft::new("tab-a", annotation_candidate(), screenshot.id.clone())
        .unwrap();
    let draft_id = draft.id.clone();
    let mut lifecycle = BrowserAnnotationLifecycle::default();
    lifecycle.store_draft(route, draft).unwrap();
    let mut snapshot = BrowserWorkspaceSnapshot::default();
    snapshot.append_journal_entry(BrowserJournalEntry {
        id: "unrelated-agent-action".to_string(),
        actor: BrowserJournalActor::Agent,
        intent: "inspect console".to_string(),
        url: "https://example.test/form".to_string(),
        started_at: "2026-07-16T00:00:00Z".to_string(),
        duration_ms: 1,
        result: "ok".to_string(),
        resource_ids: Vec::new(),
    });
    let mut pinned = snapshot.pinned_annotation_resource_ids();
    pinned.extend(lifecycle.draft_resource_ids_for_workspace(&key));
    store.reconcile_annotation_pins(&key, &pinned).unwrap();
    store
        .put(
            &key,
            BrowserResourceKind::Other,
            "text/plain",
            b"temporary one",
            false,
        )
        .unwrap();
    store
        .put(
            &key,
            BrowserResourceKind::Other,
            "text/plain",
            b"temporary two",
            false,
        )
        .unwrap();
    assert!(store.handle(&key, &screenshot.id).unwrap().pinned);

    lifecycle.take_draft(&key, &draft_id).unwrap();
    store
        .reconcile_annotation_pins(&key, &snapshot.pinned_annotation_resource_ids())
        .unwrap();
    store
        .put(
            &key,
            BrowserResourceKind::Other,
            "text/plain",
            b"temporary three",
            false,
        )
        .unwrap();
    assert!(matches!(
        store.handle(&key, &screenshot.id),
        Err(BrowserError::MissingResource { .. })
    ));
}

#[test]
fn annotation_save_and_cancel_transitions_are_failure_safe() {
    let source = include_str!("../src/browser/host/windows.rs");
    let commands = &source[source.find("fn handle_available_command(").unwrap()..];
    let save_start = commands
        .find("BrowserCommand::SaveAnnotationDraft { draft_id, comment } =>")
        .unwrap();
    let cancel_start = commands
        .find("BrowserCommand::CancelAnnotationDraft { draft_id } =>")
        .unwrap();
    let list_start = commands.find("BrowserCommand::ListTabs =>").unwrap();
    let save = &commands[save_start..cancel_start];
    let cancel = &commands[cancel_start..list_start];

    assert!(save.contains("restore_draft(route, draft)"));
    assert!(save.contains("if let Err(error) = self.reconcile_annotation_pins(workspace_key)"));
    assert!(save.contains("Ok(BrowserResponse::Workspace { mutation })"));
    assert!(cancel.contains("if let Err(error) ="));
    assert!(cancel.contains("restore_draft(route, draft)"));
    assert!(cancel.contains("return Err(error)"));

    let completion_start = source.find("fn complete_annotation_capture(").unwrap();
    let completion_end = source[completion_start..]
        .find("fn complete_async_operation(")
        .unwrap()
        + completion_start;
    let completion = &source[completion_start..completion_end];
    assert!(completion.contains("let draft = match BrowserAnnotationDraft::new("));
    assert_eq!(completion.matches("queue_annotation_cleanup(").count(), 2);
    assert!(completion.contains("return Err(error);"));

    for (start, end) in [
        ("fn cancel_annotation_route(", "fn cancel_annotation_mode("),
        (
            "fn cancel_workspace_annotations(",
            "fn cancel_project_annotations(",
        ),
        (
            "fn cancel_project_annotations(",
            "fn queue_annotation_cleanup(",
        ),
    ] {
        let start = source.find(start).unwrap();
        let end = source[start..].find(end).unwrap() + start;
        let cancellation = &source[start..end];
        assert!(cancellation.contains("annotation_cleanup"));
        assert!(cancellation.contains("retry_annotation_cleanups"));
    }
}

#[test]
fn annotation_crop_maps_css_viewport_bounds_to_png_pixels_and_clamps() {
    let mut source = image::RgbaImage::new(200, 100);
    for (x, y, pixel) in source.enumerate_pixels_mut() {
        *pixel = image::Rgba([x as u8, y as u8, 90, 255]);
    }
    let mut encoded = Cursor::new(Vec::new());
    image::DynamicImage::ImageRgba8(source)
        .write_to(&mut encoded, image::ImageFormat::Png)
        .unwrap();

    let candidate = annotation_candidate();
    let crop =
        crop_annotation_png(encoded.get_ref(), candidate.bounds, &candidate.viewport).unwrap();
    let decoded = image::load_from_memory(&crop).unwrap();
    assert_eq!((decoded.width(), decoded.height()), (60, 40));

    assert!(crop_annotation_png(
        encoded.get_ref(),
        BrowserBounds {
            x: 250,
            y: 250,
            width: 10,
            height: 10,
        },
        &candidate.viewport,
    )
    .is_err());
}

#[test]
fn initialization_script_coalesces_dom_mutations_and_bounds_redacted_telemetry() {
    let script = browser_user_input_initialization_script();
    assert!(script.contains("MutationObserver"));
    assert!(script.contains("domMutation"));
    assert!(script.contains("mutationTimer"));
    assert!(script.contains("MAX_CONSOLE"));
    assert!(script.contains("MAX_NETWORK"));
    assert!(script.contains("PerformanceObserver"));
    assert!(script.contains("XMLHttpRequest"));
    assert!(script.contains("window.fetch"));
    assert!(script.contains("[redacted]"));
    assert!(script.contains("authorization"));
    assert!(script.contains("cookie"));
    assert!(script.contains("redactStructured"));
    assert!(script.contains("Basic\\s+"));
    assert!(script.contains("SECRET_KEY_SUFFIXES"));
    assert!(script.contains("normalized.endsWith(suffix)"));
    assert!(script.contains("normalized.startsWith(prefix)"));
    assert!(script.contains("return redact(typeof arg"));
    assert!(!script.contains("postMessage(JSON.stringify({ type: \"telemetry\""));
}

#[test]
fn initialization_script_redacts_secrets_split_across_console_arguments() {
    let harness = format!(
        r#"
globalThis.window = {{ addEventListener() {{}}, ipc: {{ postMessage() {{}} }} }};
globalThis.document = {{}};
globalThis.location = new URL("https://example.test/");
globalThis.performance = {{ now: () => 0, getEntriesByType: () => [] }};
globalThis.MutationObserver = class {{ observe() {{}} }};
globalThis.PerformanceObserver = class {{ observe() {{}} }};
globalThis.XMLHttpRequest = class {{}};
XMLHttpRequest.prototype.open = function() {{}};
XMLHttpRequest.prototype.send = function() {{}};
globalThis.console = {{ debug() {{}}, info() {{}}, log() {{}}, warn() {{}}, error() {{}} }};
{}
console.log("token=", "split-token-value", "safe-tail");
console.warn("Bearer", "split-bearer-value", "safe-warning");
const messages = window.__devmanagerBrowser.console("list").map((entry) => entry.message);
process.stdout.write(JSON.stringify(messages));
"#,
        browser_user_input_initialization_script()
    );
    let output = Command::new("node")
        .args(["-e", &harness])
        .output()
        .expect("execute initialization script in Node");
    assert!(
        output.status.success(),
        "Node harness failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let messages = String::from_utf8(output.stdout).unwrap();
    assert!(!messages.contains("split-token-value"), "{messages}");
    assert!(!messages.contains("split-bearer-value"), "{messages}");
    assert!(messages.contains("[redacted]"), "{messages}");
    assert!(messages.contains("safe-tail"), "{messages}");
    assert!(messages.contains("safe-warning"), "{messages}");
}

#[test]
fn initialization_script_never_uses_password_values_as_accessible_names() {
    let script = browser_user_input_initialization_script();
    let start = script.find("const nameOf").expect("name helper");
    let end = script[start..].find("const isVisible").unwrap() + start;
    let name_helper = &script[start..end];

    assert!(name_helper.contains("isPasswordElement"));
    assert!(name_helper.contains("valueFallback"));
}

#[test]
fn initialization_script_inspects_active_keypress_and_both_drag_targets_before_acting() {
    let script = browser_user_input_initialization_script();
    let start = script
        .find("inspectTargets:")
        .expect("runtime target inspection");
    let end = script[start..].find("act:").unwrap() + start;
    let inspection = &script[start..end];

    assert!(inspection.contains("flatMap"));
    assert!(inspection.contains("action.operation === \"keypress\""));
    assert!(inspection.contains("document.activeElement"));
    assert!(inspection.contains("resolveTarget(action.source)"));
    assert!(inspection.contains("resolveTarget(action.destination)"));
}

#[test]
fn windows_ipc_routes_dom_mutations_and_all_trusted_input_kinds() {
    let windows_host = include_str!("../src/browser/host/windows.rs");

    let handler = &windows_host[windows_host.find(".with_ipc_handler").unwrap()..];
    assert!(handler.contains("parse_browser_page_ipc_message(body)"));
    assert!(handler.contains("BrowserPageIpcMessage::DomMutation"));
    assert!(windows_host.contains("BrowserHostEvent::DomMutation"));
    for (encoded, expected) in [
        ("pointer", BrowserUserInputKind::Pointer),
        ("keyboard", BrowserUserInputKind::Keyboard),
        ("textInput", BrowserUserInputKind::TextInput),
    ] {
        assert_eq!(
            parse_browser_page_ipc_message(
                &serde_json::json!({"type": "userInput", "kind": encoded}).to_string()
            )
            .unwrap(),
            BrowserPageIpcMessage::UserInput { kind: expected }
        );
    }
}

#[test]
fn windows_host_promotes_large_performance_snapshots_to_resources() {
    let source = include_str!("../src/browser/host/windows.rs");
    let start = source.find("fn complete_performance").unwrap();
    let end = source[start..].find("fn complete_cdp").unwrap() + start;
    let completion = &source[start..end];

    assert!(completion.contains("encoded.len() > INLINE_RESULT_LIMIT"));
    assert!(completion.contains("BrowserResourceKind::PerformanceTrace"));
}

#[test]
fn windows_host_redacts_cdp_json_before_inline_or_resource_selection() {
    let source = include_str!("../src/browser/host/windows.rs");
    let start = source.find("fn complete_cdp(").unwrap();
    let end = source[start..]
        .find("fn continue_upload_after_mark")
        .unwrap()
        + start;
    let complete = &source[start..end];
    let redact = complete
        .find("redact_browser_resource_bytes(\"application/json\"")
        .unwrap();
    let parse = complete.find("serde_json::from_slice(&redacted)").unwrap();
    let promote = complete
        .find("redacted.len() > INLINE_RESULT_LIMIT")
        .unwrap();
    let store = complete[promote..].find("&redacted").unwrap() + promote;
    assert!(redact < parse && parse < promote && promote < store);
}

#[test]
fn windows_permission_requests_use_devmanager_confirmation_and_never_default_grant() {
    let source = include_str!("../src/browser/host/windows.rs");

    assert!(source.contains("PermissionRequestedEventHandler"));
    assert!(source.contains("Confirm Browser Permission"));
    assert!(source.contains("COREWEBVIEW2_PERMISSION_STATE_ALLOW"));
    assert!(source.contains("COREWEBVIEW2_PERMISSION_STATE_DENY"));
    assert!(!source.contains("COREWEBVIEW2_PERMISSION_STATE_DEFAULT"));
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
fn relative_download_roots_do_not_treat_the_empty_ancestor_as_a_directory() {
    let temp = TestDir::new_relative("relative-download-root");
    let downloads = temp.path().join("nested").join("downloads");

    let selected = unique_download_path(&downloads, Path::new("report.pdf")).unwrap();

    assert_eq!(selected, downloads.join("report.pdf"));
    assert!(downloads.is_dir());
}

#[test]
fn dangling_download_leaf_redirect_is_occupied_and_never_followed() {
    let temp = TestDir::new("dangling-download-leaf");
    let downloads = temp.path().join("downloads");
    std::fs::create_dir_all(&downloads).unwrap();
    let outside = temp.path().join("outside-report.pdf");
    let redirect = downloads.join("report.pdf");
    create_file_redirect(&outside, &redirect).expect("create dangling file redirect");

    let selected = unique_download_path(&downloads, Path::new("report.pdf")).unwrap();
    assert_eq!(selected, downloads.join("report (1).pdf"));
    std::fs::write(&selected, b"inside trusted downloads").unwrap();
    assert!(
        !outside.exists(),
        "download write escaped through dangling redirect"
    );

    remove_file_redirect(&redirect);
}

#[test]
fn download_path_selection_rejects_direct_and_intermediate_directory_redirects() {
    let temp = TestDir::new("download-redirects");
    let outside = temp.path().join("outside");
    std::fs::create_dir_all(&outside).unwrap();

    let direct = temp.path().join("direct-downloads");
    create_directory_redirect(&outside, &direct).expect("create direct directory redirect");
    assert!(matches!(
        unique_download_path(&direct, Path::new("escape.txt")),
        Err(BrowserError::OutsideWorkspace { .. })
    ));
    remove_directory_redirect(&direct);

    let trusted = temp.path().join("trusted-config");
    std::fs::create_dir_all(&trusted).unwrap();
    let redirected_browser = trusted.join("browser");
    create_directory_redirect(&outside, &redirected_browser)
        .expect("create intermediate directory redirect");
    let nested_downloads = redirected_browser.join("downloads").join("project-hash");
    assert!(matches!(
        unique_download_path(&nested_downloads, Path::new("escape.txt")),
        Err(BrowserError::OutsideWorkspace { .. })
    ));
    remove_directory_redirect(&redirected_browser);
}

#[test]
fn verified_download_root_enforces_the_app_config_trust_boundary() {
    let temp = TestDir::new("verified-download-root");
    let trusted = temp.path().join("trusted-config");
    let prepared = prepare_verified_download_root(&trusted, "project-a").unwrap();
    let trusted = trusted.canonicalize().unwrap();
    assert!(prepared.starts_with(&trusted));
    assert_eq!(prepared, prepared.canonicalize().unwrap());

    let outside = temp.path().join("outside-config");
    std::fs::create_dir_all(&outside).unwrap();
    let redirected_trust = temp.path().join("redirected-config");
    create_directory_redirect(&outside, &redirected_trust)
        .expect("create redirected trust boundary");
    assert!(matches!(
        prepare_verified_download_root(&redirected_trust, "project-a"),
        Err(BrowserError::OutsideWorkspace { .. })
    ));
    remove_directory_redirect(&redirected_trust);

    let descendant_trust = temp.path().join("descendant-config");
    std::fs::create_dir_all(descendant_trust.join("browser")).unwrap();
    let redirected_downloads = descendant_trust.join("browser").join("downloads");
    create_directory_redirect(&outside, &redirected_downloads)
        .expect("create redirected download ancestor");
    assert!(matches!(
        prepare_verified_download_root(&descendant_trust, "project-a"),
        Err(BrowserError::OutsideWorkspace { .. })
    ));
    remove_directory_redirect(&redirected_downloads);
}

#[test]
fn verified_profile_root_rejects_an_intermediate_profiles_reparse() {
    let temp = TestDir::new("profile-root-redirect");
    let trusted = temp.path().join("trusted-config");
    let outside = temp.path().join("outside-profiles");
    std::fs::create_dir_all(trusted.join("browser")).unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    let redirected_profiles = trusted.join("browser").join("profiles");
    create_directory_redirect(&outside, &redirected_profiles)
        .expect("create redirected profile ancestor");

    assert!(matches!(
        prepare_verified_profile_root(&trusted, "project-a"),
        Err(BrowserError::OutsideWorkspace { .. })
    ));
    remove_directory_redirect(&redirected_profiles);
}

#[test]
fn profile_clear_rejects_app_config_root_swap_and_preserves_outside_contents() {
    let temp = TestDir::new("profile-root-swap");
    let app_config = temp.path().join("trusted-config");
    let profile = prepare_verified_profile_root(&app_config, "project-a").unwrap();
    let retained_trust_root = app_config.canonicalize().unwrap();
    let parked_config = temp.path().join("parked-config");
    std::fs::rename(&app_config, &parked_config).unwrap();

    let outside = temp.path().join("outside-config");
    let relative_profile = profile.strip_prefix(&retained_trust_root).unwrap();
    let outside_profile = outside.join(relative_profile);
    std::fs::create_dir_all(&outside_profile).unwrap();
    let outside_marker = outside_profile.join("must-survive.txt");
    std::fs::write(&outside_marker, b"outside").unwrap();
    create_directory_redirect(&outside, &app_config).expect("swap app config root for redirect");

    assert!(matches!(
        remove_verified_profile(&retained_trust_root, &profile),
        Err(BrowserError::OutsideWorkspace { .. })
    ));
    assert_eq!(std::fs::read(&outside_marker).unwrap(), b"outside");

    remove_directory_redirect(&app_config);
    std::fs::rename(&parked_config, &app_config).unwrap();
}

#[test]
fn post_initialization_root_swap_blocks_ensure_and_download_preparation_without_writes() {
    let temp = TestDir::new("live-storage-root-swap");
    let app_config = temp.path().join("trusted-config");
    prepare_verified_profile_root(&app_config, "initial-project").unwrap();
    let retained_trust_root = app_config.canonicalize().unwrap();
    let parked_config = temp.path().join("parked-config");
    std::fs::rename(&app_config, &parked_config).unwrap();
    let outside = temp.path().join("outside-config");
    std::fs::create_dir_all(&outside).unwrap();
    create_directory_redirect(&outside, &app_config).expect("swap live storage root");

    assert!(matches!(
        prepare_verified_profile_root(&retained_trust_root, "new-project"),
        Err(BrowserError::OutsideWorkspace { .. })
    ));
    assert!(matches!(
        prepare_verified_download_root(&retained_trust_root, "new-project"),
        Err(BrowserError::OutsideWorkspace { .. })
    ));
    assert_eq!(std::fs::read_dir(&outside).unwrap().count(), 0);

    remove_directory_redirect(&app_config);
    std::fs::rename(&parked_config, &app_config).unwrap();
}

#[test]
fn windows_host_uses_retained_trust_for_every_live_storage_operation() {
    let source = include_str!("../src/browser/host/windows.rs");
    for function in [
        "fn store_resource(",
        "fn handle_download_command(",
        "fn handle_command_inner(",
        "fn ensure_view(",
    ] {
        let start = source.find(function).unwrap();
        let body = &source[start..];
        assert!(body.contains("verified_trusted_app_config_dir()"));
    }
}

#[test]
fn profile_clear_from_raw_app_path_uses_canonical_trust_and_remains_idempotent() {
    let temp = TestDir::new("profile-normal-clear");
    let raw_app_config = temp.path().join("trusted-config");
    let profile = prepare_verified_profile_root(&raw_app_config, "project-a").unwrap();
    let retained_trust_root = raw_app_config.canonicalize().unwrap();
    std::fs::write(profile.join("profile-data"), b"inside").unwrap();

    remove_verified_profile(&retained_trust_root, &profile).unwrap();
    assert!(!profile.exists());
    remove_verified_profile(&retained_trust_root, &profile).unwrap();

    let profiles_root = profile.parent().unwrap();
    std::fs::remove_dir(profiles_root).unwrap();
    remove_verified_profile(&retained_trust_root, &profile).unwrap();
    assert!(matches!(
        remove_verified_profile(
            &retained_trust_root,
            retained_trust_root.join("browser").join("resources"),
        ),
        Err(BrowserError::OutsideWorkspace { .. })
    ));
}

#[test]
fn windows_host_profile_state_and_clear_plan_use_the_retained_canonical_root() {
    let source = include_str!("../src/browser/host/windows.rs");
    let constructor = source.find("fn with_status(").unwrap();
    let clear = source.find("fn clear_project_profile(").unwrap();
    assert!(source[constructor..clear].contains("BrowserHostState::new(state_app_config_dir)"));
    let clear_body = &source[clear..];
    let retained = clear_body.find("self.trusted_app_config_dir").unwrap();
    let layout = clear_body
        .find("BrowserStorageLayout::new(&trusted_app_config_dir")
        .unwrap();
    let remove = clear_body.find("remove_verified_profile(").unwrap();
    assert!(retained < layout && layout < remove);
}

#[test]
fn windows_webview_download_destinations_revalidate_the_trusted_root_before_write() {
    let source = include_str!("../src/browser/host/windows.rs");
    let start = source.find("fn configured_builder").unwrap();
    let end = source[start..].find("fn wry_bounds").unwrap() + start;
    let builder = &source[start..end];

    assert!(builder.contains("trusted_app_config_dir"));
    assert!(builder.contains("verified_unique_download_path("));
    assert!(!builder.contains("match unique_download_path("));
    assert!(source.contains("BrowserDownloadStore::open_verified("));
    assert!(source.contains("prepare_verified_download_root("));
}

#[test]
fn verified_resource_store_rejects_an_intermediate_resources_reparse() {
    let temp = TestDir::new("resource-root-redirect");
    let trusted = temp.path().join("trusted-config");
    let outside = temp.path().join("outside-resources");
    std::fs::create_dir_all(trusted.join("browser")).unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    let redirected_resources = trusted.join("browser").join("resources");
    create_directory_redirect(&outside, &redirected_resources)
        .expect("create redirected resource ancestor");

    assert!(matches!(
        BrowserResourceStore::open_verified(
            &trusted,
            "project-a",
            BrowserResourceLimits::default(),
        ),
        Err(BrowserError::OutsideWorkspace { .. })
    ));
    remove_directory_redirect(&redirected_resources);
}

#[test]
fn verified_resource_store_revalidates_after_open_before_each_write() {
    let temp = TestDir::new("resource-root-swap");
    let trusted = temp.path().join("trusted-config");
    let outside = temp.path().join("outside-resources");
    let store = BrowserResourceStore::open_verified(
        &trusted,
        "project-a",
        BrowserResourceLimits::default(),
    )
    .unwrap();
    let resources = trusted.join("browser").join("resources");
    std::fs::remove_dir_all(&resources).unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    create_directory_redirect(&outside, &resources).expect("swap resource root for redirect");

    assert!(matches!(
        store.put(
            &workspace("project-a", "conversation-a"),
            BrowserResourceKind::ConsoleLog,
            "text/plain",
            b"must stay inside",
            false,
        ),
        Err(BrowserError::OutsideWorkspace { .. })
    ));
    assert_eq!(std::fs::read_dir(&outside).unwrap().count(), 0);
    remove_directory_redirect(&resources);
}

#[test]
fn annotation_resource_pins_reconcile_by_owner_and_survive_reopen() {
    let temp = TestDir::new("annotation-pins");
    let limits = BrowserResourceLimits {
        max_temporary_count: 0,
        max_temporary_bytes: 0,
        max_resource_bytes: 1024,
    };
    let owner_a = workspace("project-a", "conversation-a");
    let owner_b = workspace("project-a", "conversation-b");
    let store = BrowserResourceStore::open(temp.path(), limits).unwrap();
    let kept = store
        .put(
            &owner_a,
            BrowserResourceKind::AnnotationScreenshot,
            "image/png",
            b"kept",
            true,
        )
        .unwrap();
    let released = store
        .put(
            &owner_a,
            BrowserResourceKind::AnnotationScreenshot,
            "image/png",
            b"released",
            true,
        )
        .unwrap();
    let other_owner = store
        .put(
            &owner_b,
            BrowserResourceKind::AnnotationScreenshot,
            "image/png",
            b"other",
            true,
        )
        .unwrap();

    store
        .reconcile_annotation_pins(&owner_a, &BTreeSet::from([kept.id.clone()]))
        .unwrap();
    assert!(store.read(&owner_a, &released.id).is_ok());
    assert!(!store.read(&owner_a, &released.id).unwrap().metadata.pinned);
    assert!(store.read(&owner_a, &kept.id).unwrap().metadata.pinned);
    assert!(
        store
            .read(&owner_b, &other_owner.id)
            .unwrap()
            .metadata
            .pinned
    );

    drop(store);
    let reopened = BrowserResourceStore::open(temp.path(), limits).unwrap();
    assert!(reopened.read(&owner_a, &kept.id).unwrap().metadata.pinned);
    reopened.set_pinned(&owner_a, &released.id, true).unwrap();
    assert!(
        reopened
            .read(&owner_a, &released.id)
            .unwrap()
            .metadata
            .pinned
    );
}

#[test]
fn annotation_host_contract_redacts_lists_creates_owned_details_and_returns_mutation_resources() {
    let temp = TestDir::new("annotation-host-contract");
    let store = BrowserResourceStore::open(temp.path(), BrowserResourceLimits::default())
        .expect("open annotation resource store");
    let key = workspace("project-a", "conversation-a");
    let other = workspace("project-a", "conversation-b");
    let screenshot = store
        .put(
            &key,
            BrowserResourceKind::AnnotationScreenshot,
            "image/png",
            b"screenshot",
            true,
        )
        .expect("store owned screenshot");
    let other_screenshot = store
        .put(
            &other,
            BrowserResourceKind::AnnotationScreenshot,
            "image/png",
            b"other screenshot",
            true,
        )
        .expect("store other screenshot");
    let mut state = BrowserHostState::new(temp.path());
    let snapshot = BrowserWorkspaceSnapshot {
        revision: BrowserRevision(1),
        tabs: vec![BrowserTabSnapshot {
            id: "tab-a".to_string(),
            title: "Fixture".to_string(),
            url: "https://example.test/form".to_string(),
            viewport: BrowserViewport::default(),
        }],
        selected_tab_id: Some("tab-a".to_string()),
        ..BrowserWorkspaceSnapshot::default()
    };
    state
        .ensure_workspace(key.clone(), snapshot.clone())
        .unwrap();
    state.ensure_workspace(other.clone(), snapshot).unwrap();
    state
        .save_annotation(
            &key,
            stored_annotation(
                "ann-owned",
                screenshot.id.clone(),
                "Bearer list-secret should never escape",
            ),
        )
        .unwrap();
    state
        .save_annotation(
            &key,
            stored_annotation(
                "ann-cross-owner",
                other_screenshot.id,
                "Cross-owner fixture",
            ),
        )
        .unwrap();

    let summaries = state
        .annotation_summaries(&key)
        .expect("list compact annotations");
    assert_eq!(summaries.len(), 2);
    assert!(summaries[0].comment.contains("[redacted]"));
    assert!(!summaries[0].comment.contains("list-secret"));
    assert!(summaries[0].screenshot.is_none());
    assert!(summaries[1].screenshot.is_none());

    let details = state
        .annotation_details(&key, "ann-owned", &store)
        .expect("create full annotation details");
    assert_eq!(details.annotation.id, "ann-owned");
    assert_eq!(
        details.details_resource.kind,
        BrowserResourceKind::AnnotationDetails
    );
    assert_eq!(details.screenshot.id, screenshot.id);
    let details_bytes = store
        .read(&key, &details.details_resource.id)
        .expect("read owned details resource")
        .bytes;
    let details_text = String::from_utf8(details_bytes).expect("details are UTF-8 JSON");
    assert!(!details_text.contains("list-secret"));
    assert!(details_text.contains("[redacted]"));
    let response_snapshot = state.workspace(&key).unwrap().clone();
    let response = BrowserResponse::Annotation {
        details: details.clone(),
        mutation: BrowserWorkspaceMutation {
            revision: response_snapshot.revision,
            snapshot: response_snapshot,
        },
    };
    let response_resources = browser_response_resource_ids(&response);
    assert_eq!(
        response_resources,
        vec![
            details.screenshot.id.clone(),
            details.details_resource.id.clone()
        ]
    );
    state
        .append_journal_entry(
            &key,
            BrowserJournalEntry {
                id: "annotation-get".to_string(),
                actor: BrowserJournalActor::Agent,
                intent: "inspect annotation details".to_string(),
                url: "https://example.test/form".to_string(),
                started_at: "2026-07-16T00:00:00Z".to_string(),
                duration_ms: 1,
                result: "ok".to_string(),
                resource_ids: response_resources,
            },
        )
        .unwrap();
    let journal_resources = &state
        .workspace(&key)
        .unwrap()
        .journal_entries
        .last()
        .unwrap()
        .resource_ids;
    assert!(journal_resources.contains(&details.screenshot.id));
    assert!(journal_resources.contains(&details.details_resource.id));
    assert!(matches!(
        state.annotation_details(&other, "ann-owned", &store),
        Err(BrowserError::MissingAnnotation { .. })
    ));
    assert!(matches!(
        state.annotation_details(&key, "ann-cross-owner", &store),
        Err(BrowserError::MissingResource { .. })
    ));
    for operation in [
        BrowserAnnotationOperation::Resolve,
        BrowserAnnotationOperation::Unresolve,
        BrowserAnnotationOperation::Delete,
    ] {
        assert!(matches!(
            state.apply_annotation_operation(&key, operation, "ann-cross-owner", &store,),
            Err(BrowserError::MissingResource { .. })
        ));
    }
    assert!(
        !state
            .workspace(&key)
            .unwrap()
            .annotation("ann-cross-owner")
            .unwrap()
            .resolved
    );

    let resolved = state
        .apply_annotation_operation(
            &key,
            BrowserAnnotationOperation::Resolve,
            "ann-owned",
            &store,
        )
        .expect("resolve annotation");
    assert_eq!(resolved.screenshot.id, screenshot.id);
    assert_eq!(
        browser_response_resource_ids(&BrowserResponse::AnnotationMutation {
            result: resolved.clone(),
        }),
        vec![screenshot.id.clone()]
    );
    assert!(store.handle(&key, &screenshot.id).unwrap().pinned);
    assert!(
        resolved
            .mutation
            .snapshot
            .annotation("ann-owned")
            .unwrap()
            .resolved
    );
    let unresolved = state
        .apply_annotation_operation(
            &key,
            BrowserAnnotationOperation::Unresolve,
            "ann-owned",
            &store,
        )
        .expect("unresolve annotation");
    assert!(
        !unresolved
            .mutation
            .snapshot
            .annotation("ann-owned")
            .unwrap()
            .resolved
    );
    let deleted = state
        .apply_annotation_operation(
            &key,
            BrowserAnnotationOperation::Delete,
            "ann-owned",
            &store,
        )
        .expect("delete annotation");
    assert_eq!(deleted.annotation_id, "ann-owned");
    assert!(deleted.mutation.snapshot.annotation("ann-owned").is_err());
}

#[test]
fn annotation_details_failure_restores_pin_and_forged_same_owner_resources_are_missing() {
    let temp = TestDir::new("annotation-details-failure");
    let store = BrowserResourceStore::open(temp.path(), BrowserResourceLimits::default()).unwrap();
    let key = workspace("project-a", "conversation-a");
    let screenshot = store
        .put(
            &key,
            BrowserResourceKind::AnnotationScreenshot,
            "image/png",
            b"valid screenshot",
            false,
        )
        .unwrap();
    let wrong_kind = store
        .put(
            &key,
            BrowserResourceKind::NetworkBody,
            "image/png",
            b"not an annotation screenshot",
            false,
        )
        .unwrap();
    let wrong_mime = store
        .put(
            &key,
            BrowserResourceKind::AnnotationScreenshot,
            "text/plain",
            b"not a png",
            false,
        )
        .unwrap();
    let snapshot = BrowserWorkspaceSnapshot {
        revision: BrowserRevision(1),
        tabs: vec![BrowserTabSnapshot {
            id: "tab-a".to_string(),
            title: "Fixture".to_string(),
            url: "https://example.test/form".to_string(),
            viewport: BrowserViewport::default(),
        }],
        selected_tab_id: Some("tab-a".to_string()),
        annotations: vec![
            stored_annotation("ann-valid", screenshot.id.clone(), "Valid"),
            stored_annotation("ann-wrong-kind", wrong_kind.id, "Wrong kind"),
            stored_annotation("ann-wrong-mime", wrong_mime.id, "Wrong mime"),
        ],
        ..BrowserWorkspaceSnapshot::default()
    };
    let mut state = BrowserHostState::new(temp.path());
    state.ensure_workspace(key.clone(), snapshot).unwrap();
    for id in ["ann-wrong-kind", "ann-wrong-mime"] {
        assert!(matches!(
            state.annotation_details(&key, id, &store),
            Err(BrowserError::MissingResource { .. })
        ));
        assert!(matches!(
            state.apply_annotation_operation(&key, BrowserAnnotationOperation::Delete, id, &store,),
            Err(BrowserError::MissingResource { .. })
        ));
        assert!(state.workspace(&key).unwrap().annotation(id).is_ok());
    }

    let failing_store = BrowserResourceStore::open(
        temp.path(),
        BrowserResourceLimits {
            max_temporary_count: 128,
            max_temporary_bytes: 64 * 1024 * 1024,
            max_resource_bytes: 1,
        },
    )
    .unwrap();
    assert!(matches!(
        state.annotation_details(&key, "ann-valid", &failing_store),
        Err(BrowserError::ResourceTooLarge { .. })
    ));
    assert!(!store.handle(&key, &screenshot.id).unwrap().pinned);
    assert!(state.workspace(&key).unwrap().journal_entries.is_empty());
}

#[test]
fn annotation_pin_reconciliation_keeps_shared_refs_and_releases_only_after_last_reference() {
    let temp = TestDir::new("annotation-shared-pins");
    let store = BrowserResourceStore::open(temp.path(), BrowserResourceLimits::default()).unwrap();
    let key = workspace("project-a", "conversation-a");
    let screenshot = store
        .put(
            &key,
            BrowserResourceKind::AnnotationScreenshot,
            "image/png",
            b"shared screenshot",
            false,
        )
        .unwrap();
    let snapshot = BrowserWorkspaceSnapshot {
        revision: BrowserRevision(1),
        tabs: vec![BrowserTabSnapshot {
            id: "tab-a".to_string(),
            title: "Fixture".to_string(),
            url: "https://example.test/form".to_string(),
            viewport: BrowserViewport::default(),
        }],
        selected_tab_id: Some("tab-a".to_string()),
        annotations: vec![
            stored_annotation("ann-one", screenshot.id.clone(), "One"),
            stored_annotation("ann-two", screenshot.id.clone(), "Two"),
        ],
        ..BrowserWorkspaceSnapshot::default()
    };
    let mut state = BrowserHostState::new(temp.path());
    state.ensure_workspace(key.clone(), snapshot).unwrap();

    let resolved = state
        .apply_annotation_operation(&key, BrowserAnnotationOperation::Resolve, "ann-one", &store)
        .unwrap();
    store
        .reconcile_annotation_pins(
            &key,
            &resolved.mutation.snapshot.pinned_annotation_resource_ids(),
        )
        .unwrap();
    assert!(store.handle(&key, &screenshot.id).unwrap().pinned);

    let deleted_second = state
        .apply_annotation_operation(&key, BrowserAnnotationOperation::Delete, "ann-two", &store)
        .unwrap();
    store
        .reconcile_annotation_pins(
            &key,
            &deleted_second
                .mutation
                .snapshot
                .pinned_annotation_resource_ids(),
        )
        .unwrap();
    assert!(!store.handle(&key, &screenshot.id).unwrap().pinned);

    let unresolved = state
        .apply_annotation_operation(
            &key,
            BrowserAnnotationOperation::Unresolve,
            "ann-one",
            &store,
        )
        .unwrap();
    assert!(
        !unresolved
            .mutation
            .snapshot
            .annotation("ann-one")
            .unwrap()
            .resolved
    );
    assert!(store.handle(&key, &screenshot.id).unwrap().pinned);

    let deleted_last = state
        .apply_annotation_operation(&key, BrowserAnnotationOperation::Delete, "ann-one", &store)
        .unwrap();
    store
        .reconcile_annotation_pins(
            &key,
            &deleted_last
                .mutation
                .snapshot
                .pinned_annotation_resource_ids(),
        )
        .unwrap();
    assert!(!store.handle(&key, &screenshot.id).unwrap().pinned);
}

#[test]
fn non_annotation_journal_eviction_releases_the_last_annotation_resource_reference() {
    let temp = TestDir::new("annotation-journal-eviction");
    let store = BrowserResourceStore::open(temp.path(), BrowserResourceLimits::default()).unwrap();
    let key = workspace("project-a", "conversation-a");
    let details = store
        .put(
            &key,
            BrowserResourceKind::AnnotationDetails,
            "application/json",
            br#"{"annotation":"fixture"}"#,
            true,
        )
        .unwrap();
    let mut state = BrowserHostState::new(temp.path());
    state
        .ensure_workspace(key.clone(), BrowserWorkspaceSnapshot::default())
        .unwrap();
    let entry = |index: usize, resource_ids: Vec<BrowserResourceId>| BrowserJournalEntry {
        id: format!("journal-{index}"),
        actor: BrowserJournalActor::Agent,
        intent: format!("non-annotation operation {index}"),
        url: "https://example.test".to_string(),
        started_at: "2026-07-16T00:00:00Z".to_string(),
        duration_ms: 1,
        result: "ok".to_string(),
        resource_ids,
    };
    state
        .append_journal_entry(&key, entry(0, vec![details.id.clone()]))
        .unwrap();
    for index in 1..MAX_BROWSER_JOURNAL_ENTRIES {
        state
            .append_journal_entry(&key, entry(index, Vec::new()))
            .unwrap();
    }
    let pinned = state
        .workspace(&key)
        .unwrap()
        .pinned_annotation_resource_ids();
    store.reconcile_annotation_pins(&key, &pinned).unwrap();
    assert!(store.handle(&key, &details.id).unwrap().pinned);

    state
        .append_journal_entry(&key, entry(MAX_BROWSER_JOURNAL_ENTRIES, Vec::new()))
        .unwrap();
    let after_eviction = state
        .workspace(&key)
        .unwrap()
        .pinned_annotation_resource_ids();
    assert!(!after_eviction.contains(&details.id));
    store
        .reconcile_annotation_pins(&key, &after_eviction)
        .unwrap();
    assert!(!store.handle(&key, &details.id).unwrap().pinned);
}

#[test]
fn direct_annotation_resolve_reconciliation_does_not_leave_a_permanent_pin() {
    let temp = TestDir::new("annotation-direct-resolve");
    let store = BrowserResourceStore::open(temp.path(), BrowserResourceLimits::default()).unwrap();
    let key = workspace("project-a", "conversation-a");
    let screenshot = store
        .put(
            &key,
            BrowserResourceKind::AnnotationScreenshot,
            "image/png",
            b"direct resolve screenshot",
            false,
        )
        .unwrap();
    let snapshot = BrowserWorkspaceSnapshot {
        revision: BrowserRevision(1),
        tabs: vec![BrowserTabSnapshot {
            id: "tab-a".to_string(),
            title: "Fixture".to_string(),
            url: "https://example.test/form".to_string(),
            viewport: BrowserViewport::default(),
        }],
        selected_tab_id: Some("tab-a".to_string()),
        annotations: vec![stored_annotation(
            "ann-direct",
            screenshot.id.clone(),
            "Direct",
        )],
        ..BrowserWorkspaceSnapshot::default()
    };
    let mut state = BrowserHostState::new(temp.path());
    state.ensure_workspace(key.clone(), snapshot).unwrap();
    let resolved = state
        .apply_annotation_operation(
            &key,
            BrowserAnnotationOperation::Resolve,
            "ann-direct",
            &store,
        )
        .unwrap();
    assert!(store.handle(&key, &screenshot.id).unwrap().pinned);
    store
        .reconcile_annotation_pins(
            &key,
            &resolved.mutation.snapshot.pinned_annotation_resource_ids(),
        )
        .unwrap();
    assert!(!store.handle(&key, &screenshot.id).unwrap().pinned);
}

#[test]
fn repeated_direct_get_finalization_releases_details_and_resolved_screenshot_pins() {
    let temp = TestDir::new("annotation-direct-get");
    let store = BrowserResourceStore::open(temp.path(), BrowserResourceLimits::default()).unwrap();
    let key = workspace("project-a", "conversation-a");
    let screenshot = store
        .put(
            &key,
            BrowserResourceKind::AnnotationScreenshot,
            "image/png",
            b"direct get screenshot",
            false,
        )
        .unwrap();
    let mut annotation = stored_annotation("ann-direct-get", screenshot.id.clone(), "Direct get");
    annotation.resolved = true;
    let snapshot = BrowserWorkspaceSnapshot {
        revision: BrowserRevision(1),
        tabs: vec![BrowserTabSnapshot {
            id: "tab-a".to_string(),
            title: "Fixture".to_string(),
            url: annotation.url.clone(),
            viewport: BrowserViewport::default(),
        }],
        selected_tab_id: Some("tab-a".to_string()),
        annotations: vec![annotation],
        ..BrowserWorkspaceSnapshot::default()
    };
    let mut state = BrowserHostState::new(temp.path());
    state.ensure_workspace(key.clone(), snapshot).unwrap();

    for _ in 0..3 {
        let details = state
            .annotation_details(&key, "ann-direct-get", &store)
            .unwrap();
        assert!(details.details_resource.pinned);
        assert!(details.screenshot.pinned);
        let pinned = state
            .workspace(&key)
            .unwrap()
            .pinned_annotation_resource_ids();
        store.reconcile_annotation_pins(&key, &pinned).unwrap();
        assert!(
            !store
                .handle(&key, &details.details_resource.id)
                .unwrap()
                .pinned
        );
        assert!(!store.handle(&key, &screenshot.id).unwrap().pinned);
    }

    let details = store
        .list(&key)
        .unwrap()
        .into_iter()
        .filter(|resource| resource.kind == BrowserResourceKind::AnnotationDetails)
        .collect::<Vec<_>>();
    assert_eq!(details.len(), 3);
    assert!(details.iter().all(|resource| !resource.pinned));
}

#[test]
fn redacted_secret_query_url_does_not_make_a_fresh_annotation_stale() {
    let mut candidate = annotation_candidate();
    candidate.url = "https://example.test/form?token=super-secret&view=review".to_string();
    candidate.revision = BrowserRevision(9);
    let annotation = BrowserAnnotationDraft::new(
        "tab-a",
        candidate.clone(),
        BrowserResourceId(format!("res-{}", "f".repeat(32))),
    )
    .unwrap()
    .into_annotation("Review the save button")
    .unwrap();
    assert_ne!(
        annotation.url, candidate.url,
        "secret URL is persisted redacted"
    );
    let annotation_id = annotation.id.clone();
    let mut snapshot = BrowserWorkspaceSnapshot {
        revision: candidate.revision,
        tabs: vec![BrowserTabSnapshot {
            id: "tab-a".to_string(),
            title: "Fixture".to_string(),
            url: candidate.url,
            viewport: candidate.viewport,
        }],
        selected_tab_id: Some("tab-a".to_string()),
        annotations: vec![annotation],
        ..BrowserWorkspaceSnapshot::default()
    };

    assert!(!snapshot.annotation_anchor_is_stale(&annotation_id).unwrap());
    snapshot.tabs[0].url = "https://example.test/another-page".to_string();
    assert!(snapshot.annotation_anchor_is_stale(&annotation_id).unwrap());
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
    assert_eq!(updated.revision.0, navigated.revision.0 + 1);

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

#[test]
fn title_and_viewport_revision_causes_cancel_annotation_before_state_mutation() {
    let source = include_str!("../src/browser/host/windows.rs");

    let drain = &source[source.find("pub fn drain_events(").unwrap()..];
    let title_start = drain.find("BrowserHostEvent::TitleChanged").unwrap();
    let title_end = drain[title_start..]
        .find("BrowserHostEvent::PageLoad")
        .unwrap()
        + title_start;
    let title = &drain[title_start..title_end];
    assert!(
        title.find("cancel_annotation_mode").unwrap() < title.find("apply_title_change").unwrap()
    );

    let commands = &source[source.find("fn handle_available_command(").unwrap()..];
    let viewport_start = commands.find("BrowserCommand::UpdateViewport").unwrap();
    let viewport_end = commands[viewport_start..]
        .find("BrowserCommand::OpenDevTools")
        .unwrap()
        + viewport_start;
    let viewport = &commands[viewport_start..viewport_end];
    assert!(viewport.contains("tab.viewport != viewport"));
    assert!(
        viewport.find("cancel_annotation_mode").unwrap()
            < viewport.find("update_viewport").unwrap()
    );
}

#[test]
fn host_journal_and_pane_metadata_do_not_stale_page_element_references() {
    let temp = TestDir::new("host-journal");
    let mut host = BrowserHostState::new(temp.path());
    let key = workspace("project-a", "conversation-a");
    let initial = host
        .ensure_workspace(key.clone(), BrowserWorkspaceSnapshot::default())
        .unwrap();
    let captured_ref = BrowserElementRef {
        revision: initial.revision,
        locator: BrowserLocator {
            test_id: Some("captured-before-journal".to_string()),
            ..BrowserLocator::default()
        },
        backend_node_id: Some(42),
    };
    let mutation = host
        .append_journal_entry(
            &key,
            BrowserJournalEntry {
                id: "op-1".to_string(),
                actor: BrowserJournalActor::Agent,
                intent: "inspect page".to_string(),
                url: "https://fixture.test".to_string(),
                started_at: "2026-07-16T00:00:00Z".to_string(),
                duration_ms: 4,
                result: "ok".to_string(),
                resource_ids: Vec::new(),
            },
        )
        .unwrap();

    assert_eq!(mutation.revision, initial.revision);
    mutation
        .snapshot
        .validate_element_ref(&captured_ref)
        .expect("journal metadata must not stale a page element reference");
    assert_eq!(mutation.snapshot.journal_entries.len(), 1);
    assert_eq!(mutation.snapshot.journal_entries[0].id, "op-1");

    let closed = host.set_pane_open(&key, false).unwrap();
    assert_eq!(closed.revision, initial.revision);
    closed
        .snapshot
        .validate_element_ref(&captured_ref)
        .expect("pane metadata must not stale a page element reference");
    let reopened = host.set_pane_open(&key, true).unwrap();
    assert_eq!(reopened.revision, initial.revision);
    reopened
        .snapshot
        .validate_element_ref(&captured_ref)
        .expect("reopening pane must not stale a page element reference");
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
