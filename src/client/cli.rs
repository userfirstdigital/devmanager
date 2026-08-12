//! Strict `devmanager-host ctl` parsing and versioned JSON output.

use std::collections::HashSet;
use std::io::{self, Write};
use std::process::ExitCode;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::json;

use super::action::{
    self, task_create_v2_command, task_rename_command, ActionArgumentSchema, ActionRisk,
    ActionScope, TaskCreateV2Arguments, TaskRenameArguments, ACTION_HOST_STATUS,
    ACTION_TASK_CREATE, ACTION_TASK_CREATE_V2, ACTION_TASK_LIST, ACTION_TASK_RENAME,
    ACTION_TASK_SHOW,
};
use super::{HostClient, HostClientConfig};
use crate::domain::command::{CommandEnvelope, CommandReceipt, RejectionCode};
use crate::domain::id::SnapshotId;
use crate::domain::query::QueryError;
use crate::domain::snapshot::{SnapshotItem, SnapshotSection, TaskSnapshotItem};
use crate::domain::{ClientId, CommandId, TaskId};
use crate::host::IpcError;
use crate::protocol::{Capability, CapabilitySet, FrameLimits};

const SCHEMA_VERSION: u16 = 1;
const STATUS_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const STATUS_CONNECT_POLL: Duration = Duration::from_millis(25);
const COMMAND_REPLAY_TIMEOUT: Duration = Duration::from_secs(16);
const MAX_COMMAND_ATTEMPTS: usize = 2;
const MAX_DIAGNOSTIC_CHARS: usize = 1_024;
const MAX_ARGUMENTS_JSON_BYTES: usize = 64 * 1024;
const MAX_TASK_LIST_PAGES: usize = 1_024;
const MAX_TASK_LIST_ITEMS: usize = 100_000;

/// Parsed ctl invocation for this lean slice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CtlCommand {
    Actions,
    Status {
        profile: String,
    },
    Tasks {
        profile: String,
    },
    TaskShow {
        profile: String,
        task_id: TaskId,
    },
    Invoke {
        profile: String,
        action_id: String,
        arguments_json: String,
        expected_task_revision: Option<u64>,
    },
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
        "tasks" => parse_tasks(args),
        "task-show" => parse_task_show(args),
        "invoke" => parse_invoke(args),
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

fn parse_tasks<I>(mut args: I) -> Result<CtlCommand, CliError>
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
    Ok(CtlCommand::Tasks { profile })
}

