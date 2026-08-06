use serde::de::{self, DeserializeSeed, MapAccess, Visitor};
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RejectionCode {
    NotFound,
    AlreadyExists,
    RevisionConflict,
    InvalidTransition,
    OwnershipConflict,
    UnsupportedCapability,
}

impl<'de> Deserialize<'de> for RejectionCode {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct RejectionCodeVisitor;

        impl Visitor<'_> for RejectionCodeVisitor {
            type Value = RejectionCode;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a named RejectionCode")
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                match value {
                    "not_found" => Ok(RejectionCode::NotFound),
                    "already_exists" => Ok(RejectionCode::AlreadyExists),
                    "revision_conflict" => Ok(RejectionCode::RevisionConflict),
                    "invalid_transition" => Ok(RejectionCode::InvalidTransition),
                    "ownership_conflict" => Ok(RejectionCode::OwnershipConflict),
                    "unsupported_capability" => Ok(RejectionCode::UnsupportedCapability),
                    _ => Err(de::Error::unknown_variant(
                        value,
                        &[
                            "not_found",
                            "already_exists",
                            "revision_conflict",
                            "invalid_transition",
                            "ownership_conflict",
                            "unsupported_capability",
                        ],
                    )),
                }
            }
        }

        deserializer.deserialize_str(RejectionCodeVisitor)
    }
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

#[derive(Debug, Clone, PartialEq, Eq)]
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

impl CommandReceipt {
    pub const fn command_id(&self) -> CommandId {
        match self {
            Self::Accepted { command_id, .. } | Self::Rejected { command_id, .. } => *command_id,
        }
    }

    pub const fn accepted_operation_id(&self) -> Option<OperationId> {
        match self {
            Self::Accepted { operation_id, .. } => Some(*operation_id),
            Self::Rejected { .. } => None,
        }
    }
}

struct AcceptedReceiptRef<'a> {
    command_id: &'a CommandId,
    operation_id: &'a OperationId,
    task_revision: &'a Option<u64>,
    event_ids: &'a [EventId],
}

impl Serialize for AcceptedReceiptRef<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut map = serializer.serialize_map(Some(4))?;
        map.serialize_entry("command_id", self.command_id)?;
        map.serialize_entry("operation_id", self.operation_id)?;
        map.serialize_entry("task_revision", self.task_revision)?;
        map.serialize_entry("event_ids", self.event_ids)?;
        map.end()
    }
}

struct RejectedReceiptRef<'a> {
    command_id: &'a CommandId,
    code: &'a RejectionCode,
    current_revision: &'a Option<u64>,
}

impl Serialize for RejectedReceiptRef<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut map = serializer.serialize_map(Some(3))?;
        map.serialize_entry("command_id", self.command_id)?;
        map.serialize_entry("code", self.code)?;
        map.serialize_entry("current_revision", self.current_revision)?;
        map.end()
    }
}

impl Serialize for CommandReceipt {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut map = serializer.serialize_map(Some(1))?;
        match self {
            Self::Accepted {
                command_id,
                operation_id,
                task_revision,
                event_ids,
            } => map.serialize_entry(
                "accepted",
                &AcceptedReceiptRef {
                    command_id,
                    operation_id,
                    task_revision,
                    event_ids,
                },
            )?,
            Self::Rejected {
                command_id,
                code,
                current_revision,
            } => map.serialize_entry(
                "rejected",
                &RejectedReceiptRef {
                    command_id,
                    code,
                    current_revision,
                },
            )?,
        }
        map.end()
    }
}

enum CommandReceiptVariant {
    Accepted,
    Rejected,
}

impl<'de> Deserialize<'de> for CommandReceiptVariant {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct VariantVisitor;

        impl Visitor<'_> for VariantVisitor {
            type Value = CommandReceiptVariant;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("accepted or rejected")
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                match value {
                    "accepted" => Ok(CommandReceiptVariant::Accepted),
                    "rejected" => Ok(CommandReceiptVariant::Rejected),
                    _ => Err(de::Error::unknown_variant(value, &["accepted", "rejected"])),
                }
            }
        }

        deserializer.deserialize_identifier(VariantVisitor)
    }
}

struct AcceptedReceiptSeed;

impl<'de> DeserializeSeed<'de> for AcceptedReceiptSeed {
    type Value = CommandReceipt;

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_map(AcceptedReceiptVisitor)
    }
}

const ACCEPTED_RECEIPT_FIELDS: &[&str] =
    &["command_id", "operation_id", "task_revision", "event_ids"];

enum AcceptedReceiptField {
    CommandId,
    OperationId,
    TaskRevision,
    EventIds,
}

impl<'de> Deserialize<'de> for AcceptedReceiptField {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct FieldVisitor;

        impl Visitor<'_> for FieldVisitor {
            type Value = AcceptedReceiptField;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("an accepted CommandReceipt field name")
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                match value {
                    "command_id" => Ok(AcceptedReceiptField::CommandId),
                    "operation_id" => Ok(AcceptedReceiptField::OperationId),
                    "task_revision" => Ok(AcceptedReceiptField::TaskRevision),
                    "event_ids" => Ok(AcceptedReceiptField::EventIds),
                    _ => Err(de::Error::unknown_field(value, ACCEPTED_RECEIPT_FIELDS)),
                }
            }
        }

        deserializer.deserialize_identifier(FieldVisitor)
    }
}

struct AcceptedReceiptVisitor;

