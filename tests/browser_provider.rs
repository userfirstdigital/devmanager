use devmanager::ai::claude_hooks::ClaudeShellKind;
use devmanager::ai::codex_cli::CodexConfigOverride;
use devmanager::browser::{
    codex_browser_config_overrides, prepare_claude_browser_overlay, BrowserProviderAccess,
    DEVMANAGER_BROWSER_TOKEN_ENV,
};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

struct TestDir(PathBuf);

impl TestDir {
    fn new(label: &str) -> Self {
        static NEXT_ID: AtomicU64 = AtomicU64::new(1);
        let path = std::env::temp_dir().join(format!(
            "devmanager-browser-provider-{label}-{}-{}",
            std::process::id(),
            NEXT_ID.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&path).expect("create provider test directory");
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

fn access() -> BrowserProviderAccess {
    BrowserProviderAccess::new("http://127.0.0.1:43127/mcp", "top-secret-browser-token")
        .expect("valid access")
}

#[test]
fn claude_overlay_contains_only_a_literal_token_placeholder_and_preserves_arguments() {
    let temp = TestDir::new("claude overlay with spaces");
    let original = "npx -y @anthropic-ai/claude-code@latest --model sonnet --settings '{\"env\":{\"A\":\"B\"}}'";
    let overlay = prepare_claude_browser_overlay(
        temp.path(),
        "claude-session",
        original,
        ClaudeShellKind::PowerShell,
        &access(),
    )
    .expect("prepare Claude browser overlay");

    assert!(overlay.startup_command().starts_with(original));
    assert!(overlay.startup_command().contains(" --mcp-config '"));
    let raw = std::fs::read_to_string(overlay.path()).expect("read overlay");
    let json: Value = serde_json::from_str(&raw).expect("valid JSON");
    assert_eq!(
        json["mcpServers"]["devmanager-browser"]["headers"]["Authorization"],
        "Bearer ${DEVMANAGER_BROWSER_TOKEN}"
    );
    assert!(!raw.contains("top-secret-browser-token"));
    assert_eq!(
        overlay.environment().get(DEVMANAGER_BROWSER_TOKEN_ENV),
        Some(&"top-secret-browser-token".to_string())
    );

    let path = overlay.path().to_path_buf();
    overlay.cleanup().expect("clean owned overlay");
    assert!(!path.exists());
}

#[test]
fn claude_overlay_uses_each_existing_shell_quoting_rule() {
    let cases = [
        (ClaudeShellKind::Posix, "'"),
        (ClaudeShellKind::PowerShell, "'"),
        (ClaudeShellKind::Cmd, "\""),
    ];
    for (shell, quote) in cases {
        let temp = TestDir::new(&format!("shell-{shell:?}"));
        let overlay = prepare_claude_browser_overlay(
            temp.path(),
            "session-with-spaces",
            "claude --model opus",
            shell,
            &access(),
        )
        .expect("prepare shell overlay");
        let suffix = overlay
            .startup_command()
            .strip_prefix("claude --model opus --mcp-config ")
            .expect("provider argument appended after existing arguments");
        assert!(suffix.starts_with(quote) && suffix.ends_with(quote));
    }
}

#[test]
fn claude_overlay_failure_leaves_the_original_command_and_directory_untouched() {
    let temp = TestDir::new("claude-failure");
    let original = "claude --model opus";
    let error = prepare_claude_browser_overlay(
        temp.path(),
        "../unsafe-session",
        original,
        ClaudeShellKind::PowerShell,
        &access(),
    )
    .expect_err("unsafe ownership name must be rejected");
    assert!(error.contains("session"));
    assert_eq!(std::fs::read_dir(temp.path()).unwrap().count(), 0);
    assert_eq!(original, "claude --model opus");
}

#[test]
fn claude_browser_overlay_rejects_cmd_roots_with_expansion_markers() {
    let temp = TestDir::new("cmd-unsafe-root-parent");
    let unsafe_root = temp.path().join("browser-%TEMP%-!");
    let original = "claude --model opus";

    let error = prepare_claude_browser_overlay(
        &unsafe_root,
        "claude-session",
        original,
        ClaudeShellKind::Cmd,
        &access(),
    )
    .expect_err("cmd.exe expansion markers must be rejected before writing an overlay");

    assert!(error.contains("cmd.exe"));
    assert!(!unsafe_root.exists());
    assert_eq!(original, "claude --model opus");
}

#[test]
fn codex_browser_config_is_typed_exact_and_only_changes_tui_tokens() {
    let overrides = codex_browser_config_overrides(&access());
    assert_eq!(
        overrides,
        vec![
            CodexConfigOverride::new(
                "mcp_servers.devmanager_browser.url",
                "\"http://127.0.0.1:43127/mcp\"",
            )
            .unwrap(),
            CodexConfigOverride::new(
                "mcp_servers.devmanager_browser.bearer_token_env_var",
                "\"DEVMANAGER_BROWSER_TOKEN\"",
            )
            .unwrap(),
            CodexConfigOverride::new("mcp_servers.devmanager_browser.required", "false").unwrap(),
            CodexConfigOverride::new(
                "mcp_servers.devmanager_browser.default_tools_approval_mode",
                "\"approve\"",
            )
            .unwrap(),
        ]
    );
}
