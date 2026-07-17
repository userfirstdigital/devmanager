use base64::Engine as _;
use devmanager::browser::{
    browser_command_channel, BrowserActionResult, BrowserAnnotation, BrowserCommand,
    BrowserCommandInbox, BrowserConsoleEntry, BrowserDownloadEntry, BrowserGatewayHandle,
    BrowserHostControl, BrowserHostState, BrowserHostStatus, BrowserInvocationActor,
    BrowserInvocationContext, BrowserNetworkEntry, BrowserPerformanceSnapshot,
    BrowserRecipeInputKind, BrowserRecordingInputSummary, BrowserRecordingOperation,
    BrowserRecordingResult, BrowserRecordingStatus, BrowserResourceHandle, BrowserResourceId,
    BrowserResourceKind, BrowserResourceLimits, BrowserResourceStore, BrowserResponse,
    BrowserRevision, BrowserRisk, BrowserSnapshotSummary, BrowserStorageLayout, BrowserTabSnapshot,
    BrowserUploadResult, BrowserViewport, BrowserWaitResult, BrowserWorkspaceKey,
    BrowserWorkspaceSnapshot,
};
use rmcp::model::{CallToolRequestParams, ClientInfo, ReadResourceRequestParams, ResourceContents};
use rmcp::transport::{
    streamable_http_client::StreamableHttpClientTransportConfig, StreamableHttpClientTransport,
};
use rmcp::ServiceExt as _;
use serde_json::{json, Map, Value};
use std::collections::VecDeque;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

fn unique_gateway_config_dir(label: &str) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(1);
    std::env::temp_dir().join(format!(
        "devmanager-browser-gateway-{label}-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ))
}

#[cfg(target_os = "windows")]
fn create_directory_redirect(target: &std::path::Path, link: &std::path::Path) {
    let status = std::process::Command::new("cmd.exe")
        .args(["/c", "mklink", "/J"])
        .arg(link)
        .arg(target)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap();
    assert!(status.success(), "create directory junction");
}

#[cfg(not(target_os = "windows"))]
fn create_directory_redirect(target: &std::path::Path, link: &std::path::Path) {
    std::os::unix::fs::symlink(target, link).unwrap();
}

#[cfg(target_os = "windows")]
fn remove_directory_redirect(link: &std::path::Path) {
    std::fs::remove_dir(link).unwrap();
}

#[cfg(not(target_os = "windows"))]
fn remove_directory_redirect(link: &std::path::Path) {
    std::fs::remove_file(link).unwrap();
}

fn workspace(project: &str, conversation: &str) -> BrowserWorkspaceKey {
    BrowserWorkspaceKey::new(project, conversation).expect("valid browser workspace key")
}

fn initialize_body() -> &'static str {
    r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"devmanager-test","version":"1"}}}"#
}

fn raw_mcp_request(
    port: u16,
    host: &str,
    authorization: Option<&str>,
    path: &str,
    body: &str,
) -> String {
    raw_mcp_request_with_headers(port, host, authorization, path, body, "")
}

fn raw_mcp_request_with_headers(
    port: u16,
    host: &str,
    authorization: Option<&str>,
    path: &str,
    body: &str,
    extra_headers: &str,
) -> String {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect gateway");
    stream
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    let auth = authorization
        .map(|value| format!("Authorization: {value}\r\n"))
        .unwrap_or_default();
    let request = format!(
        "POST {path} HTTP/1.1\r\nHost: {host}\r\n{auth}{extra_headers}Content-Type: application/json\r\nAccept: application/json, text/event-stream\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(request.as_bytes()).expect("write request");
    let mut response = String::new();
    let _ = stream.read_to_string(&mut response);
    response
}

fn response_header<'a>(response: &'a str, name: &str) -> Option<&'a str> {
    response.lines().find_map(|line| {
        let (header, value) = line.split_once(':')?;
        header.eq_ignore_ascii_case(name).then(|| value.trim())
    })
}

fn status_code(response: &str) -> u16 {
    response
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|value| value.parse().ok())
        .unwrap_or_default()
}

fn arguments(value: Value) -> Map<String, Value> {
    serde_json::from_value(value).expect("tool arguments object")
}

fn fixture_resource(
    id: &str,
    kind: BrowserResourceKind,
    mime_type: &str,
    byte_size: u64,
) -> BrowserResourceHandle {
    BrowserResourceHandle {
        id: BrowserResourceId(id.to_string()),
        uri: format!("devmanager-browser://resource/{id}"),
        mime_type: mime_type.to_string(),
        kind,
        byte_size,
        created_at_epoch_ms: 1,
        pinned: false,
    }
}

fn fixture_annotation(
    id: &str,
    screenshot_resource: &BrowserResourceId,
    comment: &str,
) -> BrowserAnnotation {
    serde_json::from_value(json!({
        "id": id,
        "kind": "element",
        "tabId": "tab-a",
        "anchorRevision": 1,
        "comment": comment,
        "url": "https://example.test/form",
        "locator": {
            "accessibilityRole": "button",
            "accessibilityName": "Save",
            "testId": "save",
            "cssSelectors": ["[data-testid=save]"]
        },
        "bounds": { "x": 10, "y": 20, "width": 120, "height": 32 },
        "viewport": { "width": 1280, "height": 720, "scalePercent": 100 },
        "screenshotResource": screenshot_resource,
        "computedStyles": { "display": "block" },
        "resolved": false
    }))
    .expect("valid annotation fixture")
}

async fn run_fake_host(
    inbox: BrowserCommandInbox,
    commands: Arc<Mutex<Vec<(BrowserWorkspaceKey, BrowserCommand)>>>,
) {
    run_fake_host_with_state(
        inbox,
        commands,
        Arc::new(Mutex::new(BrowserHostState::new(PathBuf::from(
            "gateway-fake-host",
        )))),
        None,
    )
    .await;
}