impl<'de> Visitor<'de> for AcceptedReceiptVisitor {
    type Value = CommandReceipt;

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("a named accepted CommandReceipt payload map")
    }

    fn visit_map<M>(self, mut map: M) -> Result<Self::Value, M::Error>
    where
        M: MapAccess<'de>,
    {
        let mut command_id = None;
        let mut operation_id = None;
        let mut task_revision: Option<Option<u64>> = None;
        let mut event_ids = None;

        while let Some(field) = map.next_key()? {
            match field {
                AcceptedReceiptField::CommandId => {
                    if command_id.is_some() {
                        return Err(de::Error::duplicate_field("command_id"));
                    }
                    command_id = Some(map.next_value()?);
                }
                AcceptedReceiptField::OperationId => {
                    if operation_id.is_some() {
                        return Err(de::Error::duplicate_field("operation_id"));
                    }
                    operation_id = Some(map.next_value()?);
                }
                AcceptedReceiptField::TaskRevision => {
                    if task_revision.is_some() {
                        return Err(de::Error::duplicate_field("task_revision"));
                    }
                    task_revision = Some(map.next_value()?);
                }
                AcceptedReceiptField::EventIds => {
                    if event_ids.is_some() {
                        return Err(de::Error::duplicate_field("event_ids"));
                    }
                    event_ids = Some(map.next_value()?);
                }
            }
        }

        Ok(CommandReceipt::Accepted {
            command_id: command_id.ok_or_else(|| de::Error::missing_field("command_id"))?,
            operation_id: operation_id.ok_or_else(|| de::Error::missing_field("operation_id"))?,
            task_revision: task_revision
                .ok_or_else(|| de::Error::missing_field("task_revision"))?,
            event_ids: event_ids.ok_or_else(|| de::Error::missing_field("event_ids"))?,
        })
    }
}

struct RejectedReceiptSeed;

impl<'de> DeserializeSeed<'de> for RejectedReceiptSeed {
    type Value = CommandReceipt;

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_map(RejectedReceiptVisitor)
    }
}

const REJECTED_RECEIPT_FIELDS: &[&str] = &["command_id", "code", "current_revision"];

enum RejectedReceiptField {
    CommandId,
    Code,
    CurrentRevision,
}

impl<'de> Deserialize<'de> for RejectedReceiptField {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct FieldVisitor;

        impl Visitor<'_> for FieldVisitor {
            type Value = RejectedReceiptField;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a rejected CommandReceipt field name")
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                match value {
                    "command_id" => Ok(RejectedReceiptField::CommandId),
                    "code" => Ok(RejectedReceiptField::Code),
                    "current_revision" => Ok(RejectedReceiptField::CurrentRevision),
                    _ => Err(de::Error::unknown_field(value, REJECTED_RECEIPT_FIELDS)),
                }
            }
        }

        deserializer.deserialize_identifier(FieldVisitor)
    }
}

struct RejectedReceiptVisitor;

impl<'de> Visitor<'de> for RejectedReceiptVisitor {
    type Value = CommandReceipt;

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("a named rejected CommandReceipt payload map")
    }

    fn visit_map<M>(self, mut map: M) -> Result<Self::Value, M::Error>
    where
        M: MapAccess<'de>,
    {
        let mut command_id = None;
        let mut code = None;
        let mut current_revision: Option<Option<u64>> = None;

        while let Some(field) = map.next_key()? {
            match field {
                RejectedReceiptField::CommandId => {
                    if command_id.is_some() {
                        return Err(de::Error::duplicate_field("command_id"));
                    }
                    command_id = Some(map.next_value()?);
                }
                RejectedReceiptField::Code => {
                    if code.is_some() {
                        return Err(de::Error::duplicate_field("code"));
                    }
                    code = Some(map.next_value()?);
                }
                RejectedReceiptField::CurrentRevision => {
                    if current_revision.is_some() {
                        return Err(de::Error::duplicate_field("current_revision"));
                    }
                    current_revision = Some(map.next_value()?);
                }
            }
        }

        Ok(CommandReceipt::Rejected {
            command_id: command_id.ok_or_else(|| de::Error::missing_field("command_id"))?,
            code: code.ok_or_else(|| de::Error::missing_field("code"))?,
            current_revision: current_revision
                .ok_or_else(|| de::Error::missing_field("current_revision"))?,
        })
    }
}

impl<'de> Deserialize<'de> for CommandReceipt {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct CommandReceiptVisitor;

        impl<'de> Visitor<'de> for CommandReceiptVisitor {
            type Value = CommandReceipt;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a one-entry named CommandReceipt map")
            }

            fn visit_map<M>(self, mut map: M) -> Result<Self::Value, M::Error>
            where
                M: MapAccess<'de>,
            {
                let variant = map
                    .next_key()?
                    .ok_or_else(|| de::Error::custom("CommandReceipt variant is missing"))?;
                let receipt = match variant {
                    CommandReceiptVariant::Accepted => map.next_value_seed(AcceptedReceiptSeed)?,
                    CommandReceiptVariant::Rejected => map.next_value_seed(RejectedReceiptSeed)?,
                };
                if map.next_key::<de::IgnoredAny>()?.is_some() {
                    return Err(de::Error::custom(
                        "CommandReceipt must contain exactly one variant",
                    ));
                }
                Ok(receipt)
            }
        }

        deserializer.deserialize_map(CommandReceiptVisitor)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
pub struct RenameTaskIntent {
    pub title: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetTaskAttentionIntent {
    pub attention: TaskAttention,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
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
