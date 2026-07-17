use devmanager::browser::{
    browser_user_input_initialization_script, canonical_browser_page_origin,
    BrowserPageRecordingAuthority, BrowserPageRecordingEnvelope, BrowserPageRecordingEvent,
    BrowserPageRecordingIpc, BrowserPageRecordingIpcError, BrowserRecipeAction,
    BrowserRecipeInputKind, BrowserRecipeValue, BrowserRecordingActor, BrowserRecordingCommit,
    BrowserRevision, BrowserWorkflowRecorder, BrowserWorkspaceKey,
    MAX_BROWSER_PAGE_RECORDING_IPC_BYTES, MAX_BROWSER_PAGE_RECORDING_IPC_DEPTH,
    MAX_BROWSER_PAGE_RECORDING_IPC_STRINGS, MAX_BROWSER_PAGE_RECORDING_LOCATOR_FALLBACKS,
    MAX_BROWSER_PAGE_RECORDING_SELECT_VALUES, MAX_BROWSER_PAGE_RECORDING_STRING_BYTES,
};
use static_assertions::assert_not_impl_any;
use std::process::Command;

fn workspace(project_id: &str, ai_tab_id: &str) -> BrowserWorkspaceKey {
    BrowserWorkspaceKey {
        project_id: project_id.to_string(),
        ai_tab_id: ai_tab_id.to_string(),
    }
}

fn semantic_json(instance_id: u64, sequence: u64, nonce: &str, event: serde_json::Value) -> String {
    serde_json::to_string(&serde_json::json!({
        "version": 1,
        "channel": "browserRecording",
        "workspace": { "projectId": "project-a", "aiTabId": "ai-a" },
        "tabId": "tab-a",
        "revision": 7,
        "instanceId": instance_id,
        "sequence": sequence,
        "actor": "user",
        "source": "page",
        "origin": "https://example.test",
        "event": event,
        "nonce": nonce,
    }))
    .expect("semantic IPC fixture")
}

fn semantic_locator(name: &str) -> serde_json::Value {
    serde_json::json!({
        "accessibilityRole": "textbox",
        "accessibilityName": name,
        "testId": format!("{}-field", name.to_ascii_lowercase()),
        "cssSelectors": [format!("#{}", name.to_ascii_lowercase())],
    })
}

fn click_json(
    project_id: &str,
    ai_tab_id: &str,
    tab_id: &str,
    revision: u64,
    instance_id: u64,
    sequence: u64,
    origin: &str,
    nonce: &str,
) -> String {
    format!(
        r##"{{"version":1,"channel":"browserRecording","workspace":{{"projectId":"{project_id}","aiTabId":"{ai_tab_id}"}},"tabId":"{tab_id}","revision":{revision},"instanceId":{instance_id},"sequence":{sequence},"actor":"user","source":"page","origin":"{origin}","event":{{"type":"click","locator":{{"accessibilityRole":"button","accessibilityName":"Save","testId":"save","cssSelectors":["#save"]}}}},"nonce":"{nonce}"}}"##,
    )
}