async fn run_fake_host_with_state(
    mut inbox: BrowserCommandInbox,
    commands: Arc<Mutex<Vec<(BrowserWorkspaceKey, BrowserCommand)>>>,
    host: Arc<Mutex<BrowserHostState>>,
    annotation_resources: Option<BrowserResourceStore>,
) {
    let mut priority_requests = VecDeque::new();
    let mut ordinary_request = None;
    let mut inbox_closed = false;
    loop {
        let queued = inbox.with_locked_host_work(|_controls, requests| requests);
        priority_requests.extend(queued);
        let request = if let Some(request) = priority_requests.pop_front() {
            request
        } else if let Some(request) = ordinary_request.take() {
            request
        } else if inbox_closed {
            break;
        } else {
            match tokio::time::timeout(Duration::from_millis(5), inbox.recv()).await {
                Ok(Some(request)) => ordinary_request = Some(request),
                Ok(None) => inbox_closed = true,
                Err(_) => {}
            }
            continue;
        };
        let key = request.workspace_key().clone();
        let command = request.command().clone();
        commands
            .lock()
            .unwrap()
            .push((key.clone(), command.clone()));
        let mut host = host.lock().unwrap();
        let result = match command {
            BrowserCommand::Status => Ok(BrowserResponse::Status {
                status: BrowserHostStatus {
                    available: true,
                    platform: "windows".to_string(),
                    version: Some("fixture-webview2".to_string()),
                    diagnostic: None,
                },
            }),
            BrowserCommand::WorkspaceState => host
                .workspace(&key)
                .cloned()
                .map(|snapshot| BrowserResponse::WorkspaceState { snapshot })
                .ok_or_else(|| devmanager::browser::BrowserError::CrashedView {
                    message: "missing fake workspace".to_string(),
                }),
            BrowserCommand::Ensure { snapshot } => host
                .ensure_workspace(key, snapshot)
                .map(|mutation| BrowserResponse::Workspace { mutation }),
            BrowserCommand::SetPaneOpen { open } => host
                .set_pane_open(&key, open)
                .map(|mutation| BrowserResponse::Workspace { mutation }),
            BrowserCommand::Annotations {
                operation,
                annotation_id,
            } => {
                let resources = annotation_resources
                    .as_ref()
                    .expect("annotation fake host requires a resource store");
                match operation {
                    devmanager::browser::BrowserAnnotationOperation::List => {
                        host.annotation_summaries(&key).map(|annotations| {
                            let snapshot = host.workspace(&key).unwrap().clone();
                            BrowserResponse::Annotations {
                                annotations,
                                mutation: devmanager::browser::BrowserWorkspaceMutation {
                                    revision: snapshot.revision,
                                    snapshot,
                                },
                            }
                        })
                    }
                    devmanager::browser::BrowserAnnotationOperation::Get => host
                        .annotation_details(
                            &key,
                            annotation_id.as_deref().unwrap_or_default(),
                            resources,
                        )
                        .map(|details| {
                            let snapshot = host.workspace(&key).unwrap().clone();
                            BrowserResponse::Annotation {
                                details,
                                mutation: devmanager::browser::BrowserWorkspaceMutation {
                                    revision: snapshot.revision,
                                    snapshot,
                                },
                            }
                        }),
                    devmanager::browser::BrowserAnnotationOperation::Resolve
                    | devmanager::browser::BrowserAnnotationOperation::Unresolve
                    | devmanager::browser::BrowserAnnotationOperation::Delete => host
                        .apply_annotation_operation(
                            &key,
                            operation,
                            annotation_id.as_deref().unwrap_or_default(),
                            resources,
                        )
                        .map(|result| BrowserResponse::AnnotationMutation { result }),
                }
            }
            BrowserCommand::ListTabs => host
                .workspace(&key)
                .cloned()
                .map(|snapshot| BrowserResponse::Tabs {
                    tabs: snapshot.tabs,
                    selected_tab_id: snapshot.selected_tab_id,
                })
                .ok_or_else(|| devmanager::browser::BrowserError::CrashedView {
                    message: "missing fake workspace".to_string(),
                }),
            BrowserCommand::CreateTab { url } => host
                .create_tab(&key, url.unwrap_or_else(|| "about:blank".to_string()))
                .map(|mutation| BrowserResponse::Workspace { mutation }),
            BrowserCommand::SelectTab { tab_id } => host
                .select_tab(&key, &tab_id)
                .map(|mutation| BrowserResponse::Workspace { mutation }),
            BrowserCommand::CloseTab { tab_id } => host
                .close_tab(&key, &tab_id)
                .map(|mutation| BrowserResponse::Workspace { mutation }),
            BrowserCommand::Navigate { tab_id, url } => host
                .navigate_tab(&key, &tab_id, &url)
                .map(|mutation| BrowserResponse::Workspace { mutation }),
            BrowserCommand::Back { .. }
            | BrowserCommand::Forward { .. }
            | BrowserCommand::Reload { .. }
            | BrowserCommand::Stop { .. } => Ok(BrowserResponse::Acknowledged),
            BrowserCommand::Snapshot { tab_id } => Ok(BrowserResponse::Snapshot {
                summary: BrowserSnapshotSummary {
                    tab_id,
                    url: "http://127.0.0.1:4173/".to_string(),
                    revision: BrowserRevision(7),
                    element_count: 12,
                },
                resource: fixture_resource(
                    "res-00000000000000000000000000000001",
                    BrowserResourceKind::DomSnapshot,
                    "application/json",
                    128,
                ),
            }),
            BrowserCommand::Screenshot { .. } => Ok(BrowserResponse::Screenshot {
                resource: fixture_resource(
                    "res-00000000000000000000000000000002",
                    BrowserResourceKind::Screenshot,
                    "image/png",
                    256,
                ),
            }),
            BrowserCommand::Wait { timeout_ms, .. } if timeout_ms == 13 => {
                Err(devmanager::browser::BrowserError::Timeout {
                    operation: "fixture wait".to_string(),
                })
            }
            BrowserCommand::Wait { .. } => Ok(BrowserResponse::Wait {
                result: BrowserWaitResult {
                    matched: true,
                    elapsed_ms: 1,
                    revision: BrowserRevision(7),
                },
            }),
            BrowserCommand::Act { actions, .. } => Ok(BrowserResponse::Action {
                result: BrowserActionResult {
                    completed_actions: actions.len(),
                    revision: BrowserRevision(8),
                },
            }),
            BrowserCommand::Console { .. } => Ok(BrowserResponse::Console {
                entries: vec![BrowserConsoleEntry {
                    sequence: 1,
                    level: "error".to_string(),
                    message: "fixture runtime error".to_string(),
                    timestamp_ms: 1,
                }],
                resource: None,
            }),
            BrowserCommand::Network { .. } => Ok(BrowserResponse::Network {
                entries: vec![BrowserNetworkEntry {
                    request_id: "fixture-request".to_string(),
                    url: "http://127.0.0.1:4173/api/success".to_string(),
                    method: "GET".to_string(),
                    status: Some(200),
                    failed: false,
                    body_available: true,
                    duration_ms: Some(2),
                }],
                resource: None,
                body_available: Some(true),
            }),
            BrowserCommand::Performance { .. } => Ok(BrowserResponse::Performance {
                snapshot: Some(BrowserPerformanceSnapshot {
                    navigation: json!({"type":"navigate","duration":2}),
                    entries: vec![json!({"name":"fixture","duration":1})],
                }),
                resource: None,
                tracing: false,
            }),
            BrowserCommand::Upload { paths, .. } => Ok(BrowserResponse::Upload {
                result: BrowserUploadResult {
                    files: paths,
                    revision: BrowserRevision(9),
                },
            }),
            BrowserCommand::Downloads { .. } => Ok(BrowserResponse::Downloads {
                downloads: vec![BrowserDownloadEntry {
                    id: "download-fixture".to_string(),
                    file_name: "fixture-download.txt".to_string(),
                    byte_size: 16,
                    completed: true,
                }],
            }),
            BrowserCommand::Cdp { method, .. } if method == "Runtime.fail" => {
                Err(devmanager::browser::BrowserError::CrashedView {
                    message: "fixture CDP failure".to_string(),
                })
            }
            BrowserCommand::Cdp { .. } => Ok(BrowserResponse::Cdp {
                inline_result: Some(json!({"result":{"value":4}})),
                resource: None,
            }),
            other => panic!("unexpected fake-host command: {other:?}"),
        };
        drop(host);
        request.respond(result);
    }
}

async fn run_recording_bridge_host(
    mut inbox: BrowserCommandInbox,
    observed: Arc<
        Mutex<
            Vec<(
                BrowserWorkspaceKey,
                BrowserCommand,
                BrowserInvocationContext,
                Option<PathBuf>,
            )>,
        >,
    >,
    scripted: Arc<
        Mutex<VecDeque<Result<BrowserRecordingResult, devmanager::browser::BrowserError>>>,
    >,
) {
    let mut host = BrowserHostState::new(PathBuf::from("recording-bridge-fake-host"));
    while let Some(request) = inbox.recv().await {
        let workspace_key = request.workspace_key().clone();
        let command = request.command().clone();
        observed.lock().unwrap().push((
            workspace_key.clone(),
            command.clone(),
            request.context().clone(),
            request.local_project_root().map(Path::to_path_buf),
        ));
        let result = match command {
            BrowserCommand::Ensure { snapshot } => host
                .ensure_workspace(workspace_key, snapshot)
                .map(|mutation| BrowserResponse::Workspace { mutation }),
            BrowserCommand::SetPaneOpen { open } => host
                .set_pane_open(&workspace_key, open)
                .map(|mutation| BrowserResponse::Workspace { mutation }),
            BrowserCommand::WorkspaceState => host
                .workspace(&workspace_key)
                .cloned()
                .map(|snapshot| BrowserResponse::WorkspaceState { snapshot })
                .ok_or_else(|| devmanager::browser::BrowserError::CrashedView {
                    message: "missing recording fake workspace".to_string(),
                }),
            BrowserCommand::Recording { operation } => {
                let response = scripted
                    .lock()
                    .unwrap()
                    .pop_front()
                    .expect("scripted recording response");
                response.and_then(|response| {
                    assert_eq!(response.operation, operation);
                    Ok(BrowserResponse::Recording { result: response })
                })
            }
            other => panic!("unexpected recording bridge command: {other:?}"),
        };
        request.respond(result);
    }
}

#[test]
fn token_is_256_bits_rotates_on_replacement_and_stale_auth_is_rejected() {
    let (bridge, _inbox) = browser_command_channel(8);
    let gateway = BrowserGatewayHandle::start(bridge.clone()).expect("start gateway");
    let registrar = gateway.registrar();
    let old_workspace = workspace("project-a", "conversation-a");
    let first = registrar
        .register(
            "ai-process-a",
            old_workspace.clone(),
            BrowserWorkspaceSnapshot::default(),
        )
        .expect("register first token");
    let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(first.access().bearer_token_for_launch())
        .expect("base64url token");
    assert_eq!(decoded.len(), 32);
    assert_eq!(first.access().endpoint(), gateway.endpoint());

    let replacement = registrar
        .register(
            "ai-process-a",
            workspace("project-b", "conversation-b"),
            BrowserWorkspaceSnapshot::default(),
        )
        .expect("register replacement token");
    assert_ne!(
        first.access().bearer_token_for_launch(),
        replacement.access().bearer_token_for_launch()
    );
    assert_eq!(
        bridge.drain_host_controls(),
        vec![BrowserHostControl::InterruptWorkspace {
            workspace_key: old_workspace,
        }]
    );

    let host = format!("127.0.0.1:{}", gateway.port());
    let stale = raw_mcp_request(
        gateway.port(),
        &host,
        Some(&format!(
            "Bearer {}",
            first.access().bearer_token_for_launch()
        )),
        "/mcp",
        initialize_body(),
    );
    assert_eq!(status_code(&stale), 401, "{stale}");

    let current = raw_mcp_request(
        gateway.port(),
        &host,
        Some(&format!(
            "Bearer {}",
            replacement.access().bearer_token_for_launch()
        )),
        "/mcp",
        initialize_body(),
    );
    assert_eq!(status_code(&current), 200, "{current}");
    assert_eq!(registrar.active_registration_count(), 1);
    assert!(!format!("{registrar:?}").contains(replacement.access().bearer_token_for_launch()));
}

