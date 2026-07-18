use devmanager::browser::{
    browser_command_channel, browser_user_input_initialization_script, BrowserActionTarget,
    BrowserCommand, BrowserError, BrowserInvocationContext, BrowserReplayCoordinator,
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
    const SENTINEL: &str = "ordinary textbox secret +/% sentinel";
    const URI_SENTINEL: &str = "ordinary%20textbox%20secret%20%2B%2F%25%20sentinel";
    const FORM_SENTINEL: &str = "ordinary+textbox+secret+%2B%2F%25+sentinel";
    let harness = format!(
        r#"
const write = process.stdout.write.bind(process.stdout);
const ipcMessages = [];
const listeners = {{ input: [], change: [] }};
class FakeElement {{
  constructor() {{
    this.tagName = "INPUT";
    this.id = "credential";
    this.value = "";
    this.innerText = "";
    this.disabled = false;
    this.checked = false;
    this.parentElement = null;
  }}
  addEventListener(type, listener) {{ (listeners[type] ||= []).push(listener); }}
  dispatchEvent(event) {{ for (const listener of listeners[event.type] || []) listener(event); return true; }}
  focus() {{}}
  matches() {{ return true; }}
  closest() {{ return null; }}
  hasAttribute(name) {{ return name === "value"; }}
  getAttribute(name) {{
    if (name === "type") return "text";
    if (name === "data-testid") return "credential";
    if (name === "value") return this.value;
    return null;
  }}
  getBoundingClientRect() {{ return {{ x: 1, y: 2, width: 100, height: 20 }}; }}
}}
const input = new FakeElement();
let attached = true;
globalThis.Element = FakeElement;
globalThis.CSS = {{ escape: (value) => String(value) }};
globalThis.getComputedStyle = () => ({{ display: "block", visibility: "visible", opacity: "1" }});
globalThis.document = {{
  activeElement: input,
  body: {{ innerText: "", appendChild() {{}} }},
  querySelector(selector) {{ return selector.includes("credential") ? input : null; }},
  querySelectorAll() {{ return attached ? [input] : []; }},
  elementFromPoint() {{ return input; }},
}};
globalThis.location = new URL("https://example.test/form");
globalThis.performance = {{ now: () => 0, getEntriesByType: () => [] }};
globalThis.MutationObserver = class {{ constructor() {{}} observe() {{}} }};
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
let inputEvents = 0;
let changeEvents = 0;
input.addEventListener("input", () => {{
  inputEvents += 1;
  console.log(input.value);
  void window.fetch(`https://example.test/echo?q=${{encodeURIComponent(input.value)}}`);
  window.ipc.postMessage(JSON.stringify({{ type: "userInput", kind: "textInput", echo: input.value }}));
}});
input.addEventListener("change", () => {{ changeEvents += 1; console.warn(input.value); }});
const result = window.__devmanagerBrowser.typeSecret(
  {{ locator: {{ testId: "credential", cssSelectors: [] }} }},
  {sentinel:?},
);
if (input.value !== {sentinel:?}) throw new Error("secret was not assigned to the controlled DOM");
const secretSnapshot = window.__devmanagerBrowser.snapshot();
attached = false;
console.error(input.value);
void window.fetch(`https://example.test/after?q=${{encodeURIComponent(input.value)}}`);
window.ipc.postMessage(JSON.stringify({{ type: "domMutation", echo: input.value }}));
setImmediate(() => {{
  const safe = {{
    result,
    inputEvents,
    changeEvents,
    snapshot: secretSnapshot,
    console: window.__devmanagerBrowser.console("list"),
    network: window.__devmanagerBrowser.network("list"),
    ipcMessages,
  }};
  write(JSON.stringify(safe));
}});
"#,
        browser_user_input_initialization_script(),
        sentinel = SENTINEL,
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
    assert!(
        !output.contains(SENTINEL),
        "secret escaped safe output: {output}"
    );
    assert!(
        !output.contains(URI_SENTINEL) && !output.contains(FORM_SENTINEL),
        "encoded secret escaped safe output: {output}"
    );
    let safe: serde_json::Value = serde_json::from_str(&output).expect("safe harness output");
    assert_eq!(safe["result"], serde_json::json!({"completedActions": 1}));
    assert_eq!(safe["inputEvents"], 1);
    assert_eq!(safe["changeEvents"], 1);
    assert_eq!(safe["snapshot"][0]["value"], "[redacted]");
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
    assert!(windows.contains("result.completed_actions != 1"));
    assert!(windows.contains("self.view(&target.workspace_key, &target.tab_id)?"));
    let inspect_phase = windows
        .find("BrowserAsyncPhase::InspectSecretType =>")
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
