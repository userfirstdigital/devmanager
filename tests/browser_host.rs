use devmanager::browser::{
    browser_command_channel, browser_lifecycle_control, browser_request_preempts_operation_queue,
    browser_user_input_initialization_script, prepare_verified_download_root,
    prepare_verified_profile_root, remove_verified_profile, route_browser_request,
    unique_download_path, unsupported_host_status, unsupported_platform_error,
    validate_browser_url, BrowserAction, BrowserActionTarget, BrowserCommand, BrowserCommandBridge,
    BrowserCommandRequest, BrowserConsoleOperation, BrowserDiagnosticLevel,
    BrowserDownloadOperation, BrowserDownloadState, BrowserElementRef, BrowserError,
    BrowserHostControl, BrowserHostEvent, BrowserHostState, BrowserHostStatus,
    BrowserInvocationActor, BrowserInvocationContext, BrowserJournalActor, BrowserJournalEntry,
    BrowserLocator, BrowserMemoryTarget, BrowserNetworkOperation, BrowserOperationQueue,
    BrowserOperationTarget, BrowserPageLoadState, BrowserPerformanceOperation, BrowserResourceKind,
    BrowserResourceLimits, BrowserResourceStore, BrowserResponse, BrowserRevision, BrowserRisk,
    BrowserScreenshotMode, BrowserStorageLayout, BrowserTabSnapshot, BrowserUserInputKind,
    BrowserViewport, BrowserWaitCondition, BrowserWaitResult, BrowserWebViewHost,
    BrowserWorkspaceKey, BrowserWorkspaceSnapshot,
};
use static_assertions::{assert_impl_all, assert_not_impl_any};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
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

    assert!(windows_host.contains("BrowserInputMessage::DomMutation"));
    assert!(windows_host.contains("BrowserHostEvent::DomMutation"));
    assert!(serde_json::from_str::<BrowserUserInputKind>("\"pointer\"").is_ok());
    assert!(serde_json::from_str::<BrowserUserInputKind>("\"keyboard\"").is_ok());
    assert!(serde_json::from_str::<BrowserUserInputKind>("\"textInput\"").is_ok());
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