fn parse_task_show<I>(mut args: I) -> Result<CtlCommand, CliError>
where
    I: Iterator<Item = String>,
{
    let mut json = false;
    let mut profile: Option<String> = None;
    let mut task_id: Option<TaskId> = None;
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
            "--task-id" => {
                if task_id.is_some() {
                    return Err(CliError::new("duplicate --task-id"));
                }
                let value = args
                    .next()
                    .ok_or_else(|| CliError::new("missing value for --task-id"))?;
                task_id = Some(
                    TaskId::parse(&value)
                        .map_err(|error| CliError::new(format!("invalid --task-id: {error}")))?,
                );
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
    let task_id = task_id.ok_or_else(|| CliError::new("missing required --task-id"))?;
    Ok(CtlCommand::TaskShow { profile, task_id })
}

fn parse_invoke<I>(mut args: I) -> Result<CtlCommand, CliError>
where
    I: Iterator<Item = String>,
{
    let mut json = false;
    let mut profile: Option<String> = None;
    let mut action_id: Option<String> = None;
    let mut arguments_json: Option<String> = None;
    let mut expected_task_revision: Option<u64> = None;
    let mut saw_expected_task_revision = false;
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
            "--action" => {
                if action_id.is_some() {
                    return Err(CliError::new("duplicate --action"));
                }
                let value = args
                    .next()
                    .ok_or_else(|| CliError::new("missing value for --action"))?;
                if value.is_empty() {
                    return Err(CliError::new("action id must be nonempty"));
                }
                action_id = Some(value);
            }
            "--arguments-json" => {
                if arguments_json.is_some() {
                    return Err(CliError::new("duplicate --arguments-json"));
                }
                let value = args
                    .next()
                    .ok_or_else(|| CliError::new("missing value for --arguments-json"))?;
                if value.len() > MAX_ARGUMENTS_JSON_BYTES {
                    return Err(CliError::new("arguments JSON exceeds maximum size"));
                }
                arguments_json = Some(value);
            }
            "--expected-task-revision" => {
                if saw_expected_task_revision {
                    return Err(CliError::new("duplicate --expected-task-revision"));
                }
                saw_expected_task_revision = true;
                let value = args
                    .next()
                    .ok_or_else(|| CliError::new("missing value for --expected-task-revision"))?;
                let parsed = value.parse::<u64>().map_err(|_| {
                    CliError::new(format!("invalid --expected-task-revision: {value}"))
                })?;
                expected_task_revision = Some(parsed);
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
    let action_id = action_id.ok_or_else(|| CliError::new("missing required --action"))?;
    let arguments_json =
        arguments_json.ok_or_else(|| CliError::new("missing required --arguments-json"))?;
    Ok(CtlCommand::Invoke {
        profile,
        action_id,
        arguments_json,
        expected_task_revision,
    })
}

fn validate_ctl_profile(raw: &str) -> Result<String, CliError> {
    if raw.is_empty() {
        return Err(CliError::new("profile must be nonempty"));
    }
    if raw.eq_ignore_ascii_case("production") {
        return Err(CliError::new(
            "reserved production profile is forbidden for debug ctl commands",
        ));
    }
    match crate::config::paths::AppProfile::named(raw) {
        Ok(crate::config::paths::AppProfile::Named(name)) => {
            if name == "production" {
                return Err(CliError::new(
                    "reserved production profile is forbidden for debug ctl commands",
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
            // Offline catalog has no Hello grant. Emit disabled_reason on every
            // row so omission cannot be read as enabled.
            let reason = action::disabled_reason(entry.id, CapabilitySet::empty());
            json!({
                "id": entry.id,
                "title": entry.title,
                "description": entry.description,
                "keywords": entry.keywords,
                "scope": match entry.scope {
                    ActionScope::Host => "host",
                    ActionScope::Task => "task",
                },
                "required_capability": entry.required_capability.map(capability_name),
                "risk": match entry.risk {
                    ActionRisk::ReadOnly => "read_only",
                    ActionRisk::Mutating => "mutating",
                },
                "argument_schema": argument_schema_json(entry.argument_schema),
                "enabled": reason.is_none(),
                "disabled_reason": reason,
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

fn argument_schema_json(schema: ActionArgumentSchema) -> serde_json::Value {
    let uuid = || json!({ "type": "string", "format": "uuid" });
    match schema {
        ActionArgumentSchema::None => json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {},
            "required": [],
        }),
        ActionArgumentSchema::TaskId => json!({
            "type": "object",
            "additionalProperties": false,
            "properties": { "task_id": uuid() },
            "required": ["task_id"],
        }),
        ActionArgumentSchema::TaskCreateV1 => json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "task_id": uuid(),
                "environment_id": uuid(),
                "title": { "type": "string", "minLength": 1 },
                "description": { "type": ["string", "null"] },
                "project_id": uuid(),
                "workspace": {
                    "oneOf": [
                        { "const": "main" },
                        {
                            "type": "object",
                            "additionalProperties": false,
                            "properties": {
                                "worktree": {
                                    "type": "object",
                                    "additionalProperties": false,
                                    "properties": {
                                        "path": { "type": "string", "minLength": 1 },
                                        "branch": { "type": "string", "minLength": 1 }
                                    },
                                    "required": ["path", "branch"]
                                }
                            },
                            "required": ["worktree"]
                        },
                        {
                            "type": "object",
                            "additionalProperties": false,
                            "properties": {
                                "external": {
                                    "type": "object",
                                    "additionalProperties": false,
                                    "properties": {
                                        "path": { "type": "string", "minLength": 1 }
                                    },
                                    "required": ["path"]
                                }
                            },
                            "required": ["external"]
                        }
                    ]
                }
            },
            "required": ["task_id", "environment_id", "title", "project_id", "workspace"],
        }),
        ActionArgumentSchema::TaskCreateV2 => json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "task_id": uuid(),
                "environment_id": uuid(),
                "title": { "type": "string", "minLength": 1 },
                "description": { "type": ["string", "null"] },
                "project_id": uuid(),
                "workspace": {
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {
                        "choice": {
                            "type": "string",
                            "enum": ["main", "new_worktree", "ask", "external"]
                        },
                        "path": { "type": ["string", "null"] },
                        "branch": { "type": ["string", "null"] },
                        "external_confirmed": { "type": "boolean" }
                    },
                    "required": ["choice", "path", "branch", "external_confirmed"]
                }
            },
            "required": [
                "task_id",
                "environment_id",
                "title",
                "project_id",
                "workspace"
            ],
        }),
        ActionArgumentSchema::TaskRenameV1 => json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "task_id": uuid(),
                "title": { "type": "string", "minLength": 1 },
            },
            "required": ["task_id", "title"],
        }),
        ActionArgumentSchema::PromptMetadataPageV1 => json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "namespace": { "enum": ["personal"] },
                "cursor": { "type": ["string", "null"] },
                "expected_revision": { "type": ["integer", "null"], "minimum": 0 }
            },
            "required": ["namespace"],
        }),
        ActionArgumentSchema::PromptVersionPageV1 => json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "version_id": uuid(),
                "cursor": { "type": ["string", "null"] }
            },
            "required": ["version_id"],
        }),
        ActionArgumentSchema::PromptDiffV1 => json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "old_version_id": uuid(),
                "new_version_id": uuid(),
                "cursor": { "type": ["string", "null"] }
            },
            "required": ["old_version_id", "new_version_id"],
        }),
        ActionArgumentSchema::PromptChainPageV1 => json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "chain_id": uuid(),
                "cursor": { "type": ["string", "null"] },
                "expected_revision": { "type": ["integer", "null"], "minimum": 0 }
            },
            "required": ["chain_id"],
        }),
    }
}

