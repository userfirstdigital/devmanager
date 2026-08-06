//! Strict `devmanager-host ctl` parsing and versioned JSON output.

use std::io::{self, Write};
use std::process::ExitCode;
use std::time::Duration;

use serde_json::json;

use super::action::{self, ActionRisk, ActionScope, ACTION_HOST_STATUS};
use super::{HostClient, HostClientConfig};
use crate::domain::ClientId;
use crate::host::IpcError;
use crate::protocol::{CapabilitySet, FrameLimits};

const SCHEMA_VERSION: u16 = 1;
const STATUS_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const STATUS_CONNECT_POLL: Duration = Duration::from_millis(25);
const MAX_DIAGNOSTIC_CHARS: usize = 1_024;

/// Parsed ctl invocation for this lean slice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CtlCommand {
    Actions,
    Status { profile: String },
}

/// Bounded, human-readable ctl failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CliError {
    message: String,
}

impl CliError {
    pub fn new(message: impl Into<String>) -> Self {
        let message = message.into();
        if message.chars().count() <= MAX_DIAGNOSTIC_CHARS {
            return Self { message };
        }
        let mut bounded: String = message.chars().take(MAX_DIAGNOSTIC_CHARS - 1).collect();
        bounded.push('\u{2026}');
        Self { message: bounded }
    }

    pub fn message(&self) -> &str {
        &self.message
    }
}

impl std::fmt::Display for CliError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for CliError {}

/// Parse `ctl` arguments after the leading `ctl` token has been consumed.
pub fn parse_ctl_args<I, S>(args: I) -> Result<CtlCommand, CliError>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut args = args.into_iter().map(|s| s.as_ref().to_string());
    let Some(subcommand) = args.next() else {
        return Err(CliError::new("missing ctl subcommand"));
    };
    match subcommand.as_str() {
        "actions" => parse_actions(args),
        "status" => parse_status(args),
        other => Err(CliError::new(format!("unknown ctl subcommand: {other}"))),
    }
}

fn parse_actions<I>(mut args: I) -> Result<CtlCommand, CliError>
where
    I: Iterator<Item = String>,
{
    let mut json = false;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--json" => {
                if json {
                    return Err(CliError::new("duplicate --json"));
                }
                json = true;
            }
            other if other.starts_with('-') => {
                return Err(CliError::new(format!("unknown flag: {other}")));
            }
            other => return Err(CliError::new(format!("unexpected argument: {other}"))),
        }
    }
    if !json {
        return Err(CliError::new("missing required --json"));
    }
    Ok(CtlCommand::Actions)
}

fn parse_status<I>(mut args: I) -> Result<CtlCommand, CliError>
where
    I: Iterator<Item = String>,
{
    let mut json = false;
    let mut profile: Option<String> = None;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--json" => {
                if json {
                    return Err(CliError::new("duplicate --json"));
                }
                json = true;
            }
            "--profile" => {
                if profile.is_some() {
                    return Err(CliError::new("duplicate --profile"));
                }
                let value = args
                    .next()
                    .ok_or_else(|| CliError::new("missing value for --profile"))?;
                profile = Some(value);
            }
            other if other.starts_with('-') => {
                return Err(CliError::new(format!("unknown flag: {other}")));
            }
            other => return Err(CliError::new(format!("unexpected argument: {other}"))),
        }
    }
    if !json {
        return Err(CliError::new("missing required --json"));
    }
    let profile = profile.ok_or_else(|| CliError::new("missing required --profile"))?;
    let profile = validate_ctl_profile(&profile)?;
    Ok(CtlCommand::Status { profile })
}

fn validate_ctl_profile(raw: &str) -> Result<String, CliError> {
    if raw.is_empty() {
        return Err(CliError::new("profile must be nonempty"));
    }
    if raw.eq_ignore_ascii_case("production") {
        return Err(CliError::new(
            "reserved production profile is forbidden for debug ctl status",
        ));
    }
    match crate::config::paths::AppProfile::named(raw) {
        Ok(crate::config::paths::AppProfile::Named(name)) => {
            if name == "production" {
                return Err(CliError::new(
                    "reserved production profile is forbidden for debug ctl status",
                ));
            }
            Ok(name)
        }
        Ok(_) => Err(CliError::new(format!("invalid named profile: {raw:?}"))),
        Err(error) => Err(CliError::new(error.to_string())),
    }
}

/// Render the versioned actions document without connecting or touching HostLock.
pub fn actions_json_document() -> Result<String, CliError> {
    action::require_unique_ids().map_err(CliError::new)?;
    let actions: Vec<_> = action::catalog()
        .iter()
        .map(|entry| {
            json!({
                "id": entry.id,
                "title": entry.title,
                "description": entry.description,
                "keywords": entry.keywords,
                "scope": match entry.scope {
                    ActionScope::Host => "host",
                },
                "required_capability": entry.required_capability.map(capability_name),
                "risk": match entry.risk {
                    ActionRisk::ReadOnly => "read_only",
                },
            })
        })
        .collect();
    let doc = json!({
        "schema_version": SCHEMA_VERSION,
        "actions": actions,
    });
    // Compact JSON keeps the offline catalog byte-stable across platforms.
    serde_json::to_string(&doc)
        .map_err(|error| CliError::new(format!("failed to encode actions JSON: {error}")))
}