#[test]
fn strict_page_recording_ipc_accepts_only_the_exact_active_view_context() {
    let workspace = workspace("project-a", "ai-a");
    let mut recorder = BrowserWorkflowRecorder::default();
    let instance = recorder.start(workspace.clone()).expect("start recorder");
    let nonce = "6c9bca4bd7eb4f65a1865966cedc9f78";
    let authority = BrowserPageRecordingAuthority::new(
        instance.clone(),
        "tab-a",
        BrowserRevision(7),
        "https://example.test",
        nonce,
    )
    .expect("valid active view context");
    let exact = click_json(
        "project-a",
        "ai-a",
        "tab-a",
        7,
        instance.id(),
        0,
        "https://example.test",
        nonce,
    );
    let parsed = BrowserPageRecordingEnvelope::parse(&exact).expect("strict exact envelope");
    assert_eq!(
        serde_json::to_string(&parsed).expect("deterministic envelope JSON"),
        exact,
        "the strict serde envelope has one exact wire shape"
    );
    assert!(matches!(
        parsed.event(),
        BrowserPageRecordingEvent::Click { locator } if locator.test_id.as_deref() == Some("save")
    ));

    let mut ipc = BrowserPageRecordingIpc::default();
    assert_eq!(
        ipc.ingest(&mut recorder, &exact),
        Err(BrowserPageRecordingIpcError::Inactive),
        "recording IPC is off by default"
    );
    ipc.activate(authority)
        .expect("activate exact recording view");
    assert_eq!(
        ipc.ingest(&mut recorder, &exact),
        Ok(BrowserRecordingCommit::Recorded)
    );
    assert_eq!(
        ipc.ingest(&mut recorder, &exact),
        Err(BrowserPageRecordingIpcError::Replay),
        "an exact replay is suppressed"
    );

    let hostile_variants = [
        exact.replace(nonce, "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
        exact.replace("project-a", "project-b"),
        exact.replace("ai-a", "ai-b"),
        exact.replace("tab-a", "tab-b"),
        exact.replace("\"revision\":7", "\"revision\":8"),
        exact.replace(
            &format!("\"instanceId\":{}", instance.id()),
            &format!("\"instanceId\":{}", instance.id() + 1),
        ),
        exact.replace("https://example.test", "https://evil.test"),
        exact.replace("\"actor\":\"user\"", "\"actor\":\"agent\""),
        exact.replace("\"source\":\"page\"", "\"source\":\"chrome\""),
    ];
    for hostile in hostile_variants {
        assert_eq!(
            ipc.ingest(&mut recorder, &hostile),
            Err(BrowserPageRecordingIpcError::Untrusted),
            "cross-context IPC must fail closed"
        );
    }
    assert_eq!(
        recorder.active_step_count(&instance),
        Ok(1),
        "only the trusted unique message reaches the recorder"
    );

    ipc.deactivate();
    assert_eq!(
        ipc.ingest(&mut recorder, &exact),
        Err(BrowserPageRecordingIpcError::Inactive)
    );
}

#[test]
fn page_recording_ipc_rejects_malformed_unknown_duplicate_oversized_and_deep_json() {
    let workspace = workspace("project-a", "ai-a");
    let mut recorder = BrowserWorkflowRecorder::default();
    let instance = recorder.start(workspace).expect("start recorder");
    let nonce = "6c9bca4bd7eb4f65a1865966cedc9f78";
    let mut ipc = BrowserPageRecordingIpc::default();
    ipc.activate(
        BrowserPageRecordingAuthority::new(
            instance.clone(),
            "tab-a",
            BrowserRevision(7),
            "https://example.test",
            nonce,
        )
        .expect("context"),
    )
    .expect("activate");

    let exact = click_json(
        "project-a",
        "ai-a",
        "tab-a",
        7,
        instance.id(),
        0,
        "https://example.test",
        nonce,
    );
    let malformed = "{not-json".to_string();
    let unknown = exact.replacen("\"version\":1", "\"version\":1,\"extra\":true", 1);
    let duplicate = exact.replacen("\"version\":1", "\"version\":1,\"version\":1", 1);
    let oversized = "x".repeat(MAX_BROWSER_PAGE_RECORDING_IPC_BYTES + 1);
    let deep = format!(
        "{}0{}",
        "[".repeat(MAX_BROWSER_PAGE_RECORDING_IPC_DEPTH + 1),
        "]".repeat(MAX_BROWSER_PAGE_RECORDING_IPC_DEPTH + 1)
    );

    for body in [malformed, unknown, duplicate, oversized, deep] {
        assert!(
            matches!(
                ipc.ingest(&mut recorder, &body),
                Err(BrowserPageRecordingIpcError::Malformed)
                    | Err(BrowserPageRecordingIpcError::Oversized)
                    | Err(BrowserPageRecordingIpcError::TooDeep)
            ),
            "hostile IPC must be rejected without a panic"
        );
    }
    assert_eq!(recorder.active_step_count(&instance), Ok(0));

    let reservation = recorder
        .reserve(&instance, BrowserRecordingActor::User)
        .expect("malformed IPC did not consume recorder capacity");
    assert_eq!(
        recorder.cancel(reservation),
        Ok(BrowserRecordingCommit::Recorded)
    );
}

#[test]
fn semantic_events_coalesce_and_never_retain_password_file_clipboard_or_url_secrets() {
    let mut recorder = BrowserWorkflowRecorder::default();
    let instance = recorder
        .start(workspace("project-a", "ai-a"))
        .expect("start recorder");
    let nonce = "6c9bca4bd7eb4f65a1865966cedc9f78";
    let mut ipc = BrowserPageRecordingIpc::default();
    ipc.activate(
        BrowserPageRecordingAuthority::new(
            instance.clone(),
            "tab-a",
            BrowserRevision(7),
            "https://example.test",
            nonce,
        )
        .expect("authority"),
    )
    .expect("activate");

    let events = [
        serde_json::json!({
            "type": "textEdit",
            "locator": semantic_locator("Query"),
            "edit": { "type": "text", "text": "hello" },
        }),
        serde_json::json!({
            "type": "textEdit",
            "locator": semantic_locator("Query"),
            "edit": { "type": "text", "text": "hello world" },
        }),
        serde_json::json!({
            "type": "textEdit",
            "locator": semantic_locator("Password"),
            "edit": { "type": "password" },
        }),
        serde_json::json!({
            "type": "textEdit",
            "locator": semantic_locator("Notes"),
            "edit": { "type": "clipboard" },
        }),
        serde_json::json!({
            "type": "textEdit",
            "locator": semantic_locator("Token"),
            "edit": { "type": "text", "text": "Bearer token-text-sentinel" },
        }),
        serde_json::json!({
            "type": "select",
            "locator": {
                "accessibilityRole": "combobox",
                "accessibilityName": "Plan",
                "testId": "plan",
                "cssSelectors": ["#plan"],
            },
            "values": ["pro"],
        }),
        serde_json::json!({
            "type": "navigation",
            "url": "https://example.test/results?token=url-token-sentinel&query=safe",
        }),
        serde_json::json!({
            "type": "upload",
            "locator": semantic_locator("Upload"),
        }),
        serde_json::json!({
            "type": "download",
            "locator": {
                "accessibilityRole": "link",
                "accessibilityName": "Download report",
                "testId": "download-report",
                "cssSelectors": ["#download-report"],
            },
        }),
        serde_json::json!({
            "type": "click",
            "locator": {
                "accessibilityRole": "button",
                "accessibilityName": "Save",
                "testId": "save",
                "cssSelectors": ["#save"],
            },
        }),
    ];
    for (sequence, event) in events.into_iter().enumerate() {
        assert!(matches!(
            ipc.ingest(
                &mut recorder,
                &semantic_json(instance.id(), sequence as u64, nonce, event)
            ),
            Ok(BrowserRecordingCommit::Recorded) | Ok(BrowserRecordingCommit::Buffered)
        ));
    }

    let hostile_markers = [
        serde_json::json!({
            "type": "textEdit",
            "locator": semantic_locator("Password"),
            "edit": { "type": "password", "text": "password-value-sentinel" },
        }),
        serde_json::json!({
            "type": "textEdit",
            "locator": semantic_locator("Notes"),
            "edit": { "type": "clipboard", "text": "clipboard-value-sentinel" },
        }),
        serde_json::json!({
            "type": "upload",
            "locator": semantic_locator("Upload"),
            "path": "C:/private/file-path-sentinel.txt",
            "contents": "file-contents-sentinel",
        }),
        serde_json::json!({
            "type": "download",
            "locator": semantic_locator("Download"),
            "path": "C:/private/download-path-sentinel.txt",
        }),
    ];
    for (offset, event) in hostile_markers.into_iter().enumerate() {
        assert_eq!(
            ipc.ingest(
                &mut recorder,
                &semantic_json(instance.id(), 10 + offset as u64, nonce, event),
            ),
            Err(BrowserPageRecordingIpcError::Malformed),
            "hostile marker {offset} crossed the strict wire"
        );
    }

    let review = recorder.stop(&instance).expect("stop recording");
    assert_eq!(review.recipe().steps.len(), 9, "typing coalesces once");
    assert_eq!(
        review
            .recipe()
            .inputs
            .iter()
            .map(|input| input.kind)
            .collect::<Vec<_>>(),
        [
            BrowserRecipeInputKind::Secret,
            BrowserRecipeInputKind::Secret,
            BrowserRecipeInputKind::Secret,
            BrowserRecipeInputKind::File,
        ]
    );
    assert!(matches!(
        &review.recipe().steps[0].action,
        BrowserRecipeAction::Type {
            value: BrowserRecipeValue::Literal { value }, ..
        } if value == "hello world"
    ));
    assert!(matches!(
        &review.recipe().steps[4].action,
        BrowserRecipeAction::Select { values, .. }
            if values == &[BrowserRecipeValue::Literal { value: "pro".to_string() }]
    ));
    assert!(matches!(
        &review.recipe().steps[5].action,
        BrowserRecipeAction::Navigate {
            url: BrowserRecipeValue::Literal { value }
        } if value == "https://example.test/results?query=safe"
    ));
    assert!(matches!(
        &review.recipe().steps[6].action,
        BrowserRecipeAction::Upload { .. }
    ));
    assert!(matches!(
        &review.recipe().steps[7].action,
        BrowserRecipeAction::Download { .. }
    ));
    assert!(matches!(
        &review.recipe().steps[8].action,
        BrowserRecipeAction::Click { .. }
    ));

    let retained = serde_json::to_string(review.recipe()).expect("safe recipe JSON");
    for forbidden in [
        "password-value-sentinel",
        "clipboard-value-sentinel",
        "file-path-sentinel",
        "file-contents-sentinel",
        "download-path-sentinel",
        "token-text-sentinel",
        "url-token-sentinel",
    ] {
        assert!(
            !retained.contains(forbidden),
            "retained secret: {forbidden}"
        );
    }
}

#[test]
fn crafted_bare_tokens_in_text_select_or_locator_metadata_never_reach_recording_state() {
    let mut recorder = BrowserWorkflowRecorder::default();
    let instance = recorder
        .start(workspace("project-a", "ai-a"))
        .expect("start recorder");
    let nonce = "6c9bca4bd7eb4f65a1865966cedc9f78";
    let mut ipc = BrowserPageRecordingIpc::default();
    ipc.activate(
        BrowserPageRecordingAuthority::new(
            instance.clone(),
            "tab-a",
            BrowserRevision(7),
            "https://example.test",
            nonce,
        )
        .expect("authority"),
    )
    .expect("activate");
    let jwt = "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiJzZWNyZXQtdXNlciJ9.dGhpc2lzYXNpZ25hdHVyZQ";
    let api_key = "sk-proj-abcdefghijklmnopqrstuvwxyz0123456789ABCDEFG";
    let github = "ghp_abcdefghijklmnopqrstuvwxyz0123456789";
    let aws = "AKIAIOSFODNN7EXAMPLE";
    let hostile_events = [
        serde_json::json!({
            "type": "textEdit",
            "locator": semantic_locator("Query"),
            "edit": { "type": "text", "text": jwt },
        }),
        serde_json::json!({
            "type": "textEdit",
            "locator": semantic_locator("Query"),
            "edit": { "type": "text", "text": api_key },
        }),
        serde_json::json!({
            "type": "select",
            "locator": semantic_locator("Plan"),
            "values": [github, aws],
        }),
        serde_json::json!({
            "type": "click",
            "locator": {
                "accessibilityRole": "button",
                "accessibilityName": jwt,
                "testId": "safe-button",
                "cssSelectors": ["#safe-button"],
            },
        }),
    ];
    for event in hostile_events {
        assert_eq!(
            ipc.ingest(
                &mut recorder,
                &semantic_json(instance.id(), 0, nonce, event)
            ),
            Err(BrowserPageRecordingIpcError::Malformed)
        );
    }
    assert_eq!(recorder.active_step_count(&instance), Ok(0));
    assert_eq!(
        ipc.ingest(
            &mut recorder,
            &click_json(
                "project-a",
                "ai-a",
                "tab-a",
                7,
                instance.id(),
                0,
                "https://example.test",
                nonce,
            ),
        ),
        Ok(BrowserRecordingCommit::Recorded)
    );
    let review = recorder
        .stop(&instance)
        .expect("stop clean rejected capture");
    let recipe_json = serde_json::to_string(review.recipe()).expect("safe empty recipe JSON");
    for forbidden in [jwt, api_key, github, aws] {
        assert!(!recipe_json.contains(forbidden));
    }
}

#[test]
fn recording_script_exists_only_for_the_exact_active_authority_and_has_a_safe_teardown() {
    assert!(
        !browser_user_input_initialization_script().contains("browserRecording"),
        "the always-on page adapter must not contain recording instrumentation"
    );

    let mut recorder = BrowserWorkflowRecorder::default();
    let instance = recorder
        .start(workspace("project-a", "ai-a"))
        .expect("start recorder");
    let nonce = "6c9bca4bd7eb4f65a1865966cedc9f78";
    let mut ipc = BrowserPageRecordingIpc::default();
    assert_eq!(
        ipc.activation_script(),
        Err(BrowserPageRecordingIpcError::Inactive)
    );
    ipc.activate(
        BrowserPageRecordingAuthority::new(
            instance,
            "tab-a",
            BrowserRevision(7),
            "https://example.test",
            nonce,
        )
        .expect("authority"),
    )
    .expect("activate");

    let install = ipc.activation_script().expect("active-only install script");
    let remove = ipc.deactivation_script().expect("exact teardown script");
    for required in [
        "__devmanagerBrowserRecording",
        "event.isTrusted",
        "insertFromPaste",
        "type === \"password\"",
        "type === \"file\"",
        "browserRecording",
        nonce,
        "removeEventListener",
    ] {
        assert!(
            install.contains(required),
            "missing lifecycle guard: {required}"
        );
    }
    for forbidden in [
        "clipboardData",
        ".files",
        "document.cookie",
        "localStorage",
        "sessionStorage",
        "outerHTML",
        "innerHTML",
        "getComputedStyle",
    ] {
        assert!(
            !install.contains(forbidden),
            "recording script reads forbidden data: {forbidden}"
        );
    }
    assert!(remove.contains("__devmanagerBrowserRecording"));
    assert!(remove.contains(nonce));
    assert!(!remove.contains("postMessage"));

    ipc.deactivate();
    assert_eq!(
        ipc.activation_script(),
        Err(BrowserPageRecordingIpcError::Inactive)
    );
    assert_eq!(
        ipc.deactivation_script(),
        Err(BrowserPageRecordingIpcError::Inactive)
    );
}

#[test]
fn recording_script_runtime_emits_safe_markers_without_reading_sensitive_page_values() {
    let mut recorder = BrowserWorkflowRecorder::default();
    let instance = recorder
        .start(workspace("project-a", "ai-a"))
        .expect("start recorder");
    let nonce = "6c9bca4bd7eb4f65a1865966cedc9f78";
    let mut ipc = BrowserPageRecordingIpc::default();
    ipc.activate(
        BrowserPageRecordingAuthority::new(
            instance,
            "tab-a",
            BrowserRevision(7),
            "https://example.test",
            nonce,
        )
        .expect("authority"),
    )
    .expect("activate");
    let install = serde_json::to_string(&ipc.activation_script().expect("install"))
        .expect("encode install script");
    let remove = serde_json::to_string(&ipc.deactivation_script().expect("remove"))
        .expect("encode remove script");
    let harness = format!(
        r#"
const messages = [];
const listeners = new Map();
const listenerTarget = (prefix) => ({{
  addEventListener(name, handler) {{ listeners.set(`${{prefix}}:${{name}}`, handler); }},
  removeEventListener(name, handler) {{
    if (listeners.get(`${{prefix}}:${{name}}`) === handler) listeners.delete(`${{prefix}}:${{name}}`);
  }},
}});
globalThis.location = {{ origin: "https://example.test", href: "https://example.test/start" }};
globalThis.document = listenerTarget("document");
globalThis.window = {{
  ...listenerTarget("window"),
  ipc: {{ postMessage: (body) => messages.push(JSON.parse(body)) }},
  CSS: {{ escape: (value) => String(value) }},
}};
class FakeElement {{
  constructor(type, value, options = {{}}) {{
    this.tagName = options.tagName || "INPUT";
    this.id = options.id || type;
    this.innerText = options.innerText || "";
    this.labels = options.label ? [{{ innerText: options.label }}] : [];
    this.parentElement = null;
    this.options = options.options || [];
    this.ariaLabel = options.ariaLabel || null;
    this.download = Boolean(options.download);
    this._type = type;
    this._value = value;
    this._throwValue = Boolean(options.throwValue);
  }}
  get value() {{
    if (this._throwValue) throw new Error(`forbidden value read: ${{this._type}}`);
    return this._value;
  }}
  get files() {{ throw new Error("forbidden file list read"); }}
  getAttribute(name) {{
    if (name === "type") return this._type;
    if (name === "data-testid") return this.id;
    if (name === "aria-label") return this.ariaLabel;
    if (name === "href") return this.tagName === "A" ? "/download" : null;
    return null;
  }}
  hasAttribute(name) {{ return name === "href" && this.tagName === "A"; }}
  closest(selector) {{
    if (selector === "a[download]") return this.download ? this : null;
    return null;
  }}
}}
globalThis.Element = FakeElement;

eval({install});
const input = listeners.get("document:input");
const click = listeners.get("document:click");
if (!input || !click) throw new Error("recording listeners were not installed");
input({{ isTrusted: true, target: new FakeElement("password", "password-value-sentinel", {{ label: "Password", throwValue: true }}), inputType: "insertText" }});
input({{ isTrusted: true, target: new FakeElement("file", "C:/private/file-path-sentinel", {{ label: "Upload", throwValue: true }}), inputType: "insertText" }});
input({{ isTrusted: true, target: new FakeElement("text", "clipboard-value-sentinel", {{ label: "Notes", throwValue: true }}), inputType: "insertFromPaste" }});
input({{ isTrusted: true, target: new FakeElement("text", "Bearer token-value-sentinel", {{ label: "Token" }}), inputType: "insertText" }});
input({{ isTrusted: true, target: new FakeElement("text", "ordinary value", {{ label: "Query" }}), inputType: "insertText" }});
const jwt = "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiJzZWNyZXQtdXNlciJ9.dGhpc2lzYXNpZ25hdHVyZQ";
const apiKey = "sk-proj-abcdefghijklmnopqrstuvwxyz0123456789ABCDEFG";
const github = "ghp_abcdefghijklmnopqrstuvwxyz0123456789";
const aws = "AKIAIOSFODNN7EXAMPLE";
for (const token of [jwt, apiKey, github, aws]) {{
  input({{ isTrusted: true, target: new FakeElement("text", token, {{ label: "Token" }}), inputType: "insertText" }});
}}
input({{ isTrusted: true, target: new FakeElement("select", "", {{ tagName: "SELECT", label: "Plan", options: [{{ selected: true, value: github }}] }}), inputType: "insertText" }});
input({{ isTrusted: true, target: new FakeElement("text", "aria safe value", {{ ariaLabel: jwt, id: "safe-aria" }}), inputType: "insertText" }});
input({{ isTrusted: true, target: new FakeElement("text", "label safe value", {{ label: apiKey, id: "safe-label" }}), inputType: "insertText" }});
click({{ isTrusted: true, target: new FakeElement("", "", {{ tagName: "A", id: "download", innerText: "Download", download: true }}) }});
input({{ isTrusted: false, target: new FakeElement("text", "untrusted-value-sentinel", {{ label: "Query" }}), inputType: "insertText" }});

if (messages.length !== 13) throw new Error(`expected 13 messages, got ${{messages.length}}`);
if (messages.some((message, index) => message.sequence !== index)) throw new Error("source sequence drifted");
if (messages[0].event.edit.type !== "password" || Object.hasOwn(messages[0].event.edit, "text")) throw new Error("password value crossed IPC");
if (messages[1].event.type !== "upload" || Object.hasOwn(messages[1].event, "path")) throw new Error("file data crossed IPC");
if (messages[2].event.edit.type !== "clipboard" || Object.hasOwn(messages[2].event.edit, "text")) throw new Error("clipboard data crossed IPC");
if (messages[3].event.edit.type !== "password") throw new Error("credential-like text crossed IPC");
if (messages[4].event.edit.text !== "ordinary value") throw new Error("ordinary text was not recorded");
for (const index of [5, 6, 7, 8, 9]) {{
  if (messages[index].event.edit?.type !== "password") throw new Error(`secret marker missing at ${{index}}`);
}}
if (messages[10].event.edit?.text !== "aria safe value") throw new Error("safe aria fallback value missing");
if (messages[11].event.edit?.text !== "label safe value") throw new Error("safe label fallback value missing");
if (messages[12].event.type !== "download") throw new Error("download marker missing");
const wire = JSON.stringify(messages);
for (const forbidden of ["password-value-sentinel", "file-path-sentinel", "clipboard-value-sentinel", "token-value-sentinel", "untrusted-value-sentinel", jwt, apiKey, github, aws]) {{
  if (wire.includes(forbidden)) throw new Error(`leaked ${{forbidden}}`);
}}

eval({remove});
if ([...listeners.keys()].some((key) => key === "document:input" || key === "document:click" || key === "window:popstate" || key === "window:hashchange")) {{
  throw new Error("recording listeners survived teardown");
}}
"#
    );
    let output = Command::new("node")
        .args(["--eval", &harness])
        .output()
        .expect("run Node recording lifecycle harness");
    assert!(
        output.status.success(),
        "Node harness failed: {}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn windows_host_uses_a_private_active_only_recording_channel_and_fences_view_lifecycle() {
    let windows = include_str!("../src/browser/host/windows.rs");
    let recording_ipc = include_str!("../src/browser/recording_ipc.rs");
    let unsupported = include_str!("../src/browser/host/unsupported.rs");
    assert!(recording_ipc.contains("pub(crate) struct BrowserPageRecordingRawMessage"));
    for required in [
        "recording_transport: BrowserPageRecordingTransport",
        "recording_ingresses: HashMap<BrowserViewKey, BrowserPageRecordingIngress>",
        "pub fn start_page_recording(",
        "pub fn stop_page_recording(",
        "fn install_page_recording_view(",
        "fn remove_page_recording_view(",
        "fn discard_page_recording(",
        "fn discard_project_page_recordings(",
        "self.discard_page_recording(workspace_key);",
        "self.discard_project_page_recordings(&workspace_key.project_id);",
        "activation_script()",
        "deactivation_script()",
        "ingest_from_origin(",
        "request.uri()",
    ] {
        assert!(
            windows.contains(required),
            "missing Windows host seam: {required}"
        );
    }
    assert!(
        windows.find("self.pump_page_recording_ipc()")
            < windows.find("self.event_receiver.try_iter()"),
        "semantic reservations must be made before generic input revision events drain"
    );
    assert!(
        !windows.contains("BrowserHostEvent::PageRecording"),
        "typed text must not enter the serializable/debuggable host event surface"
    );
    for required in [
        "pub fn start_page_recording(",
        "pub fn stop_page_recording(",
        "pub fn page_recording_status(",
    ] {
        assert!(
            unsupported.contains(required),
            "unsupported platforms need a compile-safe adapter: {required}"
        );
    }
}

#[test]
fn page_recording_ipc_enforces_count_and_decoded_string_bounds_before_retention() {
    let mut recorder = BrowserWorkflowRecorder::default();
    let instance = recorder
        .start(workspace("project-a", "ai-a"))
        .expect("start recorder");
    let nonce = "6c9bca4bd7eb4f65a1865966cedc9f78";
    let mut ipc = BrowserPageRecordingIpc::default();
    ipc.activate(
        BrowserPageRecordingAuthority::new(
            instance.clone(),
            "tab-a",
            BrowserRevision(7),
            "https://example.test",
            nonce,
        )
        .expect("authority"),
    )
    .expect("activate");

    let too_many_values = (0..=MAX_BROWSER_PAGE_RECORDING_SELECT_VALUES)
        .map(|index| format!("value-{index}"))
        .collect::<Vec<_>>();
    let too_many_select = semantic_json(
        instance.id(),
        0,
        nonce,
        serde_json::json!({
            "type": "select",
            "locator": {
                "accessibilityRole": "combobox",
                "accessibilityName": "Plan",
                "testId": "plan",
                "cssSelectors": ["#plan"],
            },
            "values": too_many_values,
        }),
    );
    assert!(matches!(
        ipc.ingest(&mut recorder, &too_many_select),
        Err(BrowserPageRecordingIpcError::TooManyItems)
            | Err(BrowserPageRecordingIpcError::Malformed)
    ));

    let too_many_fallbacks = (0..=MAX_BROWSER_PAGE_RECORDING_LOCATOR_FALLBACKS)
        .map(|index| format!("#fallback-{index}"))
        .collect::<Vec<_>>();
    let too_many_locator_fallbacks = semantic_json(
        instance.id(),
        0,
        nonce,
        serde_json::json!({
            "type": "click",
            "locator": {
                "accessibilityRole": "button",
                "accessibilityName": "Save",
                "testId": "save",
                "cssSelectors": too_many_fallbacks,
            },
        }),
    );
    assert!(matches!(
        ipc.ingest(&mut recorder, &too_many_locator_fallbacks),
        Err(BrowserPageRecordingIpcError::TooManyItems)
            | Err(BrowserPageRecordingIpcError::Malformed)
    ));

    let oversized_text = semantic_json(
        instance.id(),
        0,
        nonce,
        serde_json::json!({
            "type": "textEdit",
            "locator": semantic_locator("Query"),
            "edit": {
                "type": "text",
                "text": "x".repeat(MAX_BROWSER_PAGE_RECORDING_STRING_BYTES + 1),
            },
        }),
    );
    assert_eq!(
        ipc.ingest(&mut recorder, &oversized_text),
        Err(BrowserPageRecordingIpcError::Oversized)
    );

    let excessive_strings = format!(
        "[{}]",
        (0..=MAX_BROWSER_PAGE_RECORDING_IPC_STRINGS)
            .map(|index| format!("\"{index}\""))
            .collect::<Vec<_>>()
            .join(",")
    );
    assert_eq!(
        BrowserPageRecordingEnvelope::parse(&excessive_strings),
        Err(BrowserPageRecordingIpcError::TooManyItems)
    );

    let valid_values = (0..MAX_BROWSER_PAGE_RECORDING_SELECT_VALUES)
        .map(|index| format!("value-{index}"))
        .collect::<Vec<_>>();
    assert_eq!(
        ipc.ingest(
            &mut recorder,
            &semantic_json(
                instance.id(),
                0,
                nonce,
                serde_json::json!({
                    "type": "select",
                    "locator": {
                        "accessibilityRole": "combobox",
                        "accessibilityName": "Plan",
                        "testId": "plan",
                        "cssSelectors": ["#plan"],
                    },
                    "values": valid_values,
                }),
            ),
        ),
        Ok(BrowserRecordingCommit::Recorded)
    );
    assert_eq!(recorder.active_step_count(&instance), Ok(1));
}

#[test]
fn windows_recording_transport_is_bounded_before_raw_page_messages_can_queue() {
    let windows = include_str!("../src/browser/host/windows.rs");
    assert!(windows.contains("BrowserPageRecordingTransport"));
    assert!(windows.contains("MAX_BROWSER_PAGE_RECORDING_QUEUE"));
    assert!(windows.contains("BrowserPageRecordingSubmit::Overflow"));
    assert!(windows.contains("BrowserPageRecordingSubmit::Disconnected"));
    assert!(!windows.contains("let _ = ipc_recording_sender.try_send("));
}

#[test]
fn windows_stop_fences_transport_then_drains_before_retiring_the_exact_recorder() {
    let windows = include_str!("../src/browser/host/windows.rs");
    let stop_start = windows
        .find("pub fn stop_page_recording(")
        .expect("stop seam");
    let stop_end = windows[stop_start..]
        .find("\n    pub fn handle_command(")
        .map(|offset| stop_start + offset)
        .expect("stop seam end");
    let stop = &windows[stop_start..stop_end];
    let fence = stop
        .find("fence_workspace_recording_views")
        .expect("synchronous transport fence");
    let drain = stop
        .find("pump_page_recording_ipc")
        .expect("accepted-message drain");
    let retire = stop
        .find("recording_instances.remove")
        .expect("authority retirement");
    let recorder_stop = stop.find("workflow_recorder").expect("recorder Stop");
    assert!(fence < drain && drain < retire && retire < recorder_stop);
    assert!(windows.contains("self.pump_page_recording_ipc();\n        let instance"));
    assert!(windows.contains("invalidate_page_recording_transport"));
    let invalidation_start = windows
        .find("fn invalidate_page_recording_transport(")
        .expect("typed invalidation helper");
    let invalidation_end = windows[invalidation_start..]
        .find("\n    fn install_page_recording_view(")
        .map(|offset| invalidation_start + offset)
        .expect("typed invalidation helper end");
    let invalidation = &windows[invalidation_start..invalidation_end];
    assert!(invalidation.contains("self.discard_page_recording(workspace_key);"));
    assert!(invalidation.contains("the incomplete recording was discarded"));
    assert!(stop.contains("BrowserPageRecordingIpcError::TransportInvalidated"));
}

#[test]
fn late_page_ipc_old_instances_and_transport_origin_replays_never_reach_a_new_recording() {
    assert_not_impl_any!(BrowserPageRecordingAuthority: std::fmt::Debug, serde::Serialize);
    assert_not_impl_any!(BrowserPageRecordingIpc: std::fmt::Debug, serde::Serialize);

    let workspace = workspace("project-a", "ai-a");
    let nonce_one = "11111111111111111111111111111111";
    let nonce_two = "22222222222222222222222222222222";
    let mut recorder = BrowserWorkflowRecorder::default();
    let first = recorder.start(workspace.clone()).expect("first instance");
    let mut old_ipc = BrowserPageRecordingIpc::default();
    old_ipc
        .activate(
            BrowserPageRecordingAuthority::new(
                first.clone(),
                "tab-a",
                BrowserRevision(7),
                "https://example.test",
                nonce_one,
            )
            .expect("old authority"),
        )
        .expect("activate old authority");
    let first_body = click_json(
        "project-a",
        "ai-a",
        "tab-a",
        7,
        first.id(),
        0,
        "https://example.test",
        nonce_one,
    );
    assert_eq!(
        old_ipc.ingest_from_origin(&mut recorder, "https://example.test", &first_body),
        Ok(BrowserRecordingCommit::Recorded)
    );
    let first_review = recorder.stop(&first).expect("stop first instance");
    let first_json = serde_json::to_string(first_review.recipe()).expect("first review JSON");
    let late_body = first_body.replace("\"sequence\":0", "\"sequence\":1");
    assert_eq!(
        old_ipc.ingest_from_origin(&mut recorder, "https://example.test", &late_body),
        Err(BrowserPageRecordingIpcError::Untrusted)
    );
    assert_eq!(
        serde_json::to_string(
            recorder
                .review(&first)
                .expect("late IPC leaves review intact")
                .recipe()
        )
        .expect("review JSON after late IPC"),
        first_json
    );
    recorder.discard(&first).expect("discard first review");

    let second = recorder.start(workspace).expect("second instance");
    assert_ne!(first.id(), second.id());
    assert_eq!(
        old_ipc.ingest_from_origin(&mut recorder, "https://example.test", &late_body),
        Err(BrowserPageRecordingIpcError::Untrusted),
        "an old exact authority cannot reserve into the replacement instance"
    );

    let mut new_ipc = BrowserPageRecordingIpc::default();
    new_ipc
        .activate(
            BrowserPageRecordingAuthority::new(
                second.clone(),
                "tab-a",
                BrowserRevision(8),
                "https://example.test",
                nonce_two,
            )
            .expect("new authority"),
        )
        .expect("activate new authority");
    let second_body = click_json(
        "project-a",
        "ai-a",
        "tab-a",
        8,
        second.id(),
        0,
        "https://example.test",
        nonce_two,
    );
    assert_eq!(
        new_ipc.ingest_from_origin(&mut recorder, "https://evil.test", &second_body),
        Err(BrowserPageRecordingIpcError::Untrusted),
        "the trusted Wry request URI outranks the body origin"
    );
    assert_eq!(recorder.active_step_count(&second), Ok(0));
    assert_eq!(
        new_ipc.ingest_from_origin(&mut recorder, "https://example.test", &second_body),
        Ok(BrowserRecordingCommit::Recorded)
    );
    assert_eq!(recorder.active_step_count(&second), Ok(1));
}

#[test]
fn canonical_http_origins_equate_real_url_forms_and_reject_spoofs() {
    for (input, expected) in [
        ("HTTPS://Example.COM:443", "https://example.com"),
        ("http://Example.COM:80/", "http://example.com"),
        ("https://Example.COM:8443", "https://example.com:8443"),
        ("https://[2001:DB8::1]:443", "https://[2001:db8::1]"),
        ("https://bücher.example", "https://xn--bcher-kva.example"),
    ] {
        assert_eq!(
            canonical_browser_page_origin(input),
            Ok(expected.to_string()),
            "canonical origin for {input}"
        );
    }
    for spoof in [
        "https://user:password@example.com",
        "https://example.com@evil.test",
        "https://example.com.evil.test/path",
        "https://example.com?next=https://evil.test",
        "https://example.com#https://evil.test",
        "javascript://example.com",
        "file://example.com",
        "https://[2001:db8::1",
        "https://",
    ] {
        assert_eq!(
            canonical_browser_page_origin(spoof),
            Err(BrowserPageRecordingIpcError::InvalidAuthority),
            "reject spoofed or malformed origin {spoof}"
        );
    }

    let mut recorder = BrowserWorkflowRecorder::default();
    let instance = recorder
        .start(workspace("project-a", "ai-a"))
        .expect("start recorder");
    let nonce = "6c9bca4bd7eb4f65a1865966cedc9f78";
    let mut ipc = BrowserPageRecordingIpc::default();
    ipc.activate(
        BrowserPageRecordingAuthority::new(
            instance.clone(),
            "tab-a",
            BrowserRevision(7),
            "HTTPS://EXAMPLE.TEST:443",
            nonce,
        )
        .expect("canonical authority"),
    )
    .expect("activate");
    assert_eq!(
        ipc.ingest_from_origin(
            &mut recorder,
            "https://Example.Test:443",
            &click_json(
                "project-a",
                "ai-a",
                "tab-a",
                7,
                instance.id(),
                0,
                "https://example.test",
                nonce,
            ),
        ),
        Ok(BrowserRecordingCommit::Recorded)
    );
    assert_eq!(
        ipc.ingest_from_origin(
            &mut recorder,
            "https://example.test.evil",
            &click_json(
                "project-a",
                "ai-a",
                "tab-a",
                7,
                instance.id(),
                1,
                "https://example.test",
                nonce,
            ),
        ),
        Err(BrowserPageRecordingIpcError::Untrusted)
    );
}
