use serde::de::{self, MapAccess, Visitor};
use serde::ser::SerializeMap;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::domain::agent::AgentSessionFacts;
use crate::domain::artifact::ArtifactFacts;
use crate::domain::event::Event;
use crate::domain::id::{
    AgentSessionId, ClientId, CommandId, EnvironmentId, EventId, OperationId, ProjectId,
    ResourceId, TaskId,
};
use crate::domain::resource::ResourceFacts;
use crate::domain::snapshot::TaskSnapshot;
use crate::domain::task::{
    ReviewReadiness, TaskActivity, TaskAssignment, TaskAttention, TaskConnectivity, TaskFacts,
    TaskLifecycle, WorkspaceRef,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RejectionCode {
    NotFound,
    AlreadyExists,
    RevisionConflict,
    InvalidTransition,
    OwnershipConflict,
    UnsupportedCapability,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandEnvelope {
    pub command_id: CommandId,
    pub client_id: ClientId,
    pub task_id: Option<TaskId>,
    pub issued_at_ms: i64,
    pub expected_task_revision: Option<u64>,
    pub command: Command,
}

impl Serialize for CommandEnvelope {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut map = serializer.serialize_map(Some(6))?;
        map.serialize_entry("command_id", &self.command_id)?;
        map.serialize_entry("client_id", &self.client_id)?;
        map.serialize_entry("task_id", &self.task_id)?;
        map.serialize_entry("issued_at_ms", &self.issued_at_ms)?;
        map.serialize_entry("expected_task_revision", &self.expected_task_revision)?;
        map.serialize_entry("command", &self.command)?;
        map.end()
    }
}

const COMMAND_ENVELOPE_FIELDS: &[&str] = &[
    "command_id",
    "client_id",
    "task_id",
    "issued_at_ms",
    "expected_task_revision",
    "command",
];

enum CommandEnvelopeField {
    CommandId,
    ClientId,
    TaskId,
    IssuedAtMs,
    ExpectedTaskRevision,
    Command,
}

impl<'de> Deserialize<'de> for CommandEnvelopeField {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct FieldVisitor;

        impl Visitor<'_> for FieldVisitor {
            type Value = CommandEnvelopeField;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a CommandEnvelope field name")
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                match value {
                    "command_id" => Ok(CommandEnvelopeField::CommandId),
                    "client_id" => Ok(CommandEnvelopeField::ClientId),
                    "task_id" => Ok(CommandEnvelopeField::TaskId),
                    "issued_at_ms" => Ok(CommandEnvelopeField::IssuedAtMs),
                    "expected_task_revision" => Ok(CommandEnvelopeField::ExpectedTaskRevision),
                    "command" => Ok(CommandEnvelopeField::Command),
                    _ => Err(de::Error::unknown_field(value, COMMAND_ENVELOPE_FIELDS)),
                }
            }
        }

        deserializer.deserialize_identifier(FieldVisitor)
    }
}

impl<'de> Deserialize<'de> for CommandEnvelope {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct CommandEnvelopeVisitor;

        impl<'de> Visitor<'de> for CommandEnvelopeVisitor {
            type Value = CommandEnvelope;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a named CommandEnvelope map")
            }