fn capability_name(capability: crate::protocol::Capability) -> &'static str {
    capability.wire_name()
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
        CtlCommand::Tasks { profile } => {
            let document = tasks_json_document(&profile)?;
            write_stdout(&document)
        }
        CtlCommand::TaskShow { profile, task_id } => {
            let document = task_show_json_document(&profile, task_id)?;
            write_stdout(&document)
        }
        CtlCommand::Invoke {
            profile,
            action_id,
            arguments_json,
            expected_task_revision,
        } => {
            let document = invoke_json_document(
                &profile,
                &action_id,
                &arguments_json,
                expected_task_revision,
            )?;
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
    let client = connect_profile_client(profile, ClientId::new(), CapabilitySet::empty()).await?;

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

fn tasks_json_document(profile: &str) -> Result<String, CliError> {
    #[cfg(not(windows))]
    {
        let _ = profile;
        return Err(CliError::new("ctl tasks requires Windows"));
    }

    #[cfg(windows)]
    {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|error| CliError::new(format!("failed to build ctl runtime: {error}")))?;
        runtime.block_on(tasks_json_document_async(profile))
    }
}

#[cfg(windows)]
async fn tasks_json_document_async(profile: &str) -> Result<String, CliError> {
    let mut client = connect_profile_client(
        profile,
        ClientId::new(),
        CapabilitySet::from_capabilities([Capability::PagedSnapshots]),
    )
    .await?;
    if !client
        .granted_capabilities()
        .contains(Capability::PagedSnapshots)
    {
        return Err(CliError::new(
            "host did not grant required paged_snapshots capability",
        ));
    }

    let mut opened: Option<SnapshotId> = None;
    let result = assemble_task_list(&mut client, profile, &mut opened).await;
    if let Some(snapshot_id) = opened {
        let _ = client.release_snapshot(snapshot_id).await;
    }
    result
}

