use base64::Engine as _;
use devmanager::browser::{
    browser_command_channel, BrowserCommand, BrowserCommandInbox, BrowserGatewayHandle,
    BrowserHostState, BrowserHostStatus, BrowserResponse, BrowserWorkspaceKey,
    BrowserWorkspaceSnapshot,
};
use rmcp::model::{CallToolRequestParams, ClientInfo};
use rmcp::transport::{
    streamable_http_client::StreamableHttpClientTransportConfig, StreamableHttpClientTransport,
};
use rmcp::ServiceExt as _;
use serde_json::{json, Map, Value};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

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
    )
    .await;
}

async fn run_fake_host_with_state(
    mut inbox: BrowserCommandInbox,
    commands: Arc<Mutex<Vec<(BrowserWorkspaceKey, BrowserCommand)>>>,
    host: Arc<Mutex<BrowserHostState>>,
) {
    while let Some(request) = inbox.recv().await {
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
            other => panic!("unexpected fake-host command: {other:?}"),
        };
        drop(host);
        request.respond(result);
    }
}

#[test]
fn token_is_256_bits_rotates_on_replacement_and_stale_auth_is_rejected() {
    let (bridge, _inbox) = browser_command_channel(8);
    let gateway = BrowserGatewayHandle::start(bridge).expect("start gateway");
    let registrar = gateway.registrar();
    let first = registrar
        .register(
            "ai-process-a",
            workspace("project-a", "conversation-a"),
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
            workspace("project-a", "conversation-a"),
            BrowserWorkspaceSnapshot::default(),
        )
        .expect("register replacement token");
    assert_ne!(
        first.access().bearer_token_for_launch(),
        replacement.access().bearer_token_for_launch()
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
async fn real_rmcp_client_lists_and_calls_only_the_three_v1_tools() {
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
            vec!["browser_navigate", "browser_status", "browser_tabs"]
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
async fn mcp_refreshes_user_changed_workspace_state_before_each_tool_operation() {
    let (bridge, inbox) = browser_command_channel(32);
    let commands: Arc<Mutex<Vec<(BrowserWorkspaceKey, BrowserCommand)>>> =
        Arc::new(Mutex::new(Vec::new()));
    let host = Arc::new(Mutex::new(BrowserHostState::new(PathBuf::from(
        "gateway-live-state-host",
    ))));
    let fake_host = run_fake_host_with_state(inbox, Arc::clone(&commands), Arc::clone(&host));
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