            fn visit_map<M>(self, mut map: M) -> Result<Self::Value, M::Error>
            where
                M: MapAccess<'de>,
            {
                let mut command_id = None;
                let mut client_id = None;
                let mut task_id: Option<Option<TaskId>> = None;
                let mut issued_at_ms = None;
                let mut expected_task_revision: Option<Option<u64>> = None;
                let mut command = None;

                while let Some(field) = map.next_key()? {
                    match field {
                        CommandEnvelopeField::CommandId => {
                            if command_id.is_some() {
                                return Err(de::Error::duplicate_field("command_id"));
                            }
                            command_id = Some(map.next_value()?);
                        }
                        CommandEnvelopeField::ClientId => {
                            if client_id.is_some() {
                                return Err(de::Error::duplicate_field("client_id"));
                            }
                            client_id = Some(map.next_value()?);
                        }
                        CommandEnvelopeField::TaskId => {
                            if task_id.is_some() {
                                return Err(de::Error::duplicate_field("task_id"));
                            }
                            task_id = Some(map.next_value()?);
                        }
                        CommandEnvelopeField::IssuedAtMs => {
                            if issued_at_ms.is_some() {
                                return Err(de::Error::duplicate_field("issued_at_ms"));
                            }
                            issued_at_ms = Some(map.next_value()?);
                        }
                        CommandEnvelopeField::ExpectedTaskRevision => {
                            if expected_task_revision.is_some() {
                                return Err(de::Error::duplicate_field("expected_task_revision"));
                            }
                            expected_task_revision = Some(map.next_value()?);
                        }
                        CommandEnvelopeField::Command => {
                            if command.is_some() {
                                return Err(de::Error::duplicate_field("command"));
                            }
                            command = Some(map.next_value()?);
                        }
                    }
                }

                Ok(CommandEnvelope {
                    command_id: command_id.ok_or_else(|| de::Error::missing_field("command_id"))?,
                    client_id: client_id.ok_or_else(|| de::Error::missing_field("client_id"))?,
                    task_id: task_id.ok_or_else(|| de::Error::missing_field("task_id"))?,
                    issued_at_ms: issued_at_ms
                        .ok_or_else(|| de::Error::missing_field("issued_at_ms"))?,
                    expected_task_revision: expected_task_revision
                        .ok_or_else(|| de::Error::missing_field("expected_task_revision"))?,
                    command: command.ok_or_else(|| de::Error::missing_field("command"))?,
                })
            }
        }

        deserializer.deserialize_map(CommandEnvelopeVisitor)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum CommandReceipt {
    Accepted {
        command_id: CommandId,
        operation_id: OperationId,
        task_revision: Option<u64>,
        event_ids: Vec<EventId>,
    },
    Rejected {
        command_id: CommandId,
        code: RejectionCode,
        current_revision: Option<u64>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreateTaskIntent {
    pub id: TaskId,
    pub environment_id: EnvironmentId,
    pub title: String,
    pub description: Option<String>,
    pub project_id: ProjectId,
    pub workspace: WorkspaceRef,
    pub assignment: TaskAssignment,
    pub created_at_ms: i64,
    pub connectivity: TaskConnectivity,
    pub attention: TaskAttention,
    pub activity: TaskActivity,
    pub review_readiness: ReviewReadiness,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RenameTaskIntent {
    pub title: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SetTaskAttentionIntent {
    pub attention: TaskAttention,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Command {
    CreateTask(CreateTaskIntent),
    RenameTask(RenameTaskIntent),
    SetTaskAttention(SetTaskAttentionIntent),
    BeginCloseTask,
    ReopenTask,
    RegisterAgentSession { agent: AgentSessionFacts },
    SetPrimaryAgent { agent_session_id: AgentSessionId },
    RegisterArtifact { artifact: ArtifactFacts },
    RegisterResource { resource: ResourceFacts },
    ReleaseResource { resource_id: ResourceId },
}

pub fn decide(
    snapshot: Option<&TaskSnapshot>,
    envelope: &CommandEnvelope,
) -> Result<Vec<Event>, RejectionCode> {
    match &envelope.command {
        Command::CreateTask(intent) => decide_create_task(snapshot, envelope, intent),
        Command::RenameTask(intent) => {
            let snap = require_open_or_closing_task(snapshot, envelope)?;
            require_expected_revision(snap, envelope)?;
            if intent.title.trim().is_empty() {
                return Err(RejectionCode::InvalidTransition);
            }
            let title = TaskFacts::canonicalize_title(intent.title.clone())
                .map_err(|_| RejectionCode::InvalidTransition)?;
            Ok(vec![Event::TaskRenamed { title }])
        }
        Command::SetTaskAttention(intent) => {
            let snap = require_open_or_closing_task(snapshot, envelope)?;
            require_expected_revision(snap, envelope)?;
            Ok(vec![Event::TaskAttentionSet {
                attention: intent.attention,
            }])
        }
        Command::BeginCloseTask => decide_begin_close(snapshot, envelope),
        Command::ReopenTask => {
            let snap = require_task(snapshot, envelope)?;
            require_expected_revision(snap, envelope)?;
            match snap.task.lifecycle {
                TaskLifecycle::Closing | TaskLifecycle::Archived => Ok(vec![Event::TaskReopened]),
                TaskLifecycle::Open => Err(RejectionCode::InvalidTransition),
            }
        }
        Command::RegisterAgentSession { agent } => {
            let snap = require_runtime_capable_task(snapshot, envelope)?;
            require_expected_revision(snap, envelope)?;
            if agent.task_id != snap.task.id {
                return Err(RejectionCode::OwnershipConflict);
            }
            if agent.validate_for_registration().is_err() {
                return Err(RejectionCode::InvalidTransition);
            }
            if snap.agents.contains_key(&agent.id) {
                return Err(RejectionCode::AlreadyExists);
            }
            Ok(vec![Event::AgentSessionRegistered {
                agent: agent.clone(),
            }])
        }
        Command::SetPrimaryAgent { agent_session_id } => {
            let snap = require_runtime_capable_task(snapshot, envelope)?;
            require_expected_revision(snap, envelope)?;
            let Some(agent) = snap.agents.get(agent_session_id) else {
                return Err(RejectionCode::NotFound);
            };
            if !matches!(agent.role, crate::domain::agent::AgentRole::Primary) {
                return Err(RejectionCode::InvalidTransition);
            }
            Ok(vec![Event::PrimaryAgentSet {
                agent_session_id: *agent_session_id,
            }])
        }
        Command::RegisterArtifact { artifact } => {
            let snap = require_open_or_closing_task(snapshot, envelope)?;
            require_expected_revision(snap, envelope)?;
            if artifact.task_id != snap.task.id {
                return Err(RejectionCode::OwnershipConflict);
            }
            if artifact.validate().is_err() {
                return Err(RejectionCode::InvalidTransition);
            }
            if snap.artifacts.contains_key(&artifact.id) {
                return Err(RejectionCode::AlreadyExists);
            }
            Ok(vec![Event::ArtifactRegistered {
                artifact: artifact.clone(),
            }])
        }
        Command::RegisterResource { resource } => {
            let snap = require_runtime_capable_task(snapshot, envelope)?;
            require_expected_revision(snap, envelope)?;
            if resource.owner_kind != crate::domain::resource::OwnerKind::Task {
                return Err(RejectionCode::OwnershipConflict);
            }
            match resource.task_id {
                Some(id) if id == snap.task.id => {}
                _ => return Err(RejectionCode::OwnershipConflict),
            }
            if resource.validate().is_err() {
                return Err(RejectionCode::InvalidTransition);
            }
            if resource.lifecycle != crate::domain::resource::ResourceLifecycle::Active {
                return Err(RejectionCode::InvalidTransition);
            }
            if snap.resources.contains_key(&resource.id) {
                return Err(RejectionCode::AlreadyExists);
            }
            Ok(vec![Event::ResourceRegistered {
                resource: resource.clone(),
            }])
        }
        Command::ReleaseResource { resource_id } => {
            let snap = require_open_or_closing_task(snapshot, envelope)?;
            require_expected_revision(snap, envelope)?;
            let Some(existing) = snap.resources.get(resource_id) else {
                return Err(RejectionCode::NotFound);
            };
            if existing.owner_kind != crate::domain::resource::OwnerKind::Task
                || existing.task_id != Some(snap.task.id)
            {
                return Err(RejectionCode::OwnershipConflict);
            }
            match existing.lifecycle {
                crate::domain::resource::ResourceLifecycle::Active => {
                    Ok(vec![Event::ResourceReleaseBegun {
                        resource_id: *resource_id,
                        runtime_generation: existing.runtime_generation,
                    }])
                }
                crate::domain::resource::ResourceLifecycle::Releasing => Ok(Vec::new()),
                crate::domain::resource::ResourceLifecycle::Released => {
                    Err(RejectionCode::InvalidTransition)
                }
            }
        }
    }
}

fn decide_create_task(
    snapshot: Option<&TaskSnapshot>,
    envelope: &CommandEnvelope,
    intent: &CreateTaskIntent,
) -> Result<Vec<Event>, RejectionCode> {
    if snapshot.is_some() {
        return Err(RejectionCode::AlreadyExists);
    }
    if envelope.task_id.is_some() {
        return Err(RejectionCode::InvalidTransition);
    }
    if envelope.expected_task_revision.is_some() {
        return Err(RejectionCode::RevisionConflict);
    }
    if intent.title.trim().is_empty() {
        return Err(RejectionCode::InvalidTransition);
    }
    let description = match &intent.description {
        Some(value) if value.trim().is_empty() => {
            return Err(RejectionCode::InvalidTransition);
        }
        Some(value) => Some(value.trim().to_string()),
        None => None,
    };
    let task = TaskFacts {
        id: intent.id,
        environment_id: intent.environment_id,
        title: intent.title.trim().to_string(),
        description,
        project_id: intent.project_id,
        workspace: intent.workspace.clone(),
        assignment: intent.assignment.clone(),
        lifecycle: TaskLifecycle::Open,
        action_epoch: 0,
        revision: 1,
        created_at_ms: intent.created_at_ms,
    };
    task.validate_for_create()
        .map_err(|_| RejectionCode::InvalidTransition)?;
    Ok(vec![Event::TaskCreated {
        task,
        connectivity: intent.connectivity,
        attention: intent.attention,
        activity: intent.activity,
        review_readiness: intent.review_readiness,
    }])
}

fn decide_begin_close(
    snapshot: Option<&TaskSnapshot>,
    envelope: &CommandEnvelope,
) -> Result<Vec<Event>, RejectionCode> {
    let snap = require_task(snapshot, envelope)?;
    require_expected_revision(snap, envelope)?;
    match snap.task.lifecycle {
        TaskLifecycle::Closing => Ok(Vec::new()),
        TaskLifecycle::Archived => Err(RejectionCode::InvalidTransition),
        TaskLifecycle::Open => {
            let action_epoch = snap
                .task
                .action_epoch
                .checked_add(1)
                .ok_or(RejectionCode::InvalidTransition)?;
            Ok(vec![Event::TaskCloseBegun { action_epoch }])
        }
    }
}

fn require_task<'a>(
    snapshot: Option<&'a TaskSnapshot>,
    envelope: &CommandEnvelope,
) -> Result<&'a TaskSnapshot, RejectionCode> {
    let Some(snap) = snapshot else {
        return Err(RejectionCode::NotFound);
    };
    let Some(task_id) = envelope.task_id else {
        return Err(RejectionCode::InvalidTransition);
    };
    if snap.task.id != task_id {
        return Err(RejectionCode::NotFound);
    }
    Ok(snap)
}

fn require_expected_revision(
    snap: &TaskSnapshot,
    envelope: &CommandEnvelope,
) -> Result<(), RejectionCode> {
    match envelope.expected_task_revision {
        Some(expected) if expected == snap.task.revision => Ok(()),
        _ => Err(RejectionCode::RevisionConflict),
    }
}

fn require_open_or_closing_task<'a>(
    snapshot: Option<&'a TaskSnapshot>,
    envelope: &CommandEnvelope,
) -> Result<&'a TaskSnapshot, RejectionCode> {
    let snap = require_task(snapshot, envelope)?;
    match snap.task.lifecycle {
        TaskLifecycle::Open | TaskLifecycle::Closing => Ok(snap),
        TaskLifecycle::Archived => Err(RejectionCode::InvalidTransition),
    }
}

fn require_runtime_capable_task<'a>(
    snapshot: Option<&'a TaskSnapshot>,
    envelope: &CommandEnvelope,
) -> Result<&'a TaskSnapshot, RejectionCode> {
    let snap = require_task(snapshot, envelope)?;
    match snap.task.lifecycle {
        TaskLifecycle::Open => Ok(snap),
        TaskLifecycle::Closing | TaskLifecycle::Archived => Err(RejectionCode::InvalidTransition),
    }
}