#[cfg(windows)]
async fn assemble_task_list(
    client: &mut HostClient,
    profile: &str,
    opened: &mut Option<SnapshotId>,
) -> Result<String, CliError> {
    let mut tasks: Vec<TaskSnapshotItem> = Vec::new();
    let mut seen_cursors: HashSet<Vec<u8>> = HashSet::new();
    let mut page_count: u64 = 0;
    let mut snapshot_id: Option<SnapshotId> = None;
    let mut through_sequence: Option<u64> = None;
    let mut resume_cursor: Option<Vec<u8>> = None;

    loop {
        if page_count as usize >= MAX_TASK_LIST_PAGES {
            return Err(CliError::new("task.list exceeded finite page bound"));
        }
        let requested_id = snapshot_id;
        let page = match client
            .snapshot_page(SnapshotSection::Tasks, requested_id, resume_cursor.clone())
            .await
        {
            Ok(Ok(page)) => page,
            Ok(Err(QueryError::NotFound)) => {
                return Err(CliError::new("task.list snapshot was not found"))
            }
            Ok(Err(QueryError::Unauthorized)) => {
                return Err(CliError::new("task.list query was unauthorized"))
            }
            Ok(Err(QueryError::InvalidRequest)) => {
                return Err(CliError::new("task.list query was invalid"))
            }
            Ok(Err(QueryError::UnsupportedCapability)) => {
                return Err(CliError::new("task.list query capability is unsupported"))
            }
            Ok(Err(QueryError::ReplayUnavailable { .. })) => {
                return Err(CliError::new("task.list query replay is unavailable"))
            }
            Ok(Err(QueryError::Unavailable { reason })) => {
                return Err(CliError::new(format!(
                    "task.list query is unavailable: {reason}"
                )))
            }
            Err(error) => return Err(CliError::new(format!("task.list query failed: {error}"))),
        };

        *opened = Some(page.snapshot_id);
        if let Some(expected) = snapshot_id {
            if page.snapshot_id != expected {
                return Err(CliError::new(
                    "task.list snapshot identity drifted across pages",
                ));
            }
        } else {
            snapshot_id = Some(page.snapshot_id);
        }
        if let Some(expected) = through_sequence {
            if page.through_sequence != expected {
                return Err(CliError::new(
                    "task.list through_sequence drifted across pages",
                ));
            }
        } else {
            through_sequence = Some(page.through_sequence);
        }
        if page.section != SnapshotSection::Tasks {
            return Err(CliError::new("task.list returned a non-tasks section"));
        }

        for item in &page.items {
            let SnapshotItem::Task(task_item) = item else {
                return Err(CliError::new(
                    "task.list page contained a non-task snapshot item",
                ));
            };
            if tasks.len() >= MAX_TASK_LIST_ITEMS {
                return Err(CliError::new("task.list exceeded finite item bound"));
            }
            tasks.push(task_item.clone());
        }
        page_count += 1;

        match page.next_cursor {
            Some(cursor) => {
                if !seen_cursors.insert(cursor.clone()) {
                    return Err(CliError::new(
                        "task.list observed a repeated snapshot cursor",
                    ));
                }
                resume_cursor = Some(cursor);
            }
            None => break,
        }
    }

    let snapshot_id = snapshot_id.ok_or_else(|| CliError::new("task.list produced no pages"))?;
    let through_sequence =
        through_sequence.ok_or_else(|| CliError::new("task.list produced no pages"))?;

    let doc = json!({
        "schema_version": SCHEMA_VERSION,
        "action_id": ACTION_TASK_LIST,
        "profile": profile,
        "snapshot_id": snapshot_id,
        "through_sequence": through_sequence,
        "page_count": page_count,
        "tasks": tasks,
    });
    serde_json::to_string(&doc)
        .map_err(|error| CliError::new(format!("failed to encode tasks JSON: {error}")))
}