fn capability_name(capability: crate::protocol::Capability) -> &'static str {
    use crate::protocol::Capability::*;
    match capability {
        PagedSnapshots => "paged_snapshots",
        EventReplay => "event_replay",
        OperationSettlement => "operation_settlement",
        ChunkResume => "chunk_resume",
        GenericExtensions => "generic_extensions",
        SemanticConversation => "semantic_conversation",
        TerminalDeltas => "terminal_deltas",
        BrowserProjection => "browser_projection",
        PromptProjection => "prompt_projection",
        ConnectEncryption => "connect_encryption",
        Guests => "guests",
        ManagementMetadata => "management_metadata",
    }
}

/// Execute a parsed ctl command. Writes JSON to stdout on success.
pub fn run_ctl(command: CtlCommand) -> Result<(), CliError> {
    match command {
        CtlCommand::Actions => {
            let document = actions_json_document()?;
            write_stdout(&document)
        }
        CtlCommand::Status { profile } => {
            let document = status_json_document(&profile)?;
            write_stdout(&document)
        }
    }
}

fn status_json_document(profile: &str) -> Result<String, CliError> {
    #[cfg(not(windows))]
    {
        let _ = profile;
        return Err(CliError::new("ctl status requires Windows"));
    }

    #[cfg(windows)]
    {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|error| CliError::new(format!("failed to build ctl runtime: {error}")))?;
        runtime.block_on(status_json_document_async(profile))
    }
}

#[cfg(windows)]
async fn status_json_document_async(profile: &str) -> Result<String, CliError> {
    let config = HostClientConfig {
        named_profile: profile.to_string(),
        client_build: format!("devmanager-host-ctl/{}", env!("CARGO_PKG_VERSION")),
        client_id: ClientId::new(),
        requested: CapabilitySet::empty(),
        limits: FrameLimits::v1_default(),
    };

    let deadline = tokio::time::Instant::now() + STATUS_CONNECT_TIMEOUT;
    let client = loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return Err(CliError::new(
                "host status connect timed out; is a foreground host running for this profile?",
            ));
        }
        match tokio::time::timeout(remaining, HostClient::connect(config.clone())).await {
            Ok(Ok(client)) => break client,
            Ok(Err(error)) if is_retryable_connect_error(&error) => {
                let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
                if remaining.is_zero() {
                    return Err(map_connect_error(error));
                }
                tokio::time::sleep(STATUS_CONNECT_POLL.min(remaining)).await;
            }
            Ok(Err(error)) => return Err(map_connect_error(error)),
            Err(_) => {
                return Err(CliError::new(
                    "host status connect timed out; is a foreground host running for this profile?",
                ));
            }
        }
    };

    let doc = json!({
        "schema_version": SCHEMA_VERSION,
        "action_id": ACTION_HOST_STATUS,
        "profile": profile,
        "host_boot_id": client.host_boot_id(),
        "connection_id": client.connection_id(),
        "granted_capabilities": client.granted_capabilities().bits(),
        "server_build": client.server_build(),
        "protocol_major": client.protocol_major(),
        "protocol_minor": client.protocol_minor(),
    });
    serde_json::to_string(&doc)
        .map_err(|error| CliError::new(format!("failed to encode status JSON: {error}")))
}

fn is_retryable_connect_error(error: &IpcError) -> bool {
    matches!(
        error,
        IpcError::Unavailable | IpcError::Io(_) | IpcError::Timeout
    )
}

fn map_connect_error(error: IpcError) -> CliError {
    match error {
        IpcError::Unavailable | IpcError::Io(_) | IpcError::Timeout => {
            CliError::new(format!("host unavailable for status attach: {error}"))
        }
        IpcError::InvalidProfile(name) => CliError::new(format!("invalid named profile: {name:?}")),
        other => CliError::new(format!("host status attach failed: {other}")),
    }
}

fn write_stdout(document: &str) -> Result<(), CliError> {
    let mut out = io::stdout().lock();
    out.write_all(document.as_bytes())
        .and_then(|_| out.write_all(b"\n"))
        .map_err(|error| CliError::new(format!("failed to write JSON to stdout: {error}")))
}

/// Binary entry helper: parse args after `ctl`, run, map errors to exit codes.
pub fn dispatch_ctl_from_args<I, S>(args: I) -> ExitCode
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    match parse_ctl_args(args).and_then(run_ctl) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            let _ = writeln!(io::stderr(), "devmanager-host: {error}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{parse_ctl_args, CliError, CtlCommand, MAX_DIAGNOSTIC_CHARS};

    #[test]
    fn parses_actions_and_status() {
        assert_eq!(
            parse_ctl_args(["actions", "--json"]).expect("actions"),
            CtlCommand::Actions
        );
        assert_eq!(
            parse_ctl_args(["status", "--profile", "Alpha_1", "--json"]).expect("status"),
            CtlCommand::Status {
                profile: "alpha_1".to_string()
            }
        );
    }

    #[test]
    fn rejects_unknown_and_duplicates() {
        assert!(parse_ctl_args(["nope", "--json"]).is_err());
        assert!(parse_ctl_args(["actions"]).is_err());
        assert!(parse_ctl_args(["actions", "--json", "--json"]).is_err());
        assert!(parse_ctl_args(["status", "--json"]).is_err());
        assert!(parse_ctl_args(["status", "--profile", "production", "--json"]).is_err());
        assert!(parse_ctl_args(["status", "--profile", "a", "--profile", "b", "--json"]).is_err());
    }

    #[test]
    fn diagnostics_are_bounded() {
        let error = CliError::new("x".repeat(MAX_DIAGNOSTIC_CHARS * 2));
        assert_eq!(error.message().chars().count(), MAX_DIAGNOSTIC_CHARS);
        assert!(error.message().ends_with('\u{2026}'));
    }
}
