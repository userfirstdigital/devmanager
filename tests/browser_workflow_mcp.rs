use devmanager::browser::{
    browser_command_channel, get_browser_workflow_recipe, list_browser_workflow_recipes,
    save_recipe, BrowserCommand, BrowserGatewayHandle, BrowserHostState, BrowserRecipeAction,
    BrowserRecipeInput, BrowserRecipeInputKind, BrowserRecipeStep, BrowserRecipeV1,
    BrowserRecipeValue, BrowserRecipeViewport, BrowserResourceKind, BrowserResourceLimits,
    BrowserResourceStore, BrowserResponse, BrowserWorkspaceKey, BrowserWorkspaceSnapshot,
    BROWSER_RECIPE_SCHEMA_VERSION,
};
use rmcp::model::{CallToolRequestParams, ClientInfo};
use rmcp::transport::{
    streamable_http_client::StreamableHttpClientTransportConfig, StreamableHttpClientTransport,
};
use rmcp::ServiceExt as _;
use serde_json::{json, Map, Value};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

fn workspace(project: &str, conversation: &str) -> BrowserWorkspaceKey {
    BrowserWorkspaceKey::new(project, conversation).expect("valid browser workspace key")
}

fn arguments(value: Value) -> Map<String, Value> {
    serde_json::from_value(value).expect("tool arguments object")
}

fn local_schema_ref(schema: &Value) -> &str {
    schema["$ref"].as_str().unwrap_or_else(|| {
        schema["anyOf"]
            .as_array()
            .and_then(|variants| variants.iter().find_map(|variant| variant["$ref"].as_str()))
            .expect("local schema reference")
    })
}