fn task_show_json_document(profile: &str, task_id: TaskId) -> Result<String, CliError> {
    #[cfg(not(windows))]
    {
        let _ = profile;
        let _ = task_id;
        return Err(CliError::new("ctl task-show requires Windows"));
    }

    #[cfg(windows)]
    {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|error| CliError::new(format!("failed to build ctl runtime: {error}")))?;
        runtime.block_on(task_show_json_document_async(profile, task_id))
    }
}

#[cfg(windows)]
async fn task_show_json_document_async(profile: &str, task_id: TaskId) -> Result<String, CliError> {
    let mut client =
        connect_profile_client(profile, ClientId::new(), CapabilitySet::empty()).await?;
    let snapshot = match client.task_snapshot(task_id).await {
        Ok(Ok(snapshot)) => snapshot,
        Ok(Err(QueryError::NotFound)) => {
            return Err(CliError::new(format!("task {task_id} not found")))
        }
        Ok(Err(QueryError::Unauthorized)) => {
            return Err(CliError::new("task.show query was unauthorized"))
        }
        Ok(Err(QueryError::InvalidRequest)) => {
            return Err(CliError::new("task.show query was invalid"))
        }
        Ok(Err(QueryError::UnsupportedCapability)) => {
            return Err(CliError::new("task.show query capability is unsupported"))
        }
        Ok(Err(QueryError::ReplayUnavailable { .. })) => {
            return Err(CliError::new("task.show query replay is unavailable"))
        }
        Ok(Err(QueryError::Unavailable { reason })) => {
            return Err(CliError::new(format!(
                "task.show query is unavailable: {reason}"
            )))
        }
        Err(error) => return Err(CliError::new(format!("task.show query failed: {error}"))),
    };
    let doc = json!({
        "schema_version": SCHEMA_VERSION,
        "action_id": ACTION_TASK_SHOW,
        "profile": profile,
        "task_id": task_id,
        "snapshot": snapshot,
    });
    serde_json::to_string(&doc)
        .map_err(|error| CliError::new(format!("failed to encode task-show JSON: {error}")))
}

fn invoke_json_document(
    profile: &str,
    action_id: &str,
    arguments_json: &str,
    expected_task_revision: Option<u64>,
) -> Result<String, CliError> {
    match action_id {
        ACTION_TASK_CREATE_V2 => {
            if expected_task_revision.is_some() {
                return Err(CliError::new(
                    "task creation requires expected-task-revision to be absent",
                ));
            }
            #[cfg(not(windows))]
            {
                let _ = profile;
                let _ = arguments_json;
                return Err(CliError::new("ctl invoke requires Windows"));
            }
            #[cfg(windows)]
            {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map_err(|error| {
                        CliError::new(format!("failed to build ctl runtime: {error}"))
                    })?;
                runtime.block_on(task_create_invoke_async(profile, action_id, arguments_json))
            }
        }
        ACTION_TASK_CREATE => Err(CliError::new(
            "task.create is a frozen V1 codec and is not an advertised public action; use task.create.v2",
        )),
        ACTION_TASK_RENAME => {
            let Some(expected_task_revision) = expected_task_revision else {
                return Err(CliError::new("task.rename requires expected-task-revision"));
            };
            #[cfg(not(windows))]
            {
                let _ = profile;
                let _ = arguments_json;
                let _ = expected_task_revision;
                return Err(CliError::new("ctl invoke requires Windows"));
            }
            #[cfg(windows)]
            {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map_err(|error| {
                        CliError::new(format!("failed to build ctl runtime: {error}"))
                    })?;
                runtime.block_on(task_rename_invoke_async(
                    profile,
                    arguments_json,
                    expected_task_revision,
                ))
            }
        }
        other => {
            if let Some(reason) = action::disabled_reason(other, CapabilitySet::empty()) {
                return Err(CliError::new(reason.to_string()));
            }
            Err(CliError::new(format!("unsupported action id: {other}")))
        }
    }
}

