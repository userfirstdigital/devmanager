use devmanager::browser::{
    browser_command_channel, browser_user_input_initialization_script,
    parse_browser_annotation_ipc_message, BrowserActionTarget, BrowserCommand, BrowserError,
    BrowserInvocationContext, BrowserPageRecordingEnvelope, BrowserReplayCoordinator,
    BrowserReplayExecutionHandle, BrowserReplayInstance, BrowserReplayProjection,
    BrowserReplaySecretError, BrowserReplaySecretLease, BrowserReplaySecretStore,
    BrowserReplaySecretSubmission, BrowserReplayStatus, BrowserRisk, BrowserWorkspaceKey,
};
use static_assertions::{assert_impl_all, assert_not_impl_any};
use std::process::Command;
use std::time::Duration;

assert_impl_all!(BrowserReplaySecretError: Copy, Send, Sync, std::fmt::Debug);
assert_impl_all!(BrowserReplaySecretStore: Send, Sync);
assert_impl_all!(BrowserReplaySecretLease: Send, Sync);
assert_impl_all!(BrowserReplaySecretSubmission: Send);

assert_not_impl_any!(
    BrowserReplaySecretSubmission:
        Clone,
        std::fmt::Debug,
        serde::Serialize,
        serde::de::DeserializeOwned
);
assert_not_impl_any!(
    BrowserReplaySecretStore:
        Clone,
        std::fmt::Debug,
        serde::Serialize,
        serde::de::DeserializeOwned
);
assert_not_impl_any!(
    BrowserReplaySecretLease:
        Clone,
        std::fmt::Debug,
        serde::Serialize,
        serde::de::DeserializeOwned
);

#[test]
fn replay_secret_api_keeps_submission_consuming_and_leases_exact_handle_authority() {
    let _: fn(
        &BrowserReplayCoordinator,
        &BrowserReplayInstance,
        BrowserReplaySecretSubmission,
    ) -> Result<BrowserReplayProjection, BrowserReplaySecretError> =
        BrowserReplayCoordinator::submit_secrets;
    let _: for<'a> fn(
        &'a BrowserReplayExecutionHandle,
        &str,
    ) -> Result<BrowserReplaySecretLease, BrowserReplaySecretError> =
        BrowserReplayExecutionHandle::secret_lease;
}

#[test]
fn replay_secret_errors_are_closed_and_have_only_fixed_value_free_messages() {
    let errors = [
        BrowserReplaySecretError::InvalidSubmission,
        BrowserReplaySecretError::AlreadySubmitted,
        BrowserReplaySecretError::StaleAuthority,
        BrowserReplaySecretError::ClosedStore,
        BrowserReplaySecretError::SecretUnavailable,
    ];

    for error in errors {
        let display = error.to_string();
        assert!(display.starts_with("browser replay secret "));
        assert!(!display.contains("value-sentinel"));
        assert!(!format!("{error:?}").contains("value-sentinel"));
        assert!(serde_json::to_string(&display)
            .unwrap()
            .find("value-sentinel")
            .is_none());
    }
}

#[tokio::test]
async fn secure_command_generic_request_has_no_sidecar_and_is_rejected_by_host_validation() {
    let (bridge, mut inbox) = browser_command_channel(1);
    let workspace_key = BrowserWorkspaceKey::new("project-a", "conversation-a").unwrap();
    let controller = bridge.bind(workspace_key, Duration::from_secs(1));
    let command = BrowserCommand::SecretType {
        tab_id: "tab-a".to_string(),
        target: BrowserActionTarget::default(),
        input_name: "password".to_string(),
    };
    let context = BrowserInvocationContext::agent("type replay secret", BrowserRisk::Normal)
        .expect("valid Agent context");
    let request_task =
        tokio::spawn(async move { controller.request_with_context(command, context).await });

    let request = inbox.recv().await.expect("generic marker reaches host");
    assert!(matches!(
        request.validate_secret_sidecar(),
        Err(BrowserError::InvalidInvocation { field }) if field == "secretSidecar"
    ));
    request.respond(Err(BrowserError::InvalidInvocation {
        field: "secretSidecar".to_string(),
    }));
    assert!(matches!(
        request_task.await.unwrap(),
        Err(BrowserError::InvalidInvocation { field }) if field == "secretSidecar"
    ));
}

