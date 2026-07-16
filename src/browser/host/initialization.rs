pub const USER_INPUT_INITIALIZATION_SCRIPT: &str = r#"
(() => {
  const marker = "__devmanagerBrowserInputBridge";
  if (window[marker]) return;
  window[marker] = true;
  const report = (kind) => (event) => {
    if (!event.isTrusted) return;
    window.ipc.postMessage(JSON.stringify({ type: "userInput", kind }));
  };
  window.addEventListener("pointerdown", report("pointer"), true);
  window.addEventListener("keydown", report("keyboard"), true);
  window.addEventListener("input", report("input"), true);
})();
"#;

pub fn browser_user_input_initialization_script() -> &'static str {
    USER_INPUT_INITIALIZATION_SCRIPT
}