#[cfg(windows)]
async fn task_create_invoke_async(
    profile: &str,
    action_id: &str,
    arguments_json: &str,
) -> Result<String, CliError> {
    let args: TaskCreateV2Arguments = serde_json::from_str(arguments_json).map_err(|error| {
        CliError::new(format!("invalid task.create.v2 arguments JSON: {error}"))
    })?;
    let task_id = args.task_id;
    let envelope =
        task_create_v2_command(CommandId::new(), ClientId::new(), unix_epoch_ms()?, args)
            .map_err(|error| CliError::new(format!("invalid task.create.v2 arguments: {error}")))?;
    let client_id = ClientId::new();
    let envelope = CommandEnvelope {
        client_id,
        ..envelope
    };

    let mut client = connect_profile_client(profile, client_id, CapabilitySet::empty()).await?;
    let receipt = execute_command_with_reconnect(&mut client, envelope, action_id).await?;
    match &receipt {
        CommandReceipt::Accepted { .. } => {
            let doc = json!({
                "schema_version": SCHEMA_VERSION,
                "action_id": action_id,
                "profile": profile,
                "task_id": task_id,
                "receipt": receipt,
            });
            serde_json::to_string(&doc)
                .map_err(|error| CliError::new(format!("failed to encode invoke JSON: {error}")))
        }
        CommandReceipt::Rejected { code, .. } => Err(CliError::new(format!(
            "{action_id} rejected: {}",
            rejection_code_name(*code)
        ))),
    }
}

#[cfg(windows)]
async fn task_rename_invoke_async(
    profile: &str,
    arguments_json: &str,
    expected_task_revision: u64,
) -> Result<String, CliError> {
    let args: TaskRenameArguments = serde_json::from_str(arguments_json)
        .map_err(|error| CliError::new(format!("invalid task.rename arguments JSON: {error}")))?;
    let task_id = args.task_id;
    let client_id = ClientId::new();
    let command_id = CommandId::new();
    let issued_at_ms = unix_epoch_ms()?;
    let envelope = task_rename_command(
        command_id,
        client_id,
        issued_at_ms,
        expected_task_revision,
        args,
    )
    .map_err(|error| CliError::new(format!("invalid task.rename arguments: {error}")))?;

    let mut client = connect_profile_client(profile, client_id, CapabilitySet::empty()).await?;
    let receipt = execute_command_with_reconnect(&mut client, envelope, ACTION_TASK_RENAME).await?;
    match &receipt {
        CommandReceipt::Accepted { .. } => {
            let doc = json!({
                "schema_version": SCHEMA_VERSION,
                "action_id": ACTION_TASK_RENAME,
                "profile": profile,
                "task_id": task_id,
                "receipt": receipt,
            });
            serde_json::to_string(&doc)
                .map_err(|error| CliError::new(format!("failed to encode invoke JSON: {error}")))
        }
        CommandReceipt::Rejected { code, .. } => Err(CliError::new(format!(
            "task.rename rejected: {}",
            rejection_code_name(*code)
        ))),
    }
}

fn rejection_code_name(code: RejectionCode) -> &'static str {
    match code {
        RejectionCode::NotFound => "not_found",
        RejectionCode::AlreadyExists => "already_exists",
        RejectionCode::RevisionConflict => "revision_conflict",
        RejectionCode::InvalidTransition => "invalid_transition",
        RejectionCode::OwnershipConflict => "ownership_conflict",
        RejectionCode::UnsupportedCapability => "unsupported_capability",
        RejectionCode::Closing => "closing",
    }
}

fn unix_epoch_ms() -> Result<i64, CliError> {
    let duration = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| CliError::new(format!("system clock precedes Unix epoch: {error}")))?;
    i64::try_from(duration.as_millis())
        .map_err(|_| CliError::new("system clock milliseconds exceed supported range"))
}