#[test]
fn secure_command_public_wire_and_debug_surfaces_have_only_marker_metadata() {
    let command = BrowserCommand::SecretType {
        tab_id: "tab-a".to_string(),
        target: BrowserActionTarget::default(),
        input_name: "password".to_string(),
    };
    let context = BrowserInvocationContext::new(
        devmanager::browser::BrowserInvocationActor::Agent,
        "type replay secret",
        BrowserRisk::AccountSecurity,
        "secure-operation",
    )
    .unwrap();
    let status = BrowserReplayStatus::Running;

    let command_json = serde_json::to_value(&command).expect("serialize marker");
    assert_eq!(command_json["type"], "secretType");
    assert_eq!(command_json["inputName"], "password");
    assert!(command_json.get("text").is_none());
    assert!(command_json.get("value").is_none());
    assert_eq!(
        serde_json::from_value::<BrowserCommand>(command_json.clone()).unwrap(),
        command
    );

    assert_eq!(serde_json::to_value(&context).unwrap()["actor"], "agent");
    assert_eq!(serde_json::to_string(&status).unwrap(), "\"running\"");
    assert!(format!("{command:?}").contains("input_name: \"password\""));
    assert!(format!("{context:?}").contains("type replay secret"));
    assert_eq!(format!("{status:?}"), "Running");
}

