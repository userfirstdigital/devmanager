use crate::ai::claude_hooks::{
    append_claude_cli_arguments, is_safe_cmd_settings_root, ClaudeShellKind,
};
use crate::ai::codex_cli::CodexConfigOverride;
use axum::http::Uri;
use serde_json::json;
use std::collections::HashMap;
use std::fmt;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};

pub const DEVMANAGER_BROWSER_TOKEN_ENV: &str = "DEVMANAGER_BROWSER_TOKEN";
const CLAUDE_OVERLAY_PREFIX: &str = "devmanager-browser-mcp-";

#[derive(Clone)]
pub struct BrowserProviderAccess {
    endpoint: String,
    bearer_token: String,
}

impl BrowserProviderAccess {
    pub fn new(
        endpoint: impl Into<String>,
        bearer_token: impl Into<String>,
    ) -> Result<Self, String> {
        let endpoint = endpoint.into();
        validate_loopback_mcp_endpoint(&endpoint)?;
        let bearer_token = bearer_token.into();
        if bearer_token.is_empty() || bearer_token.contains(char::is_whitespace) {
            return Err("browser bearer token must be a nonblank opaque value".to_string());
        }
        Ok(Self {
            endpoint,
            bearer_token,
        })
    }

    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    pub(crate) fn bearer_token(&self) -> &str {
        &self.bearer_token
    }

    /// Returns the ephemeral secret solely for provider process launch.
    /// Callers must put it in the child environment and never persist or log it.
    pub fn bearer_token_for_launch(&self) -> &str {
        self.bearer_token()
    }

    pub fn environment(&self) -> HashMap<String, String> {
        HashMap::from([(
            DEVMANAGER_BROWSER_TOKEN_ENV.to_string(),
            self.bearer_token.clone(),
        )])
    }
}

impl fmt::Debug for BrowserProviderAccess {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("BrowserProviderAccess")
            .field("endpoint", &self.endpoint)
            .field("bearer_token", &"<redacted>")
            .finish()
    }
}

pub struct ClaudeBrowserOverlay {
    root: PathBuf,
    path: PathBuf,
    startup_command: String,
    environment: HashMap<String, String>,
    cleaned: bool,
}

impl fmt::Debug for ClaudeBrowserOverlay {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ClaudeBrowserOverlay")
            .field("path", &self.path)
            .field("startup_command", &self.startup_command)
            .field("environment", &"<redacted>")
            .finish()
    }
}

impl ClaudeBrowserOverlay {
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn startup_command(&self) -> &str {
        &self.startup_command
    }

    pub fn environment(&self) -> &HashMap<String, String> {
        &self.environment
    }

    pub fn cleanup(mut self) -> Result<(), String> {
        self.remove_owned_file()?;
        self.cleaned = true;
        Ok(())
    }

    fn remove_owned_file(&self) -> Result<(), String> {
        let owned = self.path.parent() == Some(self.root.as_path())
            && self
                .path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| {
                    name.starts_with(CLAUDE_OVERLAY_PREFIX) && name.ends_with(".json")
                });
        if !owned {
            return Err("refusing to remove an unverified Claude browser overlay path".to_string());
        }
        match std::fs::remove_file(&self.path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(format!(
                "remove Claude browser overlay {}: {error}",
                self.path.display()
            )),
        }
    }
}

impl Drop for ClaudeBrowserOverlay {
    fn drop(&mut self) {
        if !self.cleaned {
            let _ = self.remove_owned_file();
        }
    }
}

pub fn prepare_claude_browser_overlay(
    root: impl AsRef<Path>,
    process_session_id: &str,
    startup_command: &str,
    shell: ClaudeShellKind,
    access: &BrowserProviderAccess,
) -> Result<ClaudeBrowserOverlay, String> {
    validate_process_session_id(process_session_id)?;
    let root = root.as_ref().to_path_buf();
    if shell == ClaudeShellKind::Cmd && !is_safe_cmd_settings_root(&root) {
        return Err("Claude browser overlay path cannot be quoted safely for cmd.exe".to_string());
    }
    std::fs::create_dir_all(&root)
        .map_err(|error| format!("create Claude browser overlay directory: {error}"))?;

    let mut entropy = [0_u8; 8];
    getrandom::fill(&mut entropy)
        .map_err(|error| format!("generate Claude browser overlay identity: {error}"))?;
    let suffix = entropy
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let path = root.join(format!(
        "{CLAUDE_OVERLAY_PREFIX}{process_session_id}-{suffix}.json"
    ));
    let startup_command = append_claude_cli_arguments(
        startup_command,
        shell,
        &[
            "--mcp-config".to_string(),
            path.to_string_lossy().into_owned(),
        ],
    )?;

    let value = json!({
        "mcpServers": {
            "devmanager-browser": {
                "type": "http",
                "url": access.endpoint(),
                "headers": {
                    "Authorization": format!("Bearer ${{{DEVMANAGER_BROWSER_TOKEN_ENV}}}")
                }
            }
        }
    });
    let bytes = serde_json::to_vec_pretty(&value)
        .map_err(|error| format!("serialize Claude browser MCP overlay: {error}"))?;
    let write_result = write_private_file(&path, &bytes);
    if let Err(error) = write_result {
        let _ = std::fs::remove_file(&path);
        return Err(error);
    }
    Ok(ClaudeBrowserOverlay {
        root,
        path,
        startup_command,
        environment: access.environment(),
        cleaned: false,
    })
}

pub fn codex_browser_config_overrides(access: &BrowserProviderAccess) -> Vec<CodexConfigOverride> {
    [
        (
            "mcp_servers.devmanager_browser.url",
            serde_json::to_string(access.endpoint()).expect("serialize static endpoint string"),
        ),
        (
            "mcp_servers.devmanager_browser.bearer_token_env_var",
            serde_json::to_string(DEVMANAGER_BROWSER_TOKEN_ENV)
                .expect("serialize static environment variable name"),
        ),
        (
            "mcp_servers.devmanager_browser.required",
            "false".to_string(),
        ),
        (
            "mcp_servers.devmanager_browser.default_tools_approval_mode",
            "\"approve\"".to_string(),
        ),
    ]
    .into_iter()
    .map(|(key, value)| {
        CodexConfigOverride::new(key, value).expect("static Codex browser config is valid")
    })
    .collect()
}

fn validate_process_session_id(session_id: &str) -> Result<(), String> {
    if session_id.is_empty()
        || session_id.len() > 160
        || !session_id
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
    {
        return Err("browser provider process session id is unsafe".to_string());
    }
    Ok(())
}

fn validate_loopback_mcp_endpoint(endpoint: &str) -> Result<(), String> {
    let uri: Uri = endpoint
        .parse()
        .map_err(|_| "browser MCP endpoint is not a valid URI".to_string())?;
    let authority = uri
        .authority()
        .ok_or_else(|| "browser MCP endpoint has no authority".to_string())?;
    if uri.scheme_str() != Some("http")
        || !matches!(authority.host(), "127.0.0.1" | "localhost")
        || authority.port_u16().is_none()
        || uri.path() != "/mcp"
        || uri.query().is_some()
    {
        return Err("browser MCP endpoint must be an exact loopback HTTP /mcp URL".to_string());
    }
    Ok(())
}

fn write_private_file(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(path)
        .map_err(|error| format!("create Claude browser overlay {}: {error}", path.display()))?;
    file.write_all(bytes)
        .and_then(|()| file.sync_all())
        .map_err(|error| format!("write Claude browser overlay {}: {error}", path.display()))
}