#[test]
fn authenticated_dispatch_snapshots_and_rechecks_the_registration_lease() {
    let source = include_str!("../src/browser/gateway.rs");
    let dispatch_start = source.find("async fn dispatch_mcp(").unwrap();
    let snapshot_start = source.find("fn registration_dispatch_snapshot(").unwrap();
    let guarded_start = source.find("async fn dispatch_registration(").unwrap();
    let end = source[guarded_start..].find("fn validate_host(").unwrap() + guarded_start;
    let dispatch = &source[dispatch_start..snapshot_start];
    let snapshot = &source[snapshot_start..guarded_start];
    let guarded = &source[guarded_start..end];
    let capture = snapshot.find("registration.lease.capture()").unwrap();
    let clone = snapshot.find("registration.service.clone()").unwrap();
    let first_current = guarded.find("lease.is_current(ticket)").unwrap();
    let call = guarded.find("service.call(request).await").unwrap();
    let second_current = guarded[call..]
        .find("lease.is_current(ticket)")
        .map(|offset| call + offset)
        .unwrap();

    assert!(dispatch.contains("registration_dispatch_snapshot(&inner, token)"));
    assert!(dispatch.contains("dispatch_registration(snapshot, request).await"));
    assert!(capture < clone);
    assert!(first_current < call);
    assert!(call < second_current);
}

#[test]
fn gateway_revocation_publishes_priority_host_cancellation() {
    let (bridge, _inbox) = browser_command_channel(8);
    let gateway = BrowserGatewayHandle::start(bridge.clone()).expect("start gateway");
    let key = workspace("project-revoke", "conversation-revoke");
    let registration = gateway
        .registrar()
        .register(
            "revoked-process",
            key.clone(),
            BrowserWorkspaceSnapshot::default(),
        )
        .expect("register revocation fixture");

    assert!(gateway.registrar().revoke(&registration));
    assert_eq!(
        bridge.drain_host_controls(),
        vec![BrowserHostControl::InterruptWorkspace { workspace_key: key }]
    );
}

#[test]
fn gateway_registration_rejects_post_start_trust_root_swap_without_outside_writes() {
    let config = unique_gateway_config_dir("root-swap");
    let (bridge, _inbox) = browser_command_channel(8);
    let gateway = BrowserGatewayHandle::start_with_app_config_dir(bridge, &config)
        .expect("start gateway with retained trust root");
    let parked = config.with_extension("parked");
    std::fs::rename(&config, &parked).unwrap();
    let outside = config.with_extension("outside");
    std::fs::create_dir_all(&outside).unwrap();
    create_directory_redirect(&outside, &config);

    let error = gateway
        .registrar()
        .register(
            "swapped-process",
            workspace("project-swap", "conversation-swap"),
            BrowserWorkspaceSnapshot::default(),
        )
        .expect_err("swapped trust root must be rejected");
    assert!(error.contains("storage root") || error.contains("OutsideWorkspace"));
    assert!(!outside.join("browser").exists());

    remove_directory_redirect(&config);
    std::fs::rename(&parked, &config).unwrap();
    drop(gateway);
    let _ = std::fs::remove_dir_all(&config);
    let _ = std::fs::remove_dir_all(&outside);
}

#[test]
fn auth_and_host_are_rejected_before_rmcp_dispatch() {
    let (bridge, _inbox) = browser_command_channel(8);
    let gateway = BrowserGatewayHandle::start(bridge).expect("start gateway");
    let registration = gateway
        .registrar()
        .register(
            "ai-process-a",
            workspace("project-a", "conversation-a"),
            BrowserWorkspaceSnapshot::default(),
        )
        .expect("register token");
    let port = gateway.port();
    let allowed_host = format!("localhost:{port}");
    let bearer = format!("Bearer {}", registration.access().bearer_token_for_launch());

    for (host, auth, expected) in [
        (allowed_host.as_str(), None, 401),
        (allowed_host.as_str(), Some("Basic abc"), 401),
        (allowed_host.as_str(), Some("bearer abc"), 401),
        ("example.com", Some(bearer.as_str()), 403),
        ("127.0.0.1:1", Some(bearer.as_str()), 403),
    ] {
        let response = raw_mcp_request(port, host, auth, "/mcp", initialize_body());
        assert_eq!(status_code(&response), expected, "{host}: {response}");
    }
    let wrong_path = raw_mcp_request(
        port,
        &allowed_host,
        Some(&bearer),
        "/not-mcp",
        initialize_body(),
    );
    assert_eq!(status_code(&wrong_path), 404, "{wrong_path}");
}