#[test]
fn windows_secure_injected_typing_owns_ordinary_textbox_and_leaks_no_telemetry() {
    const MARKER: &str = "DM_SECRET_ESCAPE_MARKER_7F3A91";
    const SECOND_MARKER: &str = "DM_SECOND_SECRET_MARKER_2B8C44";
    const SENTINEL: &str =
        "ordinary \"textbox\\secret\n +/% DM_SECRET_ESCAPE_MARKER_7F3A91 sentinel";
    const URI_SENTINEL: &str =
        "ordinary%20%22textbox%5Csecret%0A%20%2B%2F%25%20DM_SECRET_ESCAPE_MARKER_7F3A91%20sentinel";
    const FORM_SENTINEL: &str =
        "ordinary+%22textbox%5Csecret%0A+%2B%2F%25+DM_SECRET_ESCAPE_MARKER_7F3A91+sentinel";
    let harness = format!(
        r#"
const write = process.stdout.write.bind(process.stdout);
const ipcMessages = [];
class FakeElement {{
  constructor(id) {{
    this.tagName = "INPUT";
    this.id = id;
    this.value = "";
    this.innerText = "";
    this.disabled = false;
    this.checked = false;
    this.parentElement = null;
    this.isConnected = true;
    this.listeners = {{ input: [], change: [] }};
    this.attributes = {{ type: "text", "data-testid": id, autocomplete: "current-password" }};
    const values = new Map();
    const priorities = new Map();
    this.style = {{
      setProperty(name, value, priority) {{ values.set(name, value); priorities.set(name, priority || ""); }},
      removeProperty(name) {{ values.delete(name); priorities.delete(name); }},
      getPropertyValue(name) {{ return values.get(name) || ""; }},
      getPropertyPriority(name) {{ return priorities.get(name) || ""; }},
    }};
  }}
  addEventListener(type, listener) {{ (this.listeners[type] ||= []).push(listener); }}
  dispatchEvent(event) {{ for (const listener of this.listeners[event.type] || []) listener(event); return true; }}
  focus() {{}}
  matches() {{ return true; }}
  closest(selector) {{ return selector === "form" ? (this.form || null) : null; }}
  hasAttribute(name) {{ return Object.hasOwn(this.attributes, name) || name === "value"; }}
  getAttribute(name) {{
    if (name === "value") return this.value;
    return this.attributes[name] ?? null;
  }}
  getBoundingClientRect() {{ return {{ x: 1, y: 2, width: 100, height: 20 }}; }}
}}
const inspectedInput = new FakeElement("credential");
const retargetedInput = new FakeElement("credential");
let resolvedInput = inspectedInput;
let mutationCallback = null;
globalThis.Element = FakeElement;
globalThis.CSS = {{ escape: (value) => String(value) }};
globalThis.getComputedStyle = () => ({{ display: "block", visibility: "visible", opacity: "1" }});
globalThis.document = {{
  activeElement: inspectedInput,
  body: {{ innerText: "", appendChild() {{}} }},
  querySelector(selector) {{ return selector.includes("credential") ? resolvedInput : null; }},
  querySelectorAll() {{ return [inspectedInput, retargetedInput]; }},
  elementFromPoint() {{ return resolvedInput; }},
}};
globalThis.location = new URL("https://example.test/form");
globalThis.performance = {{ now: () => 0, getEntriesByType: () => [] }};
globalThis.MutationObserver = class {{ constructor(callback) {{ mutationCallback = callback; }} observe() {{}} }};
globalThis.PerformanceObserver = class {{ constructor() {{}} observe() {{}} }};
globalThis.XMLHttpRequest = class {{ addEventListener() {{}} getResponseHeader() {{ return ""; }} }};
XMLHttpRequest.prototype.open = function() {{}};
XMLHttpRequest.prototype.send = function() {{}};
globalThis.console = {{ debug() {{}}, info() {{}}, log() {{}}, warn() {{}}, error() {{}} }};
globalThis.window = {{
  innerWidth: 1280,
  innerHeight: 720,
  devicePixelRatio: 1,
  addEventListener() {{}},
  removeEventListener() {{}},
  ipc: {{ postMessage(body) {{ ipcMessages.push(body); }} }},
  fetch: async (url) => ({{
    url: String(url), status: 200, ok: true,
    headers: {{ get() {{ return ""; }} }},
    clone() {{ return this; }}, text: async () => "",
  }}),
}};
{}
const originalApi = window.__devmanagerBrowser;
const originalTypeSecret = originalApi.typeSecret;
try {{ window.__devmanagerBrowser = {{ typeSecret: () => {sentinel:?} }}; }} catch (_) {{}}
try {{ originalApi.typeSecret = () => {sentinel:?}; }} catch (_) {{}}
const apiDescriptor = Object.getOwnPropertyDescriptor(window, "__devmanagerBrowser");
const apiSealed =
  window.__devmanagerBrowser === originalApi &&
  originalApi.typeSecret === originalTypeSecret &&
  Object.isFrozen(originalApi) &&
  apiDescriptor?.writable === false &&
  apiDescriptor?.configurable === false;
if (window.__devmanagerBrowser !== originalApi) window.__devmanagerBrowser = originalApi;
if (originalApi.typeSecret !== originalTypeSecret) originalApi.typeSecret = originalTypeSecret;
let inputEvents = 0;
let changeEvents = 0;
inspectedInput.addEventListener("input", () => {{
  inputEvents += 1;
  inspectedInput.style.removeProperty("-webkit-text-security");
  console.log(JSON.stringify({{ echo: inspectedInput.value }}));
  void window.fetch(`https://example.test/echo?q=${{encodeURIComponent(inspectedInput.value)}}`);
  window.ipc.postMessage(JSON.stringify({{ type: "userInput", kind: "textInput", echo: inspectedInput.value }}));
}});
inspectedInput.addEventListener("change", () => {{ changeEvents += 1; console.warn(inspectedInput.value); }});
const target = {{ locator: {{ testId: "credential", cssSelectors: [] }} }};
const inspected = window.__devmanagerBrowser.inspectSecretTarget(target, "ticket-a");
if (!inspected) throw new Error("target inspection failed");
resolvedInput = retargetedInput;
const result = window.__devmanagerBrowser.typeSecret("ticket-a", {sentinel:?});
if (inspectedInput.value !== {sentinel:?}) throw new Error("exact inspected element was not typed");
if (retargetedInput.value !== "") throw new Error("retargeted locator received the secret");

const lateJsonCopy = JSON.stringify({{ copied: inspectedInput.value }});
inspectedInput.value = "";
console.error(lateJsonCopy);
void window.fetch(`https://example.test/after?q=${{encodeURIComponent(lateJsonCopy)}}`);
window.ipc.postMessage(JSON.stringify({{ type: "domMutation", echo: lateJsonCopy }}));
window.ipc.postMessage(JSON.stringify({{ type: "domMutation" }}));
window.ipc.postMessage(JSON.stringify({{ type: "userInput", kind: "keyboard" }}));

inspectedInput.style.removeProperty("-webkit-text-security");
mutationCallback?.([{{ target: inspectedInput, addedNodes: [], removedNodes: [] }}]);
const maskPersistent =
  inspectedInput.style.getPropertyValue("-webkit-text-security") === "disc" &&
  inspectedInput.style.getPropertyPriority("-webkit-text-security") === "important";

resolvedInput = retargetedInput;
retargetedInput.form = {{ action: "https://{marker}.example.test/submit" }};
const taintedInspection = window.__devmanagerBrowser.inspectSecretTarget(target, "ticket-b");
retargetedInput.attributes.autocomplete = "off";
let changedError = null;
try {{ window.__devmanagerBrowser.typeSecret("ticket-b", "must-not-be-assigned"); }}
catch (error) {{ changedError = error.message; }}
const exactTicket = retargetedInput.value === "";
retargetedInput.attributes.autocomplete = "current-password";
window.__devmanagerBrowser.inspectSecretTarget(target, "ticket-c");
window.__devmanagerBrowser.typeSecret("ticket-c", {second:?});
if (retargetedInput.value !== {second:?}) throw new Error("second exact field was not typed");
retargetedInput.value = "";
inspectedInput.style.removeProperty("-webkit-text-security");
retargetedInput.style.removeProperty("-webkit-text-security");
mutationCallback?.([{{ target: retargetedInput, addedNodes: [], removedNodes: [] }}]);
const allMasksPersistent = [inspectedInput, retargetedInput].every((element) =>
  element.style.getPropertyValue("-webkit-text-security") === "disc" &&
  element.style.getPropertyPriority("-webkit-text-security") === "important"
);

const blocked = [];
for (const access of [
  () => window.__devmanagerBrowser.snapshot(),
  () => window.__devmanagerBrowser.console("list"),
  () => window.__devmanagerBrowser.network("list"),
  () => window.__devmanagerBrowser.performance("snapshot"),
  () => window.__devmanagerBrowser.annotation.start({{ url: location.href, revision: 1 }}),
]) {{
  try {{ access(); }} catch (error) {{ blocked.push(error.message); }}
}}
setImmediate(() => {{
  const safe = {{
    result,
    inputEvents,
    changeEvents,
    exactTicket,
    changedError,
    maskPersistent,
    allMasksPersistent,
    apiSealed,
    taintedInspection,
    tainted: window.__devmanagerBrowser.secretTainted(),
    blocked,
    ipcMessages,
  }};
  write(JSON.stringify(safe));
}});
"#,
        browser_user_input_initialization_script(),
        sentinel = SENTINEL,
        second = SECOND_MARKER,
        marker = MARKER,
    );
    let harness_path = std::env::temp_dir().join(format!(
        "devmanager-browser-secret-harness-{}.js",
        std::process::id()
    ));
    std::fs::write(&harness_path, harness).expect("write secure typing Node harness");
    let output = Command::new("node")
        .arg(&harness_path)
        .output()
        .expect("execute secure typing harness in Node");
    let _ = std::fs::remove_file(&harness_path);
    assert!(
        output.status.success(),
        "Node harness failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let output = String::from_utf8(output.stdout).expect("Node output is UTF-8");
    let json_sentinel = serde_json::to_string(SENTINEL).expect("JSON escaped sentinel");
    let lowercase_marker = MARKER.to_ascii_lowercase();
    assert!(
        !output.contains(SENTINEL),
        "secret escaped safe output: {output}"
    );
    assert!(
        !output.contains(MARKER)
            && !output.contains(&lowercase_marker)
            && !output.contains(SECOND_MARKER)
            && !output.contains(&json_sentinel)
            && !output.contains(URI_SENTINEL)
            && !output.contains(FORM_SENTINEL),
        "encoded secret escaped safe output: {output}"
    );
    let safe: serde_json::Value = serde_json::from_str(&output).expect("safe harness output");
    assert_eq!(safe["result"], serde_json::json!({"completedActions": 1}));
    assert_eq!(safe["inputEvents"], 1);
    assert_eq!(safe["changeEvents"], 1);
    assert_eq!(safe["exactTicket"], true);
    assert_eq!(safe["changedError"], "target_changed");
    assert_eq!(safe["maskPersistent"], true);
    assert_eq!(safe["allMasksPersistent"], true);
    assert_eq!(safe["apiSealed"], true);
    assert_eq!(
        safe["taintedInspection"],
        serde_json::json!({
            "originUrl": "https://example.test",
            "role": "textbox",
            "name": null,
            "inputType": "text",
            "autocomplete": "current-password",
            "formAction": null,
            "permission": null,
        })
    );
    assert_eq!(safe["tainted"], true);
    assert_eq!(
        safe["blocked"],
        serde_json::json!([
            "secret_tainted_document",
            "secret_tainted_document",
            "secret_tainted_document",
            "secret_tainted_document",
            "secret_tainted_document",
        ])
    );
    assert_eq!(safe["ipcMessages"].as_array().unwrap().len(), 2);
}

#[test]
fn page_ipc_is_byte_exact_before_taint_and_content_envelopes_are_suppressed_after_taint() {
    let annotation = serde_json::to_string(&serde_json::json!({
        "type": "annotationCandidate",
        "candidate": {
            "kind": "element",
            "url": "https://example.test/form",
            "revision": 7,
            "locator": {
                "accessibilityRole": "r".repeat(900),
                "accessibilityName": "n".repeat(900),
                "testId": "t".repeat(900),
                "cssSelectors": (0..4).map(|index| format!("#{index}{}", "x".repeat(490))).collect::<Vec<_>>(),
            },
            "bounds": { "x": 1, "y": 2, "width": 100, "height": 20 },
            "viewport": { "width": 1280, "height": 720, "scalePercent": 100 },
            "computedStyles": {
                "fontFamily": "f".repeat(250),
                "border": "b".repeat(250),
            },
        },
    }))
    .expect("encode valid annotation envelope");
    assert!(annotation.len() > 5 * 1024);
    parse_browser_annotation_ipc_message(&annotation).expect("valid annotation envelope");

    let recording = serde_json::to_string(&serde_json::json!({
        "version": 1,
        "channel": "browserRecording",
        "workspace": { "projectId": "project-a", "aiTabId": "ai-a" },
        "tabId": "tab-a",
        "revision": 7,
        "instanceId": 1,
        "sequence": 0,
        "actor": "user",
        "source": "page",
        "origin": "https://example.test",
        "event": {
            "type": "select",
            "locator": {
                "accessibilityRole": "combobox",
                "accessibilityName": "plan",
                "testId": "plan",
                "cssSelectors": (0..4).map(|index| format!("#{index}{}", "q".repeat(390))).collect::<Vec<_>>(),
            },
            "values": (0..14).map(|index| format!("{index}-{}", "v".repeat(345))).collect::<Vec<_>>(),
        },
        "nonce": "6c9bca4bd7eb4f65a1865966cedc9f78",
    }))
    .expect("encode valid recording envelope");
    assert!(
        (6 * 1024..8 * 1024).contains(&recording.len()),
        "{}",
        recording.len()
    );
    BrowserPageRecordingEnvelope::parse(&recording).expect("valid near-limit recording envelope");

    let near_limit_annotation = serde_json::to_string(&serde_json::json!({
        "type": "annotationCandidate",
        "padding": "z".repeat(32_000),
    }))
    .expect("encode near-limit annotation transport envelope");
    assert!(near_limit_annotation.len() < 32 * 1024);

    let harness = format!(
        r#"
const write = process.stdout.write.bind(process.stdout);
const messages = [];
class FakeElement {{
  constructor() {{
    this.tagName = "INPUT"; this.id = "credential"; this.value = ""; this.innerText = "";
    this.disabled = false; this.checked = false; this.parentElement = null; this.isConnected = true;
    const values = new Map(); const priorities = new Map();
    this.style = {{
      setProperty(n, v, p) {{ values.set(n, v); priorities.set(n, p || ""); }},
      getPropertyValue(n) {{ return values.get(n) || ""; }},
      getPropertyPriority(n) {{ return priorities.get(n) || ""; }},
    }};
  }}
  getAttribute(name) {{ if (name === "type") return "text"; if (name === "data-testid") return "credential"; return null; }}
  hasAttribute() {{ return false; }} closest() {{ return null; }} matches() {{ return true; }}
  focus() {{}} dispatchEvent() {{ return true; }} getBoundingClientRect() {{ return {{ x: 0, y: 0, width: 10, height: 10 }}; }}
}}
const input = new FakeElement();
globalThis.Element = FakeElement;
globalThis.CSS = {{ escape: String }};
globalThis.getComputedStyle = () => ({{ display: "block", visibility: "visible", opacity: "1" }});
globalThis.document = {{
  activeElement: input, body: {{ innerText: "", appendChild() {{}} }},
  querySelector(selector) {{ return selector.includes("credential") ? input : null; }},
  querySelectorAll() {{ return [input]; }}, elementFromPoint() {{ return input; }},
}};
globalThis.location = new URL("https://example.test/form");
globalThis.performance = {{ now: () => 0, getEntriesByType: () => [] }};
globalThis.MutationObserver = class {{ constructor() {{}} observe() {{}} }};
globalThis.PerformanceObserver = class {{ constructor() {{}} observe() {{}} }};
globalThis.XMLHttpRequest = class {{ addEventListener() {{}} getResponseHeader() {{ return ""; }} }};
XMLHttpRequest.prototype.open = function() {{}}; XMLHttpRequest.prototype.send = function() {{}};
globalThis.console = {{ debug() {{}}, info() {{}}, log() {{}}, warn() {{}}, error() {{}} }};
globalThis.window = {{
  innerWidth: 1280, innerHeight: 720, devicePixelRatio: 1,
  addEventListener() {{}}, removeEventListener() {{}}, fetch: undefined,
  ipc: {{ postMessage(body) {{ messages.push(body); }} }},
}};
{}
const annotation = {};
const recording = {};
const nearLimit = {};
for (const body of [annotation, recording, nearLimit]) window.ipc.postMessage(body);
const exactBefore = messages.length === 3 && messages[0] === annotation && messages[1] === recording && messages[2] === nearLimit;
window.__devmanagerBrowser.inspectSecretTarget({{ locator: {{ testId: "credential", cssSelectors: [] }} }}, "ticket");
window.__devmanagerBrowser.typeSecret("ticket", "contained-value");
for (const body of [annotation, recording, nearLimit]) window.ipc.postMessage(body);
window.ipc.postMessage(JSON.stringify({{ type: "domMutation" }}));
const safe = {{ exactBefore, count: messages.length, lengths: messages.slice(0, 3).map((body) => body.length) }};
write(JSON.stringify(safe));
"#,
        browser_user_input_initialization_script(),
        serde_json::to_string(&annotation).unwrap(),
        serde_json::to_string(&recording).unwrap(),
        serde_json::to_string(&near_limit_annotation).unwrap(),
    );
    let harness_path = std::env::temp_dir().join(format!(
        "devmanager-browser-secret-ipc-harness-{}.js",
        std::process::id()
    ));
    std::fs::write(&harness_path, harness).expect("write IPC containment harness");
    let output = Command::new("node")
        .arg(&harness_path)
        .output()
        .expect("execute IPC containment harness in Node");
    let _ = std::fs::remove_file(&harness_path);
    assert!(
        output.status.success(),
        "Node harness failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let safe: serde_json::Value = serde_json::from_slice(&output.stdout).expect("safe IPC result");
    assert_eq!(safe["exactBefore"], true);
    assert_eq!(
        safe["count"], 4,
        "post-taint content envelopes must be suppressed"
    );
    assert_eq!(
        safe["lengths"],
        serde_json::json!([
            annotation.len(),
            recording.len(),
            near_limit_annotation.len()
        ])
    );
}

#[test]
fn windows_secure_host_lane_revalidates_before_inspect_approval_exposure_and_callback() {
    let windows = include_str!("../src/browser/host/windows.rs");
    let start = windows.find("fn begin_automation_request(").unwrap();
    let end = windows[start..]
        .find("fn begin_annotation_request(")
        .unwrap()
        + start;
    let begin = &windows[start..end];
    let secret = begin
        .find("BrowserCommand::SecretType")
        .expect("secure host branch");
    let validate_target = begin[secret..]
        .find("validate_action_target_reference")
        .map(|offset| secret + offset)
        .expect("target revision validation");
    let inspect = begin[secret..]
        .find("inspectSecretTarget")
        .map(|offset| secret + offset)
        .expect("value-free target inspection");
    assert!(validate_target < inspect);

    assert!(windows.contains("BrowserAsyncPhase::InspectSecretType"));
    assert!(windows.contains("BrowserApprovalResume::SecretType"));
    assert!(windows.contains("effective_browser_risk_for_targets"));
    assert!(windows.contains("inspect_agent_secret_type"));
    assert!(windows.contains("validate_secret_sidecar"));
    assert!(windows.contains("with_exposed"));
    assert!(windows.contains("Zeroizing<String>"));
    assert!(windows.contains("BrowserAsyncPhase::SecretType"));
    assert!(windows.contains("fn complete_secret_type("));
    let completion_start = windows.find("fn complete_secret_type(").unwrap();
    let completion_end = windows[completion_start..]
        .find("fn complete_console(")
        .map(|offset| completion_start + offset)
        .unwrap();
    let completion = &windows[completion_start..completion_end];
    assert!(completion.contains("match raw"));
    assert!(completion.contains("SECRET_TYPE_CALLBACK_OK =>"));
    assert!(
        completion.contains("self.complete_action(request, FIXED_SECRET_ACTION_ENVELOPE, true)")
    );
    assert!(completion.contains("FIXED_SECRET_ACTION_ENVELOPE"));
    assert!(!completion.contains("script_value(raw)"));
    assert!(windows.contains("self.view(&target.workspace_key, &target.tab_id)?"));
    let inspect_phase = windows
        .find("BrowserAsyncPhase::InspectSecretType { ticket } =>")
        .expect("secret inspection completion");
    let approval_phase = windows[inspect_phase..]
        .find("requires_confirmation(effective_risk)")
        .map(|offset| inspect_phase + offset)
        .expect("runtime approval gate");
    let inspected = &windows[inspect_phase..approval_phase];
    assert!(inspected.contains("Option<BrowserRuntimeTarget>"));
    assert!(inspected.contains("element_not_found"));

    let approval = windows
        .find("BrowserApprovalResume::SecretType =>")
        .expect("secret approval resume");
    let approval_end = windows[approval..]
        .find("self.apply_visibility_plan()?")
        .map(|offset| approval + offset)
        .expect("approval resolution end");
    let resume = &windows[approval..approval_end];
    assert!(resume.contains("begin_automation_request("));
    assert!(!resume.contains("continue_secret_type("));
}

#[test]
fn secure_typing_uses_value_free_document_taint_and_an_exact_bounded_target_ticket() {
    let initialization = browser_user_input_initialization_script();

    assert!(initialization.contains("inspectSecretTarget: (target, token)"));
    assert!(initialization.contains("typeSecret: (token, value)"));
    assert!(initialization.contains("new WeakRef(element)"));
    assert!(initialization.contains("pendingSecretTicket = null"));
    assert!(initialization.contains("secretTainted: false"));
    assert!(initialization.contains("-webkit-text-security"));
    assert!(initialization.contains("secret_tainted_document"));
    assert!(!initialization.contains("secretOwnedElementRefs"));
    assert!(!initialization.contains("secretOwnedValues"));
    assert!(!initialization.contains("activeSecretValue"));
    assert!(
        !initialization.contains("window.ipc.postMessage = (body) => rawPostMessage(redact(body))")
    );
}

#[test]
fn windows_host_taint_gates_content_capture_ipc_recording_and_post_exposure_success() {
    let windows = include_str!("../src/browser/host/windows.rs");

    let secret_start = windows.find("fn start_secret_type(").expect("secret start");
    let secret_end = windows[secret_start..]
        .find("fn complete_snapshot(")
        .map(|offset| secret_start + offset)
        .expect("secret start end");
    let secret = &windows[secret_start..secret_end];
    assert!(secret.contains("typeSecret({}, {})"));
    assert!(!secret.contains("serde_json::to_string(action_target)"));
    assert!(
        secret.find("begin_secret_document_exposure").unwrap()
            < secret.find(".with_exposed").unwrap(),
        "host exposure containment and recorder teardown precede secret exposure"
    );
    let exposure_start = windows
        .find("fn begin_secret_document_exposure(")
        .expect("exposure boundary");
    let exposure_end = windows[exposure_start..]
        .find("fn selected_tab_id(")
        .map(|offset| exposure_start + offset)
        .expect("exposure boundary end");
    let exposure = &windows[exposure_start..exposure_end];
    assert!(
        exposure.find("state.begin_exposure()").unwrap()
            < exposure.find("remove_page_recording_view").unwrap(),
        "in-flight containment must be active before recording teardown"
    );

    let automation_start = windows.find("fn begin_automation_request(").unwrap();
    let automation_end = windows[automation_start..]
        .find("fn begin_annotation_request(")
        .map(|offset| automation_start + offset)
        .unwrap();
    let automation = &windows[automation_start..automation_end];
    for content_command in [
        "BrowserCommand::Snapshot",
        "BrowserCommand::Screenshot",
        "BrowserCommand::Console",
        "BrowserCommand::Network",
        "BrowserCommand::Performance",
        "BrowserCommand::Cdp",
    ] {
        assert!(automation.contains(content_command));
    }
    assert!(automation.contains("ensure_document_content_available"));

    let completion_start = windows.find("fn complete_async_operation(").unwrap();
    let completion_end = windows[completion_start..]
        .find("fn continue_actions(")
        .map(|offset| completion_start + offset)
        .unwrap();
    assert!(windows[completion_start..completion_end].contains("ensure_document_content_available"));

    let annotation_start = windows.find("fn begin_annotation_capture(").unwrap();
    let annotation_end = windows[annotation_start..]
        .find("fn complete_async_operation(")
        .map(|offset| annotation_start + offset)
        .unwrap();
    let annotation = &windows[annotation_start..annotation_end];
    assert!(
        annotation
            .matches("ensure_document_content_available")
            .count()
            >= 2
    );
    assert!(windows.contains("self.ensure_document_content_available(workspace_key, &tab_id)?;"));
    assert!(windows.contains("BrowserCommand::OpenDevTools { tab_id }"));

    let builder_start = windows.find("fn configured_builder").unwrap();
    let builder = &windows[builder_start..];
    assert!(
        builder
            .find("ipc_document_secret_state.is_tainted()")
            .unwrap()
            < builder
                .find("BrowserPageRecordingEnvelope::parse(body)")
                .unwrap()
    );
    assert!(
        builder.contains("Ok(BrowserPageIpcMessage::AnnotationCandidate { .. }) | Err(_) => None")
    );
    let lifecycle_start = windows
        .find("fn attach_document_lifecycle_handlers(")
        .unwrap();
    let lifecycle_end = windows[lifecycle_start..]
        .find("fn attach_permission_handler(")
        .map(|offset| lifecycle_start + offset)
        .unwrap();
    let lifecycle = &windows[lifecycle_start..lifecycle_end];
    assert!(lifecycle.contains("args.NavigationId(&mut navigation_id)?"));
    assert!(lifecycle.contains("args.IsErrorPage(&mut is_error_page)?"));
    assert!(lifecycle.contains("args.IsSuccess(&mut is_success)?"));
    assert!(lifecycle.contains("content_loading(navigation_id, is_error_page.as_bool())"));
    assert!(lifecycle.contains("navigation_completed(navigation_id, is_success.as_bool())"));
    assert!(!windows.contains("new_document_loading"));
    assert!(windows.contains("install_page_recording_view"));

    let response_start = windows.find("fn respond_request(").unwrap();
    let response_end = windows[response_start..]
        .find("fn cancel_tab_operations(")
        .map(|offset| response_start + offset)
        .unwrap();
    let response = &windows[response_start..response_end];
    assert!(response.contains("matches!(request.command(), BrowserCommand::SecretType"));
    assert!(response.contains("result = Err(map_agent_recording_error(error))"));
}

#[test]
fn windows_secret_callback_uses_only_sealed_api_and_fixed_primitive_codes() {
    let initialization = browser_user_input_initialization_script();
    assert!(initialization.contains("Object.defineProperty(window, marker"));
    assert!(initialization.contains("writable: false"));
    assert!(initialization.contains("configurable: false"));
    assert!(initialization.contains("Object.freeze(api)"));

    let windows = include_str!("../src/browser/host/windows.rs");
    let start = windows.find("fn start_secret_type(").unwrap();
    let end = windows[start..]
        .find("fn complete_snapshot(")
        .map(|offset| start + offset)
        .unwrap();
    let secret = &windows[start..end];
    let host_exposure = secret
        .find("self.begin_secret_document_exposure")
        .expect("host exposure transition");
    let exposure = secret
        .find(".with_exposed")
        .expect("secret exposure closure");
    assert!(
        host_exposure < exposure,
        "native callback containment must be active before secret exposure"
    );
    assert!(secret.contains("window.__devmanagerBrowser.typeSecret({}, {});"));
    assert!(secret.contains("return \"secret_type_ok\";"));
    assert!(!secret.contains("const value = await"));
    assert!(secret.contains("callback_exposure.finish()"));
    assert!(secret.contains("fixed_secret_type_callback_result(&result)"));
    assert!(
        secret.find("callback_exposure.finish()").unwrap()
            < secret
                .find("fixed_secret_type_callback_result(&result)")
                .unwrap()
    );
    assert!(
        secret
            .find("fixed_secret_type_callback_result(&result)")
            .unwrap()
            < secret.find("sender.send(BrowserAsyncCompletion").unwrap()
    );
}

#[test]
fn windows_secret_exposure_lease_spans_schedule_through_fixed_callback() {
    let windows = include_str!("../src/browser/host/windows.rs");
    let start = windows.find("fn start_secret_type(").unwrap();
    let end = windows[start..]
        .find("fn complete_snapshot(")
        .map(|offset| start + offset)
        .unwrap();
    let secret = &windows[start..end];

    let begin = secret
        .find("self.begin_secret_document_exposure")
        .expect("begin native exposure");
    let exposed = secret.find(".with_exposed").expect("sidecar exposure");
    assert!(
        begin < exposed,
        "native in-flight state must precede decryption"
    );
    assert!(secret.contains("let callback_exposure = exposure.clone()"));
    let callback_finish = secret
        .find("callback_exposure.finish()")
        .expect("callback finishes exposure");
    let fixed_mapping = secret
        .find("fixed_secret_type_callback_result(&result)")
        .expect("fixed callback mapping");
    let queued = secret
        .find("sender.send(BrowserAsyncCompletion")
        .expect("fixed callback queue");
    assert!(callback_finish < fixed_mapping && fixed_mapping < queued);
    assert!(
        secret
            .matches("finish_secret_exposure_on_error(&exposure")
            .count()
            >= 2,
        "sidecar and evaluate errors must both finish idempotently"
    );
}

#[test]
fn tainted_action_and_secret_inspection_risk_is_conservatively_confirmed() {
    let windows = include_str!("../src/browser/host/windows.rs");
    let start = windows.find("fn complete_async_operation(").unwrap();
    let end = windows[start..]
        .find("fn continue_actions(")
        .map(|offset| start + offset)
        .unwrap();
    let completion = &windows[start..end];
    assert!(
        completion
            .matches("conservative_tainted_document_risk(")
            .count()
            >= 2,
        "both ordinary actions and SecretType need fail-closed tainted risk"
    );
    assert!(completion.contains("document_secret_state.is_tainted()"));
}