fn unique_temp_dir(label: &str) -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    std::env::temp_dir().join(format!(
        "devmanager-workflow-mcp-{label}-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ))
}

fn recipe(id: &str, name: &str) -> BrowserRecipeV1 {
    BrowserRecipeV1 {
        schema_version: BROWSER_RECIPE_SCHEMA_VERSION,
        id: id.to_string(),
        name: name.to_string(),
        description: format!("{name} description"),
        start_url: "https://example.test/start".to_string(),
        viewport: BrowserRecipeViewport {
            width: 1280,
            height: 720,
            scale_percent: 100,
        },
        inputs: vec![
            BrowserRecipeInput {
                name: "query".to_string(),
                kind: BrowserRecipeInputKind::Text,
                default_value: Some("private default omitted from metadata".to_string()),
            },
            BrowserRecipeInput {
                name: "destination".to_string(),
                kind: BrowserRecipeInputKind::Url,
                default_value: None,
            },
            BrowserRecipeInput {
                name: "upload".to_string(),
                kind: BrowserRecipeInputKind::File,
                default_value: None,
            },
        ],
        steps: vec![BrowserRecipeStep {
            id: "navigate".to_string(),
            action: BrowserRecipeAction::Navigate {
                url: BrowserRecipeValue::Input {
                    name: "destination".to_string(),
                },
            },
            wait: None,
            assertions: Vec::new(),
        }],
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn browser_workflow_exposes_one_exact_seven_operation_tool() {
    let (bridge, _inbox) = browser_command_channel(8);
    let gateway = BrowserGatewayHandle::start(bridge).expect("start gateway");
    let registration = gateway
        .registrar()
        .register(
            "workflow-schema-process",
            workspace("workflow-schema-project", "workflow-schema-conversation"),
            BrowserWorkspaceSnapshot::default(),
        )
        .expect("register workflow schema token");
    let transport = StreamableHttpClientTransport::from_config(
        StreamableHttpClientTransportConfig::with_uri(gateway.endpoint().to_string())
            .auth_header(registration.access().bearer_token_for_launch()),
    );
    let client = ClientInfo::default()
        .serve(transport)
        .await
        .expect("initialize workflow schema client");

    let listed = client.peer().list_tools(None).await.expect("list tools");
    let workflow_tools = listed
        .tools
        .iter()
        .filter(|tool| tool.name == "browser_workflow")
        .collect::<Vec<_>>();
    assert_eq!(workflow_tools.len(), 1, "one browser_workflow tool");
    let workflow = workflow_tools[0];
    assert_eq!(workflow.input_schema["additionalProperties"], false);
    assert_eq!(
        workflow.input_schema["required"],
        json!(["intent", "risk", "operation"])
    );
    let properties = workflow.input_schema["properties"]
        .as_object()
        .expect("workflow properties");
    let mut property_names = properties.keys().cloned().collect::<Vec<_>>();
    property_names.sort();
    assert_eq!(
        property_names,
        vec![
            "candidate",
            "confirm",
            "inputs",
            "intent",
            "operation",
            "recipeId",
            "repairId",
            "replayInstanceId",
            "resume",
            "risk",
        ]
    );
    assert_eq!(
        workflow.input_schema["properties"]["intent"]["maxLength"],
        1024
    );
    assert_eq!(
        workflow.input_schema["properties"]["recipeId"]["maxLength"],
        128
    );
    assert_eq!(
        workflow.input_schema["properties"]["inputs"]["maxItems"],
        64
    );
    assert_eq!(
        workflow.input_schema["properties"]["replayInstanceId"]["minimum"],
        1
    );
    assert_eq!(
        workflow.input_schema["properties"]["repairId"]["minimum"],
        1
    );
    let operation_ref = workflow.input_schema["properties"]["operation"]["$ref"]
        .as_str()
        .expect("workflow operation enum reference");
    let operation_definition = operation_ref
        .strip_prefix("#/$defs/")
        .expect("local workflow operation definition");
    assert_eq!(
        workflow.input_schema["$defs"][operation_definition]["enum"],
        json!([
            "list",
            "get",
            "replay",
            "status",
            "cancel",
            "repairPreview",
            "repairApply"
        ])
    );
    let risk_ref = workflow.input_schema["properties"]["risk"]["$ref"]
        .as_str()
        .expect("workflow risk enum reference");
    let risk_definition = risk_ref
        .strip_prefix("#/$defs/")
        .expect("local workflow risk definition");
    assert_eq!(
        workflow.input_schema["$defs"][risk_definition]["enum"],
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
    let input_ref = workflow.input_schema["properties"]["inputs"]["items"]["$ref"]
        .as_str()
        .expect("workflow public input reference");
    let input_definition = input_ref
        .strip_prefix("#/$defs/")
        .expect("local workflow input definition");
    let input_schema = &workflow.input_schema["$defs"][input_definition];
    assert_eq!(input_schema["additionalProperties"], false);
    assert_eq!(input_schema["required"], json!(["name", "kind", "value"]));
    assert_eq!(input_schema["properties"]["name"]["maxLength"], 128);
    assert_eq!(input_schema["properties"]["value"]["maxLength"], 65_536);
    let kind_ref = input_schema["properties"]["kind"]["$ref"]
        .as_str()
        .expect("workflow public input kind reference");
    let kind_definition = kind_ref
        .strip_prefix("#/$defs/")
        .expect("local workflow public input kind definition");
    assert_eq!(
        workflow.input_schema["$defs"][kind_definition]["enum"],
        json!(["text", "url", "file"])
    );
    let candidate_ref = local_schema_ref(&workflow.input_schema["properties"]["candidate"]);
    let candidate_definition = candidate_ref
        .strip_prefix("#/$defs/")
        .expect("local workflow candidate definition");
    let candidate_schema = &workflow.input_schema["$defs"][candidate_definition];
    assert_eq!(candidate_schema["additionalProperties"], false);
    let locator_ref = local_schema_ref(&candidate_schema["properties"]["locator"]);
    let locator_definition = locator_ref
        .strip_prefix("#/$defs/")
        .expect("local workflow locator definition");
    assert_eq!(
        workflow.input_schema["$defs"][locator_definition]["additionalProperties"],
        false
    );
    let schema_text = serde_json::to_string(&workflow.input_schema).unwrap();
    for forbidden in [
        "projectId",
        "conversationId",
        "aiTabId",
        "workspaceKey",
        "route",
        "token",
        "password",
        "secret",
        "fileContent",
        "projectRoot",
        "localProjectRoot",
    ] {
        assert!(
            !schema_text.contains(forbidden),
            "workflow schema exposed forbidden field {forbidden}"
        );
    }

    client.cancel().await.expect("close workflow schema client");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn browser_workflow_malformed_calls_are_typed_and_keep_the_session_alive() {
    let (bridge, mut inbox) = browser_command_channel(8);
    let gateway = BrowserGatewayHandle::start(bridge).expect("start gateway");
    let registration = gateway
        .registrar()
        .register(
            "workflow-malformed-process",
            workspace(
                "workflow-malformed-project",
                "workflow-malformed-conversation",
            ),
            BrowserWorkspaceSnapshot::default(),
        )
        .expect("register workflow malformed token");
    let transport = StreamableHttpClientTransport::from_config(
        StreamableHttpClientTransportConfig::with_uri(gateway.endpoint().to_string())
            .auth_header(registration.access().bearer_token_for_launch()),
    );
    let client = ClientInfo::default()
        .serve(transport)
        .await
        .expect("initialize workflow malformed client");
    let host = async move {
        let mut state = BrowserHostState::new(unique_temp_dir("malformed-host"));
        while let Some(request) = inbox.recv().await {
            let key = request.workspace_key().clone();
            let response = match request.command().clone() {
                BrowserCommand::Ensure { snapshot } => state
                    .ensure_workspace(key, snapshot)
                    .map(|mutation| BrowserResponse::Workspace { mutation }),
                BrowserCommand::SetPaneOpen { open } => state
                    .set_pane_open(&key, open)
                    .map(|mutation| BrowserResponse::Workspace { mutation }),
                BrowserCommand::WorkspaceState => Ok(BrowserResponse::WorkspaceState {
                    snapshot: state.workspace(&key).expect("malformed workspace").clone(),
                }),
                other => panic!("unexpected malformed-workflow host command: {other:?}"),
            };
            request.respond(response);
        }
    };

    let scenario = async move {
        let candidate = json!({
            "revision": 7,
            "locator": {"testId": "replacement", "cssSelectors": ["[data-testid=replacement]"]}
        });
        let malformed = vec![
            json!({"risk":"normal","operation":"list"}),
            json!({"intent":"list workflows","operation":"list"}),
            json!({"intent":"list workflows","risk":"normal"}),
            json!({"intent":"   ","risk":"normal","operation":"list"}),
            json!({"intent":"x".repeat(1025),"risk":"normal","operation":"list"}),
            json!({"intent":"list workflows","risk":"unknown","operation":"list"}),
            json!({"intent":"list workflows","risk":"normal","operation":"unknown"}),
            json!({"intent":"list workflows","risk":"normal","operation":"list","recipeId":"flow"}),
            json!({"intent":"get workflow","risk":"normal","operation":"get"}),
            json!({"intent":"get workflow","risk":"normal","operation":"get","recipeId":""}),
            json!({"intent":"get workflow","risk":"normal","operation":"get","recipeId":"flow","inputs":[]}),
            json!({"intent":"replay workflow","risk":"normal","operation":"replay"}),
            json!({"intent":"replay workflow","risk":"normal","operation":"replay","recipeId":"flow","replayInstanceId":1}),
            json!({"intent":"replay workflow","risk":"normal","operation":"replay","recipeId":"flow","inputs":[{"name":"value","kind":"secret","value":"hidden"}]}),
            json!({"intent":"replay workflow","risk":"normal","operation":"replay","recipeId":"flow","inputs":[{"name":"x".repeat(129),"kind":"text","value":"value"}]}),
            json!({"intent":"inspect replay","risk":"normal","operation":"status"}),
            json!({"intent":"inspect replay","risk":"normal","operation":"status","replayInstanceId":0}),
            json!({"intent":"inspect replay","risk":"normal","operation":"status","replayInstanceId":1,"repairId":1}),
            json!({"intent":"cancel replay","risk":"normal","operation":"cancel","replayInstanceId":1,"confirm":true}),
            json!({"intent":"preview repair","risk":"normal","operation":"repairPreview","replayInstanceId":1,"repairId":1}),
            json!({"intent":"preview repair","risk":"normal","operation":"repairPreview","replayInstanceId":0,"repairId":1,"candidate":candidate.clone()}),
            json!({"intent":"preview repair","risk":"normal","operation":"repairPreview","replayInstanceId":1,"repairId":1,"candidate":{"revision":7,"locator":{"testId":"replacement"},"token":"candidate-token-sentinel"}}),
            json!({"intent":"preview repair","risk":"normal","operation":"repairPreview","replayInstanceId":1,"repairId":1,"candidate":{"revision":7,"locator":{"testId":"replacement","password":"locator-password-sentinel","secret":"locator-secret-sentinel"}}}),
            json!({"intent":"apply repair","risk":"destructive","operation":"repairApply","replayInstanceId":1,"repairId":1,"confirm":false,"resume":true}),
            json!({"intent":"apply repair","risk":"destructive","operation":"repairApply","replayInstanceId":1,"repairId":1,"confirm":true}),
            json!({"intent":"apply repair","risk":"destructive","operation":"repairApply","replayInstanceId":1,"repairId":1,"confirm":true,"resume":true,"candidate":candidate}),
            json!({"intent":"route elsewhere","risk":"normal","operation":"list","projectId":"other"}),
        ];
        for arguments_value in malformed {
            let result = client
                .peer()
                .call_tool(
                    CallToolRequestParams::new("browser_workflow")
                        .with_arguments(arguments(arguments_value)),
                )
                .await
                .expect("malformed workflow call returns a tool result");
            assert_eq!(result.is_error, Some(true));
            assert_eq!(
                result.structured_content.unwrap()["error"]["code"],
                "invalid_request"
            );
            assert_eq!(
                client
                    .peer()
                    .list_tools(None)
                    .await
                    .expect("authenticated MCP session remains usable")
                    .tools
                    .iter()
                    .filter(|tool| tool.name == "browser_workflow")
                    .count(),
                1
            );
        }

        for (sentinel, arguments_value) in [
            (
                "credential-like-invalid-risk-sentinel",
                json!({
                    "intent": "reject an invalid workflow risk",
                    "risk": "credential-like-invalid-risk-sentinel",
                    "operation": "list"
                }),
            ),
            (
                "password-like-invalid-type-sentinel",
                json!({
                    "intent": "reject an invalid workflow identity type",
                    "risk": "normal",
                    "operation": "status",
                    "replayInstanceId": "password-like-invalid-type-sentinel"
                }),
            ),
        ] {
            let result = client
                .peer()
                .call_tool(
                    CallToolRequestParams::new("browser_workflow")
                        .with_arguments(arguments(arguments_value)),
                )
                .await
                .expect("sensitive malformed workflow call returns a tool result");
            assert_eq!(result.is_error, Some(true));
            let body = result.structured_content.expect("typed parse failure");
            assert_eq!(body["error"]["code"], "invalid_request");
            assert_eq!(
                body["error"]["message"],
                "malformed browser_workflow request"
            );
            assert!(!serde_json::to_string(&body).unwrap().contains(sentinel));
            assert_eq!(
                client
                    .peer()
                    .list_tools(None)
                    .await
                    .expect("session survives sensitive parse failure")
                    .tools
                    .iter()
                    .filter(|tool| tool.name == "browser_workflow")
                    .count(),
                1
            );
        }

        client
            .cancel()
            .await
            .expect("close workflow malformed client");
        drop(registration);
        drop(gateway);
    };
    tokio::join!(host, scenario);
}

#[test]
fn workflow_repository_list_and_get_are_sorted_compact_owner_scoped_resources() {
    let project_root = unique_temp_dir("repository");
    std::fs::create_dir_all(&project_root).unwrap();
    let project_root = project_root.canonicalize().unwrap();
    save_recipe(&project_root, &recipe("z-last", "Zulu")).unwrap();
    save_recipe(&project_root, &recipe("a-first", "Alpha")).unwrap();
    let owner = workspace("workflow-resource-project", "conversation-a");
    let foreign = workspace("workflow-resource-project", "conversation-b");
    let resource_root = unique_temp_dir("resources");
    let store = BrowserResourceStore::open(
        &resource_root,
        BrowserResourceLimits {
            max_temporary_count: 8,
            max_temporary_bytes: 1024 * 1024,
            max_resource_bytes: 1024 * 1024,
        },
    )
    .unwrap();

    let listed = list_browser_workflow_recipes(&project_root).unwrap();
    assert_eq!(
        listed
            .iter()
            .map(|recipe| recipe.id.as_str())
            .collect::<Vec<_>>(),
        vec!["a-first", "z-last"]
    );
    assert_eq!(listed[0].step_count, 1);
    assert_eq!(
        listed[0]
            .inputs
            .iter()
            .map(|input| (input.name.as_str(), input.kind))
            .collect::<Vec<_>>(),
        vec![
            ("query", BrowserRecipeInputKind::Text),
            ("destination", BrowserRecipeInputKind::Url),
            ("upload", BrowserRecipeInputKind::File),
        ]
    );
    let metadata = serde_json::to_value(&listed[0]).unwrap();
    assert!(
        serde_json::to_string(&metadata)
            .unwrap()
            .contains("private default omitted from metadata")
            == false
    );

    let got = get_browser_workflow_recipe(&project_root, &owner, &store, "a-first").unwrap();
    assert_eq!(got.recipe.id, "a-first");
    assert_eq!(got.resource.kind, BrowserResourceKind::WorkflowRecipe);
    assert_eq!(got.resource.mime_type, "application/json");
    assert!(!got.resource.pinned);
    let stored = store.read(&owner, &got.resource.id).unwrap();
    assert_eq!(stored.metadata.owner, owner);
    assert_eq!(stored.bytes.last(), Some(&b'\n'));
    let decoded: BrowserRecipeV1 = serde_json::from_slice(&stored.bytes).unwrap();
    assert_eq!(decoded.id, "a-first");
    assert!(store.read(&foreign, &got.resource.id).is_err());

    drop(store);
    std::fs::remove_dir_all(project_root).unwrap();
    std::fs::remove_dir_all(resource_root).unwrap();
}

#[test]
fn workflow_recipe_resources_are_unpinned_and_follow_bounded_oldest_first_cleanup() {
    let project_root = unique_temp_dir("cleanup-repository");
    std::fs::create_dir_all(&project_root).unwrap();
    let project_root = project_root.canonicalize().unwrap();
    save_recipe(&project_root, &recipe("cleanup", "Cleanup")).unwrap();
    let owner = workspace("workflow-cleanup-project", "conversation");
    let resource_root = unique_temp_dir("cleanup-resources");
    let store = BrowserResourceStore::open(
        &resource_root,
        BrowserResourceLimits {
            max_temporary_count: 1,
            max_temporary_bytes: 1024 * 1024,
            max_resource_bytes: 1024 * 1024,
        },
    )
    .unwrap();

    let first = get_browser_workflow_recipe(&project_root, &owner, &store, "cleanup")
        .unwrap()
        .resource;
    let second = get_browser_workflow_recipe(&project_root, &owner, &store, "cleanup")
        .unwrap()
        .resource;
    assert_ne!(first.id, second.id);
    assert!(store.read(&owner, &first.id).is_err());
    assert_eq!(
        store.read(&owner, &second.id).unwrap().metadata.kind,
        BrowserResourceKind::WorkflowRecipe
    );

    drop(store);
    std::fs::remove_dir_all(project_root).unwrap();
    std::fs::remove_dir_all(resource_root).unwrap();
}

#[test]
fn workflow_repository_rejects_unknown_future_recipe_versions() {
    let project_root = unique_temp_dir("future-version");
    let workflow_root = project_root.join(".devmanager").join("browser-workflows");
    std::fs::create_dir_all(&workflow_root).unwrap();
    let project_root = project_root.canonicalize().unwrap();
    let mut future = serde_json::to_value(recipe("future", "Future")).unwrap();
    future["schemaVersion"] = json!(BROWSER_RECIPE_SCHEMA_VERSION + 1);
    std::fs::write(
        workflow_root.join("future.json"),
        serde_json::to_vec_pretty(&future).unwrap(),
    )
    .unwrap();

    assert!(list_browser_workflow_recipes(&project_root).is_err());

    std::fs::remove_dir_all(project_root).unwrap();
}