#[cfg(windows)]
async fn execute_command_with_reconnect(
    client: &mut HostClient,
    envelope: CommandEnvelope,
    action_id: &str,
) -> Result<CommandReceipt, CliError> {
    let deadline = tokio::time::Instant::now() + COMMAND_REPLAY_TIMEOUT;
    for attempt in 0..MAX_COMMAND_ATTEMPTS {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            client.disconnect();
            return Err(command_replay_timeout_error(action_id));
        }
        let outcome =
            tokio::time::timeout(remaining, client.execute_command(envelope.clone())).await;
        match outcome {
            Ok(Ok(receipt)) => return Ok(receipt),
            Ok(Err(error))
                if is_retryable_connect_error(&error) && attempt + 1 < MAX_COMMAND_ATTEMPTS =>
            {
                reconnect_before_deadline(client, deadline, action_id).await?;
            }
            Ok(Err(error)) => {
                return Err(CliError::new(format!(
                    "{action_id} command failed: {error}"
                )))
            }
            Err(_) => {
                client.disconnect();
                return Err(command_replay_timeout_error(action_id));
            }
        }
    }
    Err(CliError::new(format!(
        "{action_id} command exhausted its bounded replay attempts"
    )))
}

#[cfg(windows)]
async fn reconnect_before_deadline(
    client: &mut HostClient,
    deadline: tokio::time::Instant,
    action_id: &str,
) -> Result<(), CliError> {
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            client.disconnect();
            return Err(command_replay_timeout_error(action_id));
        }
        match tokio::time::timeout(remaining, client.reconnect()).await {
            Ok(Ok(())) => return Ok(()),
            Ok(Err(error)) if is_retryable_connect_error(&error) => {
                let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
                if remaining.is_zero() {
                    client.disconnect();
                    return Err(command_replay_timeout_error(action_id));
                }
                tokio::time::sleep(STATUS_CONNECT_POLL.min(remaining)).await;
            }
            Ok(Err(error)) => return Err(map_connect_error(error)),
            Err(_) => {
                client.disconnect();
                return Err(command_replay_timeout_error(action_id));
            }
        }
    }
}

fn command_replay_timeout_error(action_id: &str) -> CliError {
    CliError::new(format!(
        "{action_id} command exceeded its bounded reconnect/replay window"
    ))
}

#[cfg(windows)]
async fn connect_profile_client(
    profile: &str,
    client_id: ClientId,
    requested: CapabilitySet,
) -> Result<HostClient, CliError> {
    let config = HostClientConfig {
        named_profile: profile.to_string(),
        client_build: format!("devmanager-host-ctl/{}", env!("CARGO_PKG_VERSION")),
        client_id,
        requested,
        limits: FrameLimits::v1_default(),
    };

    let deadline = tokio::time::Instant::now() + STATUS_CONNECT_TIMEOUT;
    let client = loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return Err(CliError::new(
                "host connect timed out; is a foreground host running for this profile?",
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
                    "host connect timed out; is a foreground host running for this profile?",
                ));
            }
        }
    };
    Ok(client)
}

fn is_retryable_connect_error(error: &IpcError) -> bool {
    matches!(
        error,
        IpcError::Unavailable | IpcError::Io(_) | IpcError::Timeout | IpcError::ConnectionPoisoned
    )
}