#[test]
fn fabricated_session_ids_cannot_cross_token_bound_workspace_services() {
    let (bridge, _inbox) = browser_command_channel(8);
    let gateway = BrowserGatewayHandle::start(bridge).expect("start gateway");
    let registrar = gateway.registrar();
    let first = registrar
        .register(
            "ai-process-a",
            workspace("shared-project", "conversation-a"),
            BrowserWorkspaceSnapshot::default(),
        )
        .unwrap();
    let second = registrar
        .register(
            "ai-process-b",
            workspace("shared-project", "conversation-b"),
            BrowserWorkspaceSnapshot::default(),
        )
        .unwrap();
    assert_eq!(first.access().endpoint(), second.access().endpoint());
    assert_eq!(registrar.active_registration_count(), 2);
    let host = format!("127.0.0.1:{}", gateway.port());
    let initialized = raw_mcp_request(
        gateway.port(),
        &host,
        Some(&format!(
            "Bearer {}",
            first.access().bearer_token_for_launch()
        )),
        "/mcp",
        initialize_body(),
    );
    assert_eq!(status_code(&initialized), 200, "{initialized}");
    let session_id =
        response_header(&initialized, "mcp-session-id").expect("rmcp session id from first token");
    let fabricated = raw_mcp_request_with_headers(
        gateway.port(),
        &host,
        Some(&format!(
            "Bearer {}",
            second.access().bearer_token_for_launch()
        )),
        "/mcp",
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}"#,
        &format!("Mcp-Session-Id: {session_id}\r\nMCP-Protocol-Version: 2025-03-26\r\n"),
    );
    assert_eq!(status_code(&fabricated), 404, "{fabricated}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tokens_on_one_listener_route_to_their_exact_bound_workspaces() {
    let (bridge, inbox) = browser_command_channel(32);
    let commands: Arc<Mutex<Vec<(BrowserWorkspaceKey, BrowserCommand)>>> =
        Arc::new(Mutex::new(Vec::new()));
    let gateway = BrowserGatewayHandle::start(bridge).expect("start gateway");
    let observed_commands = Arc::clone(&commands);
    let scenario = async move {
        let expected = [
            workspace("shared-project", "conversation-a"),
            workspace("shared-project", "conversation-b"),
            workspace("separate-project", "conversation-c"),
        ];
        let registrations = expected
            .iter()
            .enumerate()
            .map(|(index, key)| {
                gateway
                    .registrar()
                    .register(
                        format!("ai-process-{index}"),
                        key.clone(),
                        BrowserWorkspaceSnapshot::default(),
                    )
                    .expect("register workspace token")
            })
            .collect::<Vec<_>>();
        assert_eq!(gateway.registrar().active_registration_count(), 3);

        for (registration, key) in registrations.iter().zip(&expected) {
            let transport = StreamableHttpClientTransport::from_config(
                StreamableHttpClientTransportConfig::with_uri(gateway.endpoint().to_string())
                    .auth_header(registration.access().bearer_token_for_launch()),
            );
            let client = ClientInfo::default()
                .serve(transport)
                .await
                .expect("initialize bound client");
            let status = client
                .peer()
                .call_tool(
                    CallToolRequestParams::new("browser_status").with_arguments(arguments(json!({
                        "intent": "identify my visible browser pane",
                        "risk": "normal"
                    }))),
                )
                .await
                .expect("call bound status")
                .structured_content
                .expect("structured status");
            assert_eq!(status["workspace"]["projectId"], key.project_id);
            assert_eq!(status["workspace"]["aiTabId"], key.ai_tab_id);
            client.cancel().await.expect("close bound client");
        }

        let routed = observed_commands.lock().unwrap().clone();
        for (index, key) in expected.iter().enumerate() {
            let commands = &routed[index * 4..index * 4 + 4];
            assert!(commands.iter().all(|(routed_key, _)| routed_key == key));
            assert!(matches!(commands[0].1, BrowserCommand::Ensure { .. }));
            assert_eq!(commands[1].1, BrowserCommand::SetPaneOpen { open: true });
            assert_eq!(commands[2].1, BrowserCommand::WorkspaceState);
            assert_eq!(commands[3].1, BrowserCommand::Status);
        }
    };
    let (_, ()) = tokio::join!(run_fake_host(inbox, commands), scenario);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn real_rmcp_resources_are_standard_and_token_owner_isolated() {
    let config_dir = unique_gateway_config_dir("resources");
    let layout = BrowserStorageLayout::new(&config_dir, "project-a");
    let store = BrowserResourceStore::open(&layout.resources_dir, BrowserResourceLimits::default())
        .expect("open project resource store");
    let owner_a = workspace("project-a", "conversation-a");
    let owner_b = workspace("project-a", "conversation-b");
    let resource_a = store
        .put(
            &owner_a,
            BrowserResourceKind::DomSnapshot,
            "application/json",
            br#"{"owner":"a"}"#,
            false,
        )
        .expect("store owner-a resource");
    let resource_b = store
        .put(
            &owner_b,
            BrowserResourceKind::NetworkBody,
            "text/plain",
            b"owner-b-only",
            false,
        )
        .expect("store owner-b resource");
    let (bridge, _inbox) = browser_command_channel(8);
    let gateway = BrowserGatewayHandle::start_with_app_config_dir(bridge, &config_dir)
        .expect("start resource-aware gateway");
    let registration = gateway
        .registrar()
        .register(
            "resource-client-a",
            owner_a,
            BrowserWorkspaceSnapshot::default(),
        )
        .expect("register owner-a token");
    let transport = StreamableHttpClientTransport::from_config(
        StreamableHttpClientTransportConfig::with_uri(gateway.endpoint().to_string())
            .auth_header(registration.access().bearer_token_for_launch()),
    );
    let client = ClientInfo::default()
        .serve(transport)
        .await
        .expect("initialize owner-a resource client");

    let listed = client
        .peer()
        .list_resources(None)
        .await
        .expect("list resources");
    assert_eq!(listed.resources.len(), 1);
    assert_eq!(listed.resources[0].uri, resource_a.uri);
    assert_eq!(
        listed.resources[0].mime_type.as_deref(),
        Some("application/json")
    );
    assert_eq!(listed.resources[0].size, Some(resource_a.byte_size));

    let read = client
        .peer()
        .read_resource(ReadResourceRequestParams::new(resource_a.uri.clone()))
        .await
        .expect("read owned resource");
    assert!(matches!(
        read.contents.as_slice(),
        [ResourceContents::TextResourceContents { text, .. }] if text == r#"{"owner":"a"}"#
    ));
    assert!(client
        .peer()
        .read_resource(ReadResourceRequestParams::new(resource_b.uri))
        .await
        .is_err());

    client.cancel().await.expect("close resource client");
    drop(gateway);
    let _ = std::fs::remove_dir_all(config_dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn real_rmcp_annotations_list_get_and_read_resources_are_workspace_owned() {
    let config_dir = unique_gateway_config_dir("annotations");
    let layout = BrowserStorageLayout::new(&config_dir, "project-a");
    let store = BrowserResourceStore::open(&layout.resources_dir, BrowserResourceLimits::default())
        .expect("open annotation store");
    let owner_a = workspace("project-a", "conversation-a");
    let owner_b = workspace("project-a", "conversation-b");
    let screenshot = store
        .put(
            &owner_a,
            BrowserResourceKind::AnnotationScreenshot,
            "image/png",
            b"owned annotation screenshot",
            true,
        )
        .expect("store annotation screenshot");
    let snapshot_a = BrowserWorkspaceSnapshot {
        revision: BrowserRevision(1),
        tabs: vec![BrowserTabSnapshot {
            id: "tab-a".to_string(),
            title: "Fixture".to_string(),
            url: "https://example.test/form".to_string(),
            viewport: BrowserViewport::default(),
        }],
        selected_tab_id: Some("tab-a".to_string()),
        annotations: vec![fixture_annotation(
            "ann-a",
            &screenshot.id,
            "Review the save button",
        )],
        ..BrowserWorkspaceSnapshot::default()
    };
    let (bridge, inbox) = browser_command_channel(32);
    let commands: Arc<Mutex<Vec<(BrowserWorkspaceKey, BrowserCommand)>>> =
        Arc::new(Mutex::new(Vec::new()));
    let host = Arc::new(Mutex::new(BrowserHostState::new(&config_dir)));
    let gateway = BrowserGatewayHandle::start_with_app_config_dir(bridge, &config_dir)
        .expect("start annotation gateway");
    let registration_a = gateway
        .registrar()
        .register("annotation-client-a", owner_a, snapshot_a)
        .expect("register annotation owner");
    let registration_b = gateway
        .registrar()
        .register(
            "annotation-client-b",
            owner_b,
            BrowserWorkspaceSnapshot::default(),
        )
        .expect("register second conversation");
    let transport_a = StreamableHttpClientTransport::from_config(
        StreamableHttpClientTransportConfig::with_uri(gateway.endpoint().to_string())
            .auth_header(registration_a.access().bearer_token_for_launch()),
    );
    let transport_b = StreamableHttpClientTransport::from_config(
        StreamableHttpClientTransportConfig::with_uri(gateway.endpoint().to_string())
            .auth_header(registration_b.access().bearer_token_for_launch()),
    );
    let fake_host =
        run_fake_host_with_state(inbox, Arc::clone(&commands), Arc::clone(&host), Some(store));
    let scenario = async move {
        let client_a = ClientInfo::default()
            .serve(transport_a)
            .await
            .expect("initialize owner client");
        let listed = client_a
            .peer()
            .call_tool(
                CallToolRequestParams::new("browser_annotations").with_arguments(arguments(
                    json!({
                        "intent": "list the current page annotations",
                        "risk": "normal",
                        "operation": "list"
                    }),
                )),
            )
            .await
            .expect("list annotations");
        assert_eq!(listed.is_error, Some(false));
        let listed = listed
            .structured_content
            .expect("structured annotation list");
        assert_eq!(listed["annotations"][0]["id"], "ann-a");
        assert!(listed["annotations"][0].get("screenshot").is_none());

        let fetched = client_a
            .peer()
            .call_tool(
                CallToolRequestParams::new("browser_annotations").with_arguments(arguments(
                    json!({
                        "intent": "inspect the saved annotation details",
                        "risk": "normal",
                        "operation": "get",
                        "annotationId": "ann-a"
                    }),
                )),
            )
            .await
            .expect("get annotation");
        assert_eq!(fetched.is_error, Some(false));
        let fetched = fetched
            .structured_content
            .expect("structured annotation details");
        let screenshot_uri = fetched["resources"]["screenshot"]["uri"]
            .as_str()
            .unwrap()
            .to_string();
        let details_uri = fetched["resources"]["details"]["uri"]
            .as_str()
            .unwrap()
            .to_string();
        let screenshot_read = client_a
            .peer()
            .read_resource(ReadResourceRequestParams::new(screenshot_uri.clone()))
            .await
            .expect("read owned screenshot");
        assert!(matches!(
            screenshot_read.contents.as_slice(),
            [ResourceContents::BlobResourceContents { .. }]
        ));
        let details_read = client_a
            .peer()
            .read_resource(ReadResourceRequestParams::new(details_uri.clone()))
            .await
            .expect("read owned details");
        assert!(matches!(
            details_read.contents.as_slice(),
            [ResourceContents::TextResourceContents { text, .. }] if text.contains("ann-a")
        ));

        let client_b = ClientInfo::default()
            .serve(transport_b)
            .await
            .expect("initialize other conversation client");
        let cross_workspace = client_b
            .peer()
            .call_tool(
                CallToolRequestParams::new("browser_annotations").with_arguments(arguments(
                    json!({
                        "intent": "attempt a cross-workspace lookup",
                        "risk": "normal",
                        "operation": "get",
                        "annotationId": "ann-a"
                    }),
                )),
            )
            .await
            .expect("cross-workspace tool result");
        assert_eq!(cross_workspace.is_error, Some(true));
        assert_eq!(
            cross_workspace.structured_content.unwrap()["error"]["code"],
            "missing_annotation"
        );
        assert!(client_b
            .peer()
            .read_resource(ReadResourceRequestParams::new(screenshot_uri))
            .await
            .is_err());
        assert!(client_b
            .peer()
            .read_resource(ReadResourceRequestParams::new(details_uri))
            .await
            .is_err());

        client_b.cancel().await.expect("close other client");
        client_a.cancel().await.expect("close owner client");
        drop(gateway);
    };
    tokio::join!(fake_host, scenario);
    let _ = std::fs::remove_dir_all(config_dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn task4_mcp_commands_retain_one_agent_invocation_context() {
    let (bridge, mut inbox) = browser_command_channel(8);
    let gateway = BrowserGatewayHandle::start(bridge).expect("start gateway");
    let key = workspace("project-context", "conversation-context");
    let registration = gateway
        .registrar()
        .register(
            "context-client",
            key.clone(),
            BrowserWorkspaceSnapshot::default(),
        )
        .expect("register context client");
    let transport = StreamableHttpClientTransport::from_config(
        StreamableHttpClientTransportConfig::with_uri(gateway.endpoint().to_string())
            .auth_header(registration.access().bearer_token_for_launch()),
    );
    let scenario = async move {
        let client = ClientInfo::default()
            .serve(transport)
            .await
            .expect("initialize context client");
        let status = client
            .peer()
            .call_tool(
                CallToolRequestParams::new("browser_status").with_arguments(arguments(json!({
                    "intent": "inspect the active financial form",
                    "risk": "financial"
                }))),
            )
            .await
            .expect("call context-bearing status");
        assert_eq!(status.is_error, Some(false));
        client.cancel().await.expect("close context client");
    };
    let host = async move {
        let mut state = BrowserHostState::new("context-fake-host");
        let mut contexts: Vec<BrowserInvocationContext> = Vec::new();
        for _ in 0..4 {
            let request = inbox.recv().await.expect("context-routed request");
            contexts.push(request.context().clone());
            let command = request.command().clone();
            let result = match command {
                BrowserCommand::Ensure { snapshot } => state
                    .ensure_workspace(key.clone(), snapshot)
                    .map(|mutation| BrowserResponse::Workspace { mutation }),
                BrowserCommand::SetPaneOpen { open } => state
                    .set_pane_open(&key, open)
                    .map(|mutation| BrowserResponse::Workspace { mutation }),
                BrowserCommand::WorkspaceState => Ok(BrowserResponse::WorkspaceState {
                    snapshot: state.workspace(&key).unwrap().clone(),
                }),
                BrowserCommand::Status => Ok(BrowserResponse::Status {
                    status: BrowserHostStatus {
                        available: true,
                        platform: "windows".to_string(),
                        version: Some("fixture".to_string()),
                        diagnostic: None,
                    },
                }),
                other => panic!("unexpected context command: {other:?}"),
            };
            request.respond(result);
        }
        assert!(contexts.iter().all(|context| {
            context.actor == BrowserInvocationActor::Agent
                && context.intent == "inspect the active financial form"
                && context.declared_risk == BrowserRisk::Financial
        }));
        assert!(contexts
            .windows(2)
            .all(|pair| pair[0].operation_id == pair[1].operation_id));
    };

    tokio::join!(host, scenario);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn real_rmcp_client_lists_the_browser_tools_with_exact_bound_schemas() {
    let (bridge, inbox) = browser_command_channel(32);
    let commands: Arc<Mutex<Vec<(BrowserWorkspaceKey, BrowserCommand)>>> =
        Arc::new(Mutex::new(Vec::new()));
    let gateway = BrowserGatewayHandle::start(bridge).expect("start gateway");
    let observed_commands = Arc::clone(&commands);
    let scenario = async move {
        let registration = gateway
            .registrar()
            .register(
                "ai-process-a",
                workspace("project-a", "conversation-a"),
                BrowserWorkspaceSnapshot::default(),
            )
            .expect("register token");
        let transport = StreamableHttpClientTransport::from_config(
            StreamableHttpClientTransportConfig::with_uri(gateway.endpoint().to_string())
                .auth_header(registration.access().bearer_token_for_launch()),
        );
        let client = ClientInfo::default()
            .serve(transport)
            .await
            .expect("initialize real rmcp client");
        let server = client.peer_info().expect("initialized server information");
        assert_eq!(server.server_info.name, "devmanager-browser");
        assert_eq!(
            server.server_info.title.as_deref(),
            Some("devmanager-browser")
        );
        assert_eq!(server.server_info.version, "v1");
        assert!(server
            .instructions
            .as_deref()
            .is_some_and(|instructions| instructions.contains("per-conversation companion pane")));

        let listed = client.peer().list_tools(None).await.expect("list tools");
        let names = listed
            .tools
            .iter()
            .map(|tool| tool.name.as_ref())
            .collect::<Vec<_>>();
        assert_eq!(
            names,
            vec![
                "browser_act",
                "browser_annotations",
                "browser_cdp",
                "browser_console",
                "browser_downloads",
                "browser_navigate",
                "browser_network",
                "browser_performance",
                "browser_recording",
                "browser_screenshot",
                "browser_snapshot",
                "browser_status",
                "browser_tabs",
                "browser_upload",
                "browser_wait",
            ]
        );
        assert!(listed.tools.iter().all(|tool| {
            let required = tool
                .input_schema
                .get("required")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            required.contains(&json!("intent")) && required.contains(&json!("risk"))
        }));
        assert!(listed.tools.iter().all(|tool| {
            let properties = &tool.input_schema["properties"];
            properties.get("projectId").is_none()
                && properties.get("conversationId").is_none()
                && properties.get("aiTabId").is_none()
                && properties.get("workspaceKey").is_none()
        }));
        let status_tool = listed
            .tools
            .iter()
            .find(|tool| tool.name == "browser_status")
            .unwrap();
        let risk_ref = status_tool.input_schema["properties"]["risk"]["$ref"]
            .as_str()
            .expect("risk enum reference");
        let risk_definition = risk_ref
            .strip_prefix("#/$defs/")
            .expect("local risk definition");
        assert_eq!(
            status_tool.input_schema["$defs"][risk_definition]["enum"],
            json!([
                "normal",
                "financial",
                "destructive",
                "accountSecurity",
                "permissionChange",
                "outsideWorkspaceFile",
                "osPermission"
            ])
        );

        let annotations_tool = listed
            .tools
            .iter()
            .find(|tool| tool.name == "browser_annotations")
            .expect("browser_annotations tool");
        assert_eq!(annotations_tool.input_schema["additionalProperties"], false);
        assert_eq!(
            annotations_tool.input_schema["required"],
            json!(["intent", "risk", "operation"])
        );
        let annotation_properties = annotations_tool.input_schema["properties"]
            .as_object()
            .expect("annotation properties");
        let mut annotation_property_names =
            annotation_properties.keys().cloned().collect::<Vec<_>>();
        annotation_property_names.sort();
        assert_eq!(
            annotation_property_names,
            vec!["annotationId", "intent", "operation", "risk"]
        );
        let operation_ref = annotations_tool.input_schema["properties"]["operation"]["$ref"]
            .as_str()
            .expect("annotation operation enum reference");
        let operation_definition = operation_ref
            .strip_prefix("#/$defs/")
            .expect("local operation definition");
        assert_eq!(
            annotations_tool.input_schema["$defs"][operation_definition]["enum"],
            json!(["list", "get", "resolve", "unresolve", "delete"])
        );

        let unknown_field = client
            .peer()
            .call_tool(
                CallToolRequestParams::new("browser_annotations").with_arguments(arguments(
                    json!({
                        "intent": "attempt forbidden client-side routing",
                        "risk": "normal",
                        "operation": "list",
                        "projectId": "other-project"
                    }),
                )),
            )
            .await
            .expect("malformed annotation arguments return a tool result");
        assert_eq!(unknown_field.is_error, Some(true));
        assert_eq!(
            unknown_field.structured_content.unwrap()["error"]["code"],
            "invalid_request"
        );

        let blank_intent = client
            .peer()
            .call_tool(
                CallToolRequestParams::new("browser_annotations").with_arguments(arguments(
                    json!({
                        "intent": "  ",
                        "risk": "normal",
                        "operation": "list"
                    }),
                )),
            )
            .await
            .expect("blank intent returns a tool result");
        assert_eq!(blank_intent.is_error, Some(true));
        assert_eq!(
            blank_intent.structured_content.unwrap()["error"]["code"],
            "invalid_request"
        );

        let missing_annotation_id = client
            .peer()
            .call_tool(
                CallToolRequestParams::new("browser_annotations").with_arguments(arguments(
                    json!({
                        "intent": "inspect one annotation",
                        "risk": "normal",
                        "operation": "get"
                    }),
                )),
            )
            .await
            .expect("missing annotation id returns a tool result");
        assert_eq!(missing_annotation_id.is_error, Some(true));
        let missing_annotation_id = missing_annotation_id
            .structured_content
            .expect("typed missing-id result");
        assert_eq!(missing_annotation_id["error"]["code"], "invalid_request");
        assert!(missing_annotation_id["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("annotationId")));

        let status = client
            .peer()
            .call_tool(
                CallToolRequestParams::new("browser_status").with_arguments(arguments(json!({
                    "intent": "inspect the visible page",
                    "risk": "normal"
                }))),
            )
            .await
            .expect("call browser_status");
        assert_eq!(status.is_error, Some(false));
        let structured = status.structured_content.expect("structured status");
        assert_eq!(structured["ok"], true);
        assert_eq!(structured["workspace"]["projectId"], "project-a");
        assert_eq!(structured["workspace"]["aiTabId"], "conversation-a");
        assert_eq!(structured["paneOpen"], true);
        assert_eq!(structured["host"]["available"], true);
        assert!(structured.get("token").is_none());

        let recorded = observed_commands.lock().unwrap().clone();
        assert!(matches!(recorded[0].1, BrowserCommand::Ensure { .. }));
        assert_eq!(recorded[1].1, BrowserCommand::SetPaneOpen { open: true });
        assert_eq!(recorded[2].1, BrowserCommand::WorkspaceState);
        assert_eq!(recorded[3].1, BrowserCommand::Status);

        let blank_intent = client
            .peer()
            .call_tool(
                CallToolRequestParams::new("browser_status").with_arguments(arguments(json!({
                    "intent": "   ",
                    "risk": "normal"
                }))),
            )
            .await
            .expect("blank intent is a typed tool error");
        assert_eq!(blank_intent.is_error, Some(true));
        assert_eq!(
            blank_intent.structured_content.unwrap()["error"]["code"],
            "invalid_request"
        );

        let created = client
            .peer()
            .call_tool(
                CallToolRequestParams::new("browser_tabs").with_arguments(arguments(json!({
                    "intent": "open the app in another logical tab",
                    "risk": "normal",
                    "operation": "create",
                    "url": "https://example.test/created"
                }))),
            )
            .await
            .expect("create browser tab");
        assert_eq!(created.is_error, Some(false));
        let created = created.structured_content.unwrap();
        assert_eq!(created["tabs"].as_array().unwrap().len(), 2);
        let created_id = created["selectedTabId"].as_str().unwrap().to_string();

        let navigated = client
            .peer()
            .call_tool(
                CallToolRequestParams::new("browser_navigate").with_arguments(arguments(json!({
                    "intent": "load the fixture route",
                    "risk": "normal",
                    "operation": "goto",
                    "url": "https://example.test/navigated"
                }))),
            )
            .await
            .expect("navigate selected tab");
        assert_eq!(navigated.is_error, Some(false));
        let navigated = navigated.structured_content.unwrap();
        assert_eq!(navigated["tab"]["id"], created_id);
        assert_eq!(navigated["tab"]["url"], "https://example.test/navigated");
        assert_eq!(navigated["loadAcknowledged"], true);

        let missing_tab = client
            .peer()
            .call_tool(
                CallToolRequestParams::new("browser_tabs").with_arguments(arguments(json!({
                    "intent": "select a tab",
                    "risk": "normal",
                    "operation": "select"
                }))),
            )
            .await
            .expect("missing tab id is typed");
        assert_eq!(missing_tab.is_error, Some(true));
        assert_eq!(
            missing_tab.structured_content.unwrap()["error"]["code"],
            "invalid_request"
        );

        let blocked_url = client
            .peer()
            .call_tool(
                CallToolRequestParams::new("browser_navigate").with_arguments(arguments(json!({
                    "intent": "attempt an unsafe scheme",
                    "risk": "normal",
                    "operation": "goto",
                    "url": "javascript:alert(1)"
                }))),
            )
            .await
            .expect("invalid URL is a typed browser error");
        assert_eq!(blocked_url.is_error, Some(true));
        assert_eq!(
            blocked_url.structured_content.unwrap()["error"]["code"],
            "navigation_failure"
        );

        let closed = client
            .peer()
            .call_tool(
                CallToolRequestParams::new("browser_tabs").with_arguments(arguments(json!({
                    "intent": "close the extra tab",
                    "risk": "normal",
                    "operation": "close",
                    "tabId": created_id
                }))),
            )
            .await
            .expect("close browser tab");
        assert_eq!(closed.is_error, Some(false));
        assert_eq!(
            closed.structured_content.unwrap()["tabs"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        let _ = client.cancel().await;
    };
    let (_, ()) = tokio::join!(run_fake_host(inbox, commands), scenario);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn browser_recording_has_one_exact_bounded_route_free_schema_and_typed_malformed_errors() {
    let (bridge, _inbox) = browser_command_channel(8);
    let gateway = BrowserGatewayHandle::start(bridge).expect("start gateway");
    let registration = gateway
        .registrar()
        .register(
            "recording-schema-process",
            workspace("recording-schema-project", "recording-schema-conversation"),
            BrowserWorkspaceSnapshot::default(),
        )
        .expect("register recording schema token");
    let transport = StreamableHttpClientTransport::from_config(
        StreamableHttpClientTransportConfig::with_uri(gateway.endpoint().to_string())
            .auth_header(registration.access().bearer_token_for_launch()),
    );
    let client = ClientInfo::default()
        .serve(transport)
        .await
        .expect("initialize recording schema client");

    let listed = client.peer().list_tools(None).await.expect("list tools");
    let recording = listed
        .tools
        .iter()
        .find(|tool| tool.name == "browser_recording")
        .expect("browser_recording tool");
    assert_eq!(recording.input_schema["additionalProperties"], false);
    assert_eq!(
        recording.input_schema["required"],
        json!(["intent", "risk", "operation"])
    );
    let properties = recording.input_schema["properties"]
        .as_object()
        .expect("recording properties");
    let mut property_names = properties.keys().cloned().collect::<Vec<_>>();
    property_names.sort();
    assert_eq!(property_names, vec!["intent", "operation", "risk"]);
    let operation_ref = recording.input_schema["properties"]["operation"]["$ref"]
        .as_str()
        .expect("recording operation enum reference");
    let operation_definition = operation_ref
        .strip_prefix("#/$defs/")
        .expect("local recording operation definition");
    assert_eq!(
        recording.input_schema["$defs"][operation_definition]["enum"],
        json!(["status", "start", "stop", "review", "discard", "save"])
    );
    let risk_ref = recording.input_schema["properties"]["risk"]["$ref"]
        .as_str()
        .expect("recording risk enum reference");
    let risk_definition = risk_ref
        .strip_prefix("#/$defs/")
        .expect("local recording risk definition");
    assert_eq!(
        recording.input_schema["$defs"][risk_definition]["enum"],
        json!([
            "normal",
            "financial",
            "destructive",
            "accountSecurity",
            "permissionChange",
            "outsideWorkspaceFile",
            "osPermission"
        ])
    );
    assert_eq!(
        recording.input_schema["properties"]["intent"]["maxLength"],
        1024
    );
    let schema_text = serde_json::to_string(&recording.input_schema).unwrap();
    for forbidden in [
        "projectId",
        "conversationId",
        "aiTabId",
        "workspaceKey",
        "route",
        "tabId",
        "token",
        "password",
        "secret",
        "path",
        "fileContent",
    ] {
        assert!(
            !schema_text.contains(forbidden),
            "recording schema exposed forbidden field {forbidden}"
        );
    }

    for malformed in [
        json!({
            "intent": "attempt client-side routing",
            "risk": "normal",
            "operation": "status",
            "projectId": "other-project"
        }),
        json!({
            "intent": "   ",
            "risk": "normal",
            "operation": "status"
        }),
        json!({
            "intent": "x".repeat(1025),
            "risk": "normal",
            "operation": "status"
        }),
        json!({
            "intent": "inspect the recording",
            "risk": "normal",
            "operation": "status",
            "instanceId": 1
        }),
        json!({
            "intent": "inspect the recording",
            "risk": "normal",
            "operation": "mutate"
        }),
    ] {
        let result = client
            .peer()
            .call_tool(
                CallToolRequestParams::new("browser_recording")
                    .with_arguments(arguments(malformed)),
            )
            .await
            .expect("malformed recording arguments return a tool result");
        assert_eq!(result.is_error, Some(true));
        assert_eq!(
            result.structured_content.unwrap()["error"]["code"],
            "invalid_request"
        );
    }

    client
        .cancel()
        .await
        .expect("close recording schema client");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn browser_recording_valid_operations_dispatch_through_the_authenticated_workspace_bridge() {
    let (bridge, inbox) = browser_command_channel(16);
    let observed = Arc::new(Mutex::new(Vec::new()));
    let review_resource = fixture_resource(
        "res-00000000000000000000000000000009",
        BrowserResourceKind::WorkflowReview,
        "application/json",
        512,
    );
    let result = |operation, status, resource| BrowserRecordingResult {
        operation,
        status,
        recording_id: (status != BrowserRecordingStatus::Inactive).then_some(41),
        recipe_id: (status == BrowserRecordingStatus::Review).then(|| "checkout-flow".to_string()),
        step_count: usize::from(status == BrowserRecordingStatus::Review) * 2,
        inputs: (status == BrowserRecordingStatus::Review)
            .then(|| {
                vec![
                    BrowserRecordingInputSummary {
                        name: "customer_name".to_string(),
                        kind: BrowserRecipeInputKind::Text,
                    },
                    BrowserRecordingInputSummary {
                        name: "account_password".to_string(),
                        kind: BrowserRecipeInputKind::Secret,
                    },
                ]
            })
            .unwrap_or_default(),
        valid: status == BrowserRecordingStatus::Review,
        resource,
        overwrote_existing: None,
    };
    let scripted = Arc::new(Mutex::new(VecDeque::from([
        Ok(result(
            BrowserRecordingOperation::Status,
            BrowserRecordingStatus::Inactive,
            None,
        )),
        Ok(result(
            BrowserRecordingOperation::Start,
            BrowserRecordingStatus::Recording,
            None,
        )),
        Ok(result(
            BrowserRecordingOperation::Stop,
            BrowserRecordingStatus::Review,
            Some(review_resource.clone()),
        )),
        Ok(result(
            BrowserRecordingOperation::Review,
            BrowserRecordingStatus::Review,
            Some(review_resource),
        )),
        Ok(BrowserRecordingResult {
            operation: BrowserRecordingOperation::Discard,
            status: BrowserRecordingStatus::Inactive,
            recording_id: Some(41),
            recipe_id: Some("checkout-flow".to_string()),
            step_count: 0,
            inputs: Vec::new(),
            valid: false,
            resource: None,
            overwrote_existing: None,
        }),
        Ok(BrowserRecordingResult {
            operation: BrowserRecordingOperation::Save,
            status: BrowserRecordingStatus::Inactive,
            recording_id: Some(42),
            recipe_id: Some("checkout-flow".to_string()),
            step_count: 2,
            inputs: Vec::new(),
            valid: true,
            resource: None,
            overwrote_existing: Some(false),
        }),
    ])));
    let project_root = unique_gateway_config_dir("recording-bridge-root");
    std::fs::create_dir_all(&project_root).expect("create recording bridge project root");
    let canonical_project_root = project_root.canonicalize().unwrap();
    let gateway = BrowserGatewayHandle::start(bridge).expect("start gateway");
    let expected_workspace = workspace("recording-project", "recording-conversation");
    let registration = gateway
        .registrar()
        .register_with_project_root(
            "recording-bridge-process",
            expected_workspace.clone(),
            BrowserWorkspaceSnapshot::default(),
            &project_root,
        )
        .expect("register recording bridge token");
    let host = run_recording_bridge_host(inbox, Arc::clone(&observed), scripted);
    let scenario = async move {
        let transport = StreamableHttpClientTransport::from_config(
            StreamableHttpClientTransportConfig::with_uri(gateway.endpoint().to_string())
                .auth_header(registration.access().bearer_token_for_launch()),
        );
        let client = ClientInfo::default()
            .serve(transport)
            .await
            .expect("initialize recording bridge client");

        for (operation, expected_status) in [
            ("status", "inactive"),
            ("start", "recording"),
            ("stop", "review"),
            ("review", "review"),
            ("discard", "inactive"),
            ("save", "inactive"),
        ] {
            let response = client
                .peer()
                .call_tool(
                    CallToolRequestParams::new("browser_recording").with_arguments(arguments(
                        json!({
                            "intent": format!("{operation} the exact workflow recording"),
                            "risk": "normal",
                            "operation": operation,
                        }),
                    )),
                )
                .await
                .expect("recording operation returns a tool result");
            assert_eq!(response.is_error, Some(false), "{operation} failed");
            let body = response.structured_content.expect("recording result body");
            assert_eq!(body["version"], 1);
            assert_eq!(body["operation"], operation);
            assert_eq!(body["recording"]["status"], expected_status);
            assert!(body.get("workspace").is_none());
            assert!(body.get("path").is_none());
            if matches!(operation, "stop" | "review") {
                assert_eq!(body["recording"]["stepCount"], 2);
                assert_eq!(
                    body["recording"]["inputs"],
                    json!([
                        {"name":"customer_name","kind":"text"},
                        {"name":"account_password","kind":"secret"}
                    ])
                );
                assert!(body["resource"]["uri"]
                    .as_str()
                    .is_some_and(|uri| uri.starts_with("devmanager-browser://resource/")));
            }
            if operation == "save" {
                assert_eq!(body["overwroteExisting"], false);
            }
        }

        let calls = observed.lock().unwrap().clone();
        let recording_calls = calls
            .iter()
            .filter_map(
                |(workspace_key, command, context, project_root)| match command {
                    BrowserCommand::Recording { operation } => {
                        Some((workspace_key, operation, context, project_root))
                    }
                    _ => None,
                },
            )
            .collect::<Vec<_>>();
        assert_eq!(recording_calls.len(), 6);
        assert!(recording_calls
            .iter()
            .all(|(workspace_key, _, _, _)| *workspace_key == &expected_workspace));
        assert_eq!(
            recording_calls
                .iter()
                .map(|(_, operation, _, _)| **operation)
                .collect::<Vec<_>>(),
            vec![
                BrowserRecordingOperation::Status,
                BrowserRecordingOperation::Start,
                BrowserRecordingOperation::Stop,
                BrowserRecordingOperation::Review,
                BrowserRecordingOperation::Discard,
                BrowserRecordingOperation::Save,
            ]
        );
        assert!(recording_calls.iter().all(|(_, _, context, project_root)| {
            context.actor == BrowserInvocationActor::Agent
                && context.declared_risk == BrowserRisk::Normal
                && !context.intent.trim().is_empty()
                && project_root.as_ref() == Some(&canonical_project_root)
        }));

        client
            .cancel()
            .await
            .expect("close recording bridge client");
    };
    tokio::join!(host, scenario);
    std::fs::remove_dir_all(project_root).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn browser_recording_tool_failure_is_typed_and_does_not_end_the_authenticated_session() {
    let (bridge, inbox) = browser_command_channel(8);
    let observed = Arc::new(Mutex::new(Vec::new()));
    let scripted = Arc::new(Mutex::new(VecDeque::from([
        Err(devmanager::browser::BrowserError::CrashedView {
            message: "fixture recording failure".to_string(),
        }),
        Ok(BrowserRecordingResult {
            operation: BrowserRecordingOperation::Status,
            status: BrowserRecordingStatus::Inactive,
            recording_id: None,
            recipe_id: None,
            step_count: 0,
            inputs: Vec::new(),
            valid: false,
            resource: None,
            overwrote_existing: None,
        }),
    ])));
    let project_root = unique_gateway_config_dir("recording-failure-root");
    std::fs::create_dir_all(&project_root).unwrap();
    let gateway = BrowserGatewayHandle::start(bridge).expect("start gateway");
    let registration = gateway
        .registrar()
        .register_with_project_root(
            "recording-failure-process",
            workspace(
                "recording-failure-project",
                "recording-failure-conversation",
            ),
            BrowserWorkspaceSnapshot::default(),
            &project_root,
        )
        .unwrap();
    let host = run_recording_bridge_host(inbox, Arc::clone(&observed), scripted);
    let scenario = async move {
        let transport = StreamableHttpClientTransport::from_config(
            StreamableHttpClientTransportConfig::with_uri(gateway.endpoint().to_string())
                .auth_header(registration.access().bearer_token_for_launch()),
        );
        let client = ClientInfo::default().serve(transport).await.unwrap();
        let failed = client
            .peer()
            .call_tool(
                CallToolRequestParams::new("browser_recording").with_arguments(arguments(json!({
                    "intent": "review the current workflow recording",
                    "risk": "normal",
                    "operation": "review",
                }))),
            )
            .await
            .expect("host failure is returned as a tool result");
        assert_eq!(failed.is_error, Some(true));
        assert_eq!(
            failed.structured_content.unwrap()["error"]["code"],
            "crashed_view"
        );

        let status = client
            .peer()
            .call_tool(
                CallToolRequestParams::new("browser_recording").with_arguments(arguments(json!({
                    "intent": "check recording state after the failure",
                    "risk": "normal",
                    "operation": "status",
                }))),
            )
            .await
            .expect("same authenticated session remains usable");
        assert_eq!(status.is_error, Some(false));
        assert_eq!(
            status.structured_content.unwrap()["recording"]["status"],
            "inactive"
        );
        client.cancel().await.unwrap();
    };
    tokio::join!(host, scenario);
    assert_eq!(
        observed
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, command, _, _)| matches!(command, BrowserCommand::Recording { .. }))
            .count(),
        2
    );
    std::fs::remove_dir_all(project_root).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn real_rmcp_client_routes_all_ten_automation_groups_with_compact_results() {
    let config_dir = unique_gateway_config_dir("automation-tools");
    let project_root = config_dir.join("project-root");
    std::fs::create_dir_all(&project_root).expect("create automation project root");
    std::fs::write(project_root.join("fixture-upload.txt"), b"fixture upload")
        .expect("write upload fixture");

    let (bridge, inbox) = browser_command_channel(64);
    let commands: Arc<Mutex<Vec<(BrowserWorkspaceKey, BrowserCommand)>>> =
        Arc::new(Mutex::new(Vec::new()));
    let gateway = BrowserGatewayHandle::start_with_app_config_dir(bridge, &config_dir)
        .expect("start automation gateway");
    let scenario_project_root = project_root.clone();
    let scenario = async move {
        let key = workspace("project-tools", "conversation-tools");
        let initial_snapshot = BrowserWorkspaceSnapshot {
            tabs: vec![BrowserTabSnapshot {
                id: "tab-main".to_string(),
                title: "Loopback fixture".to_string(),
                url: "http://127.0.0.1:4173/".to_string(),
                viewport: BrowserViewport::default(),
            }],
            selected_tab_id: Some("tab-main".to_string()),
            ..BrowserWorkspaceSnapshot::default()
        };
        let registration = gateway
            .registrar()
            .register_with_project_root(
                "automation-client",
                key,
                initial_snapshot,
                &scenario_project_root,
            )
            .expect("register automation client");
        let transport = StreamableHttpClientTransport::from_config(
            StreamableHttpClientTransportConfig::with_uri(gateway.endpoint().to_string())
                .auth_header(registration.access().bearer_token_for_launch()),
        );
        let client = ClientInfo::default()
            .serve(transport)
            .await
            .expect("initialize automation client");

        let calls = [
            (
                "browser_snapshot",
                json!({"intent":"inspect semantic page","risk":"normal"}),
                "snapshot",
            ),
            (
                "browser_screenshot",
                json!({"intent":"capture viewport","risk":"normal","mode":"viewport"}),
                "screenshot",
            ),
            (
                "browser_wait",
                json!({
                    "intent":"wait for fixture mutation",
                    "risk":"normal",
                    "condition":{"type":"duration","durationMs":1},
                    "timeoutMs":100
                }),
                "wait",
            ),
            (
                "browser_act",
                json!({
                    "intent":"focus fixture target",
                    "risk":"normal",
                    "actions":[{
                        "operation":"focus",
                        "target":{"locator":{"testId":"fixture-target"}}
                    }]
                }),
                "action",
            ),
            (
                "browser_console",
                json!({"intent":"inspect console","risk":"normal","operation":"list"}),
                "console",
            ),
            (
                "browser_network",
                json!({"intent":"inspect requests","risk":"normal","operation":"list"}),
                "network",
            ),
            (
                "browser_performance",
                json!({"intent":"inspect timings","risk":"normal","operation":"snapshot"}),
                "performance",
            ),
            (
                "browser_upload",
                json!({
                    "intent":"upload project fixture",
                    "risk":"normal",
                    "target":{"locator":{"testId":"fixture-upload"}},
                    "paths":["fixture-upload.txt"]
                }),
                "upload",
            ),
            (
                "browser_downloads",
                json!({"intent":"list downloads","risk":"normal","operation":"list"}),
                "downloads",
            ),
            (
                "browser_cdp",
                json!({
                    "intent":"evaluate fixture expression",
                    "risk":"normal",
                    "method":"Runtime.evaluate",
                    "params":{"expression":"2 + 2"}
                }),
                "cdp",
            ),
        ];

        for (tool_name, tool_arguments, expected_type) in calls {
            let result = client
                .peer()
                .call_tool(
                    CallToolRequestParams::new(tool_name).with_arguments(arguments(tool_arguments)),
                )
                .await
                .unwrap_or_else(|error| panic!("call {tool_name}: {error}"));
            assert_eq!(result.is_error, Some(false), "{tool_name}");
            let structured = result
                .structured_content
                .unwrap_or_else(|| panic!("structured result for {tool_name}"));
            assert_eq!(structured["ok"], true, "{tool_name}");
            assert_eq!(structured["version"], 1, "{tool_name}");
            if expected_type == "upload" {
                assert_eq!(structured["uploadedCount"], 1);
                assert!(structured.get("paths").is_none());
            } else {
                assert_eq!(structured["result"]["type"], expected_type, "{tool_name}");
            }
            if matches!(expected_type, "snapshot" | "screenshot") {
                let resource = &structured["result"]["resource"];
                assert!(resource["uri"]
                    .as_str()
                    .is_some_and(|uri| uri.starts_with("devmanager-browser://resource/res-")));
                assert!(resource["byteSize"].as_u64().is_some_and(|size| size > 0));
            }
        }

        let typed_failure = client
            .peer()
            .call_tool(
                CallToolRequestParams::new("browser_wait").with_arguments(arguments(json!({
                    "intent":"exercise bounded timeout validation",
                    "risk":"normal",
                    "condition":{"type":"duration","durationMs":1},
                    "timeoutMs":0
                }))),
            )
            .await
            .expect("typed invalid wait result");
        assert_eq!(typed_failure.is_error, Some(true));
        assert_eq!(
            typed_failure.structured_content.unwrap()["error"]["code"],
            "invalid_request"
        );

        let host_failure = client
            .peer()
            .call_tool(
                CallToolRequestParams::new("browser_wait").with_arguments(arguments(json!({
                    "intent":"exercise typed host timeout",
                    "risk":"normal",
                    "condition":{"type":"duration","durationMs":50},
                    "timeoutMs":13
                }))),
            )
            .await
            .expect("typed host timeout result");
        assert_eq!(host_failure.is_error, Some(true));
        assert_eq!(
            host_failure.structured_content.unwrap()["error"]["code"],
            "timeout"
        );

        let missing_upload_tab = client
            .peer()
            .call_tool(
                CallToolRequestParams::new("browser_upload").with_arguments(arguments(json!({
                    "intent":"reject an upload to a nonexistent tab",
                    "risk":"normal",
                    "tabId":"missing-tab",
                    "target":{"locator":{"testId":"fixture-upload"}},
                    "paths":["fixture-upload.txt"]
                }))),
            )
            .await
            .expect("typed missing upload tab result");
        assert_eq!(missing_upload_tab.is_error, Some(true));
        assert_eq!(
            missing_upload_tab.structured_content.unwrap()["error"]["code"],
            "invalid_request"
        );

        client.cancel().await.expect("close automation client");
    };

    let (_, ()) = tokio::join!(run_fake_host(inbox, commands), scenario);
    let _ = std::fs::remove_dir_all(config_dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mcp_refreshes_user_changed_workspace_state_before_each_tool_operation() {
    let (bridge, inbox) = browser_command_channel(32);
    let commands: Arc<Mutex<Vec<(BrowserWorkspaceKey, BrowserCommand)>>> =
        Arc::new(Mutex::new(Vec::new()));
    let host = Arc::new(Mutex::new(BrowserHostState::new(PathBuf::from(
        "gateway-live-state-host",
    ))));
    let fake_host = run_fake_host_with_state(inbox, Arc::clone(&commands), Arc::clone(&host), None);
    let gateway = BrowserGatewayHandle::start(bridge).expect("start gateway");
    let scenario = async move {
        let key = workspace("project-live", "conversation-live");
        let registration = gateway
            .registrar()
            .register(
                "ai-process-live",
                key.clone(),
                BrowserWorkspaceSnapshot::default(),
            )
            .expect("register live-state token");
        let transport = StreamableHttpClientTransport::from_config(
            StreamableHttpClientTransportConfig::with_uri(gateway.endpoint().to_string())
                .auth_header(registration.access().bearer_token_for_launch()),
        );
        let client = ClientInfo::default()
            .serve(transport)
            .await
            .expect("initialize live-state client");

        client
            .peer()
            .call_tool(
                CallToolRequestParams::new("browser_status").with_arguments(arguments(json!({
                    "intent": "initialize my companion pane",
                    "risk": "normal"
                }))),
            )
            .await
            .expect("initialize browser workspace");

        let external = host
            .lock()
            .unwrap()
            .create_tab(&key, "https://example.test/user-selected")
            .expect("user creates and selects a tab outside MCP");
        let external_tab_id = external
            .snapshot
            .selected_tab_id
            .clone()
            .expect("externally selected tab");
        let external_revision = external.revision.0;

        let status = client
            .peer()
            .call_tool(
                CallToolRequestParams::new("browser_status").with_arguments(arguments(json!({
                    "intent": "read the current user-selected tab",
                    "risk": "normal"
                }))),
            )
            .await
            .expect("read refreshed browser status")
            .structured_content
            .expect("structured refreshed status");
        assert_eq!(status["selectedTabId"], external_tab_id);
        assert_eq!(status["revision"], external_revision);

        let navigated = client
            .peer()
            .call_tool(
                CallToolRequestParams::new("browser_navigate").with_arguments(arguments(json!({
                    "intent": "navigate the currently selected tab",
                    "risk": "normal",
                    "operation": "goto",
                    "url": "https://example.test/after-refresh"
                }))),
            )
            .await
            .expect("navigate refreshed selection")
            .structured_content
            .expect("structured navigation result");
        assert_eq!(navigated["tab"]["id"], external_tab_id);
        assert_eq!(
            navigated["tab"]["url"],
            "https://example.test/after-refresh"
        );

        let recorded = commands.lock().unwrap().clone();
        let navigate_index = recorded
            .iter()
            .position(|(_, command)| matches!(command, BrowserCommand::Navigate { .. }))
            .expect("navigate command recorded");
        assert!(matches!(
            recorded[navigate_index - 1].1,
            BrowserCommand::WorkspaceState
        ));
        client.cancel().await.expect("close live-state client");
    };

    let (_, ()) = tokio::join!(fake_host, scenario);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gateway_shutdown_is_bounded_with_a_live_rmcp_client() {
    let (bridge, mut inbox) = browser_command_channel(8);
    let gateway = BrowserGatewayHandle::start(bridge).unwrap();
    let registration = gateway
        .registrar()
        .register(
            "live-client-process",
            workspace("project", "conversation"),
            BrowserWorkspaceSnapshot::default(),
        )
        .unwrap();
    let transport = StreamableHttpClientTransport::from_config(
        StreamableHttpClientTransportConfig::with_uri(gateway.endpoint().to_string())
            .auth_header(registration.access().bearer_token_for_launch()),
    );
    let client = ClientInfo::default()
        .serve(transport)
        .await
        .expect("initialize live client");

    let (completed, mut dropping) = {
        let peer = client.peer();
        let call = peer.call_tool(CallToolRequestParams::new("browser_status").with_arguments(
            arguments(json!({
                "intent": "hold an active request during shutdown",
                "risk": "normal"
            })),
        ));
        tokio::pin!(call);
        let pending_request = tokio::select! {
            request = inbox.recv() => request.expect("active controller request"),
            result = &mut call => panic!("tool call unexpectedly completed before host response: {result:?}"),
        };
        assert!(matches!(
            pending_request.command(),
            BrowserCommand::Ensure { .. }
        ));

        let mut dropping = tokio::task::spawn_blocking(move || drop(gateway));
        let completed = tokio::time::timeout(Duration::from_millis(500), &mut dropping)
            .await
            .is_ok();
        pending_request.respond(Err(devmanager::browser::BrowserError::Interrupted));
        let _ = tokio::time::timeout(Duration::from_secs(2), &mut call).await;
        (completed, dropping)
    };
    let _ = client.cancel().await;
    if !completed {
        tokio::time::timeout(Duration::from_secs(2), &mut dropping)
            .await
            .expect("gateway drop should finish after the active request is released")
            .expect("gateway drop worker");
    }
    assert!(
        completed,
        "gateway drop must be bounded while an authenticated request is active"
    );
}