fn map_connect_error(error: IpcError) -> CliError {
    match error {
        IpcError::Unavailable | IpcError::Io(_) | IpcError::Timeout => {
            CliError::new(format!("host unavailable for ctl attach: {error}"))
        }
        IpcError::InvalidProfile(name) => CliError::new(format!("invalid named profile: {name:?}")),
        other => CliError::new(format!("host ctl attach failed: {other}")),
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
    use super::{
        parse_ctl_args, CliError, CtlCommand, COMMAND_REPLAY_TIMEOUT, MAX_ARGUMENTS_JSON_BYTES,
        MAX_COMMAND_ATTEMPTS, MAX_DIAGNOSTIC_CHARS, STATUS_CONNECT_TIMEOUT,
    };
    use crate::client::action::ACTION_TASK_CREATE;
    use crate::domain::TaskId;

    #[test]
    fn parses_actions_status_and_task_show() {
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
        assert_eq!(
            parse_ctl_args(["tasks", "--profile", "Alpha_1", "--json"]).expect("tasks"),
            CtlCommand::Tasks {
                profile: "alpha_1".to_string()
            }
        );
        let task_id = TaskId::new();
        assert_eq!(
            parse_ctl_args([
                "task-show".to_string(),
                "--profile".to_string(),
                "Alpha_1".to_string(),
                "--task-id".to_string(),
                task_id.to_string(),
                "--json".to_string(),
            ])
            .expect("task-show"),
            CtlCommand::TaskShow {
                profile: "alpha_1".to_string(),
                task_id,
            }
        );
    }

    #[test]
    fn parses_invoke_task_create_without_expected_revision() {
        let arguments = r#"{"title":"CLI Created Task"}"#;
        assert_eq!(
            parse_ctl_args([
                "invoke",
                "--profile",
                "Alpha_1",
                "--action",
                ACTION_TASK_CREATE,
                "--arguments-json",
                arguments,
                "--json",
            ])
            .expect("invoke"),
            CtlCommand::Invoke {
                profile: "alpha_1".to_string(),
                action_id: ACTION_TASK_CREATE.to_string(),
                arguments_json: arguments.to_string(),
                expected_task_revision: None,
            }
        );
        assert_eq!(
            parse_ctl_args([
                "invoke",
                "--profile",
                "Alpha_1",
                "--action",
                ACTION_TASK_CREATE,
                "--arguments-json",
                arguments,
                "--expected-task-revision",
                "1",
                "--json",
            ])
            .expect("invoke with revision"),
            CtlCommand::Invoke {
                profile: "alpha_1".to_string(),
                action_id: ACTION_TASK_CREATE.to_string(),
                arguments_json: arguments.to_string(),
                expected_task_revision: Some(1),
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
        assert!(parse_ctl_args([
            "task-show",
            "--profile",
            "valid",
            "--task-id",
            "not-a-uuid",
            "--json"
        ])
        .is_err());
        assert!(parse_ctl_args(["task-show", "--profile", "valid", "--json"]).is_err());
        let task_id = TaskId::new().to_string();
        assert!(parse_ctl_args([
            "task-show".to_string(),
            "--profile".to_string(),
            "valid".to_string(),
            "--task-id".to_string(),
            task_id.clone(),
            "--task-id".to_string(),
            task_id,
            "--json".to_string(),
        ])
        .is_err());
        assert!(parse_ctl_args([
            "invoke",
            "--profile",
            "valid",
            "--action",
            ACTION_TASK_CREATE,
            "--json"
        ])
        .is_err());
        assert!(parse_ctl_args([
            "invoke",
            "--profile",
            "valid",
            "--action",
            ACTION_TASK_CREATE,
            "--arguments-json",
            "{}",
            "--arguments-json",
            "{}",
            "--json"
        ])
        .is_err());
        let oversized = "x".repeat(MAX_ARGUMENTS_JSON_BYTES + 1);
        assert!(parse_ctl_args([
            "invoke",
            "--profile",
            "valid",
            "--action",
            ACTION_TASK_CREATE,
            "--arguments-json",
            &oversized,
            "--json"
        ])
        .is_err());
    }

    #[test]
    fn diagnostics_are_bounded() {
        let error = CliError::new("x".repeat(MAX_DIAGNOSTIC_CHARS * 2));
        assert_eq!(error.message().chars().count(), MAX_DIAGNOSTIC_CHARS);
        assert!(error.message().ends_with('\u{2026}'));
    }

    #[test]
    fn command_replay_budget_allows_one_full_replay_after_timeout() {
        assert_eq!(MAX_COMMAND_ATTEMPTS, 2);
        assert!(COMMAND_REPLAY_TIMEOUT > STATUS_CONNECT_TIMEOUT * 3);
    }
}
