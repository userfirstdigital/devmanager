use serde::de::{self, DeserializeSeed, MapAccess, Visitor};
use serde::ser::SerializeMap;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::domain::agent::{AgentSessionFacts, AgentSessionLifecycle};
use crate::domain::artifact::ArtifactFacts;
use crate::domain::browser::{BrowserContractError, BrowserRequest};
use crate::domain::event::Event;
use crate::domain::id::{
    AgentSessionId, ClientId, CommandId, EnvironmentId, EventId, OperationId, ProjectId,
    ResourceId, TaskId, TurnId,
};
use crate::domain::provider_input::{
    validate_action_nested_ids, PresentProviderApprovalIntent, PresentProviderQuestionIntent,
    ProviderInputAction, ProviderInputIntentError, ProviderResolutionWinner,
    SettleProviderWaitIntent,
};
use crate::domain::resource::ResourceFacts;
use crate::domain::snapshot::TaskSnapshot;
use crate::domain::task::{
    ReviewReadiness, TaskActivity, TaskAssignment, TaskAttention, TaskConnectivity, TaskFacts,
    TaskLifecycle, WorkspaceRef,
};
use crate::prompts::{PromptCommand, PromptMutationReceipt};
use crate::workspace::WorkspaceRequest;
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RejectionCode {
    NotFound,
    AlreadyExists,
    RevisionConflict,
    InvalidTransition,
    OwnershipConflict,
    UnsupportedCapability,
    Closing,
    IdempotencyConflict,
    AlreadyResolved,
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
                    "closing" => Ok(RejectionCode::Closing),
                    "idempotency_conflict" => Ok(RejectionCode::IdempotencyConflict),
                    "already_resolved" => Ok(RejectionCode::AlreadyResolved),
                    _ => Err(de::Error::unknown_variant(
                        value,
                        &[
                            "not_found",
                            "already_exists",
                            "revision_conflict",
                            "invalid_transition",
                            "ownership_conflict",
                            "unsupported_capability",
                            "closing",
                            "idempotency_conflict",
                            "already_resolved",
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
        prompt_mutation: Option<PromptMutationReceipt>,
    },
    Rejected {
        command_id: CommandId,
        code: RejectionCode,
        current_revision: Option<u64>,
        resolution: Option<ProviderResolutionWinner>,
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
    prompt_mutation: &'a Option<PromptMutationReceipt>,
}

impl Serialize for AcceptedReceiptRef<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let fields = 4 + usize::from(self.prompt_mutation.is_some());
        let mut map = serializer.serialize_map(Some(fields))?;
        map.serialize_entry("command_id", self.command_id)?;
        map.serialize_entry("operation_id", self.operation_id)?;
        map.serialize_entry("task_revision", self.task_revision)?;
        map.serialize_entry("event_ids", self.event_ids)?;
        if let Some(mutation) = self.prompt_mutation {
            map.serialize_entry("prompt_mutation", mutation)?;
        }
        map.end()
    }
}

struct RejectedReceiptRef<'a> {
    command_id: &'a CommandId,
    code: &'a RejectionCode,
    current_revision: &'a Option<u64>,
    resolution: &'a Option<ProviderResolutionWinner>,
}

impl Serialize for RejectedReceiptRef<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut map =
            serializer.serialize_map(Some(if self.resolution.is_some() { 4 } else { 3 }))?;
        map.serialize_entry("command_id", self.command_id)?;
        map.serialize_entry("code", self.code)?;
        map.serialize_entry("current_revision", self.current_revision)?;
        if let Some(resolution) = self.resolution {
            map.serialize_entry("resolution", resolution)?;
        }
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
                prompt_mutation,
            } => map.serialize_entry(
                "accepted",
                &AcceptedReceiptRef {
                    command_id,
                    operation_id,
                    task_revision,
                    event_ids,
                    prompt_mutation,
                },
            )?,
            Self::Rejected {
                command_id,
                code,
                current_revision,
                resolution,
            } => map.serialize_entry(
                "rejected",
                &RejectedReceiptRef {
                    command_id,
                    code,
                    current_revision,
                    resolution,
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

const ACCEPTED_RECEIPT_FIELDS: &[&str] = &[
    "command_id",
    "operation_id",
    "task_revision",
    "event_ids",
    "prompt_mutation",
];

enum AcceptedReceiptField {
    CommandId,
    OperationId,
    TaskRevision,
    EventIds,
    PromptMutation,
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
                    "prompt_mutation" => Ok(AcceptedReceiptField::PromptMutation),
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
        let mut prompt_mutation = None;

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
                AcceptedReceiptField::PromptMutation => {
                    if prompt_mutation.is_some() {
                        return Err(de::Error::duplicate_field("prompt_mutation"));
                    }
                    prompt_mutation = Some(map.next_value()?);
                }
            }
        }

        Ok(CommandReceipt::Accepted {
            command_id: command_id.ok_or_else(|| de::Error::missing_field("command_id"))?,
            operation_id: operation_id.ok_or_else(|| de::Error::missing_field("operation_id"))?,
            task_revision: task_revision
                .ok_or_else(|| de::Error::missing_field("task_revision"))?,
            event_ids: event_ids.ok_or_else(|| de::Error::missing_field("event_ids"))?,
            prompt_mutation,
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

const REJECTED_RECEIPT_FIELDS: &[&str] = &["command_id", "code", "current_revision", "resolution"];

enum RejectedReceiptField {
    CommandId,
    Code,
    CurrentRevision,
    Resolution,
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
                    "resolution" => Ok(RejectedReceiptField::Resolution),
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
        let mut resolution: Option<Option<ProviderResolutionWinner>> = None;

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
                RejectedReceiptField::Resolution => {
                    if resolution.is_some() {
                        return Err(de::Error::duplicate_field("resolution"));
                    }
                    resolution = Some(map.next_value()?);
                }
            }
        }

        Ok(CommandReceipt::Rejected {
            command_id: command_id.ok_or_else(|| de::Error::missing_field("command_id"))?,
            code: code.ok_or_else(|| de::Error::missing_field("code"))?,
            current_revision: current_revision
                .ok_or_else(|| de::Error::missing_field("current_revision"))?,
            resolution: resolution.unwrap_or(None),
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

/// Request-shaped task creation accepted only at the authenticated host
/// boundary. The host resolves `workspace` against the ProjectId root from
/// host-owned configuration and creates the durable [`CreateTaskIntent`]
/// privately before persistence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateTaskRequestIntent {
    pub id: TaskId,
    pub environment_id: EnvironmentId,
    pub title: String,
    pub description: Option<String>,
    pub project_id: ProjectId,
    pub workspace: WorkspaceRequest,
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ConfirmHostQuitIntent {
    pub inspection_id: u64,
    pub allow_uninspected_worktrees: bool,
}

enum ConfirmHostQuitField {
    InspectionId,
    AllowUninspectedWorktrees,
}

impl<'de> Deserialize<'de> for ConfirmHostQuitField {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct FieldVisitor;
        impl Visitor<'_> for FieldVisitor {
            type Value = ConfirmHostQuitField;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("confirm_host_quit field")
            }
            fn visit_str<E: de::Error>(self, value: &str) -> Result<Self::Value, E> {
                match value {
                    "inspection_id" => Ok(ConfirmHostQuitField::InspectionId),
                    "allow_uninspected_worktrees" => {
                        Ok(ConfirmHostQuitField::AllowUninspectedWorktrees)
                    }
                    other => Err(E::unknown_field(
                        other,
                        &["inspection_id", "allow_uninspected_worktrees"],
                    )),
                }
            }
        }
        deserializer.deserialize_identifier(FieldVisitor)
    }
}

impl<'de> Deserialize<'de> for ConfirmHostQuitIntent {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct IntentVisitor;
        impl<'de> Visitor<'de> for IntentVisitor {
            type Value = ConfirmHostQuitIntent;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("confirm_host_quit named map")
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                let mut inspection_id = None;
                let mut allow_uninspected_worktrees = None;
                while let Some(key) = map.next_key::<ConfirmHostQuitField>()? {
                    match key {
                        ConfirmHostQuitField::InspectionId => {
                            if inspection_id.is_some() {
                                return Err(de::Error::duplicate_field("inspection_id"));
                            }
                            inspection_id = Some(map.next_value()?);
                        }
                        ConfirmHostQuitField::AllowUninspectedWorktrees => {
                            if allow_uninspected_worktrees.is_some() {
                                return Err(de::Error::duplicate_field(
                                    "allow_uninspected_worktrees",
                                ));
                            }
                            allow_uninspected_worktrees = Some(map.next_value()?);
                        }
                    }
                }
                Ok(ConfirmHostQuitIntent {
                    inspection_id: inspection_id
                        .ok_or_else(|| de::Error::missing_field("inspection_id"))?,
                    allow_uninspected_worktrees: allow_uninspected_worktrees
                        .ok_or_else(|| de::Error::missing_field("allow_uninspected_worktrees"))?,
                })
            }
        }
        deserializer.deserialize_map(IntentVisitor)
    }
}

#[derive(Clone, PartialEq, Eq, Serialize)]
pub struct SubmitProviderInputIntent {
    agent_session_id: AgentSessionId,
    runtime_generation: u64,
    turn_id: TurnId,
    action_epoch: u64,
    question_id: Option<crate::domain::id::QuestionId>,
    approval_id: Option<crate::domain::id::ApprovalId>,
    action: ProviderInputAction,
}

impl std::fmt::Debug for SubmitProviderInputIntent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SubmitProviderInputIntent")
            .field("agent_session_id", &self.agent_session_id)
            .field("runtime_generation", &self.runtime_generation)
            .field("turn_id", &self.turn_id)
            .field("action_epoch", &self.action_epoch)
            .field("question_id", &self.question_id)
            .field("approval_id", &self.approval_id)
            .field("action", &self.action)
            .finish()
    }
}

impl SubmitProviderInputIntent {
    pub fn try_new(
        agent_session_id: AgentSessionId,
        runtime_generation: u64,
        turn_id: TurnId,
        action_epoch: u64,
        question_id: Option<crate::domain::id::QuestionId>,
        approval_id: Option<crate::domain::id::ApprovalId>,
        action: ProviderInputAction,
    ) -> Result<Self, ProviderInputIntentError> {
        validate_action_nested_ids(&action, question_id, approval_id)?;
        Ok(Self {
            agent_session_id,
            runtime_generation,
            turn_id,
            action_epoch,
            question_id,
            approval_id,
            action,
        })
    }

    pub fn agent_session_id(&self) -> AgentSessionId {
        self.agent_session_id
    }

    pub fn runtime_generation(&self) -> u64 {
        self.runtime_generation
    }

    pub fn turn_id(&self) -> TurnId {
        self.turn_id
    }

    pub fn action_epoch(&self) -> u64 {
        self.action_epoch
    }

    pub fn question_id(&self) -> Option<crate::domain::id::QuestionId> {
        self.question_id
    }

    pub fn approval_id(&self) -> Option<crate::domain::id::ApprovalId> {
        self.approval_id
    }

    pub fn action(&self) -> &ProviderInputAction {
        &self.action
    }

    pub fn validate(&self) -> Result<(), ProviderInputIntentError> {
        validate_action_nested_ids(&self.action, self.question_id, self.approval_id)
    }
}

impl<'de> Deserialize<'de> for SubmitProviderInputIntent {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            agent_session_id: AgentSessionId,
            runtime_generation: u64,
            turn_id: TurnId,
            action_epoch: u64,
            question_id: Option<crate::domain::id::QuestionId>,
            approval_id: Option<crate::domain::id::ApprovalId>,
            action: ProviderInputAction,
        }
        let wire = Wire::deserialize(deserializer)?;
        Self::try_new(
            wire.agent_session_id,
            wire.runtime_generation,
            wire.turn_id,
            wire.action_epoch,
            wire.question_id,
            wire.approval_id,
            wire.action,
        )
        .map_err(de::Error::custom)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum Command {
    CreateTask(CreateTaskIntent),
    CreateTaskV2(CreateTaskRequestIntent),
    RenameTask(RenameTaskIntent),
    SetTaskAttention(SetTaskAttentionIntent),
    BeginCloseTask,
    ReopenTask,
    RegisterAgentSession {
        agent: AgentSessionFacts,
    },
    SetPrimaryAgent {
        agent_session_id: AgentSessionId,
    },
    RegisterArtifact {
        artifact: ArtifactFacts,
    },
    RegisterResource {
        resource: ResourceFacts,
    },
    ReleaseResource {
        resource_id: ResourceId,
    },
    ConfirmHostQuit(ConfirmHostQuitIntent),
    SubmitProviderInput(SubmitProviderInputIntent),
    /// Journal ingress only. Host `ClientRequest` rejects this variant.
    PresentProviderQuestion(PresentProviderQuestionIntent),
    /// Journal ingress only. Host `ClientRequest` rejects this variant.
    PresentProviderApproval(PresentProviderApprovalIntent),
    /// Journal ingress only. Host `ClientRequest` rejects this variant.
    SettleProviderWait(SettleProviderWaitIntent),
    PromptLibrary(PromptCommand),
    Browser(BrowserRequest),
    /// Host-boundary update handoff: inspect+prepare with expiring token.
    PrepareUpdate(PrepareUpdateIntent),
    /// Confirm drain after PrepareUpdate; stops new launches until abort/arm.
    ConfirmUpdateDrain(ConfirmUpdateDrainIntent),
    /// Abort pre-install handoff and restore Ready admission.
    AbortUpdateHandoff,
    /// Arm durable staged-install readiness (recoverable). Irreversible only after
    /// durable stage marker is written by the installer path.
    ArmUpdateInstall(ArmUpdateInstallIntent),
}

/// Canonical SHA-256 over client, task, expected revision, and command.
/// `issued_at_ms` is excluded. Fence identities (task/agent/generation/
/// epoch/turn/question/approval/action) are part of `command` and therefore
/// part of the digest. A retry with a different fence is a conflict.
pub fn command_payload_digest(envelope: &CommandEnvelope) -> Result<[u8; 32], String> {
    use sha2::{Digest, Sha256};
    #[derive(Serialize)]
    struct DigestBody<'a> {
        client_id: ClientId,
        task_id: Option<TaskId>,
        expected_task_revision: Option<u64>,
        command: &'a Command,
    }
    let packed = rmp_serde::to_vec_named(&DigestBody {
        client_id: envelope.client_id,
        task_id: envelope.task_id,
        expected_task_revision: envelope.expected_task_revision,
        command: &envelope.command,
    })
    .map_err(|error| error.to_string())?;
    let digest = Sha256::digest(packed);
    let mut out = [0u8; 32];
    out.copy_from_slice(&digest);
    Ok(out)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub struct PrepareUpdateIntent {
    pub target_version: String,
    pub client_build: String,
    pub host_build: String,
    pub allow_explicit_confirm_with_active: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub struct ConfirmUpdateDrainIntent {
    pub token_id: Uuid,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub struct ArmUpdateInstallIntent {
    pub token_id: Uuid,
}

pub fn decide(
    snapshot: Option<&TaskSnapshot>,
    envelope: &CommandEnvelope,
) -> Result<Vec<Event>, RejectionCode> {
    match &envelope.command {
        Command::CreateTask(intent) => decide_create_task(snapshot, envelope, intent),
        // Request-shaped creation must be normalized by the host boundary;
        // accepting it in the domain would make the client request itself
        // authoritative over durable workspace state.
        Command::CreateTaskV2(_) => Err(RejectionCode::InvalidTransition),
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
        Command::ConfirmHostQuit(_) => Err(RejectionCode::InvalidTransition),
        Command::SubmitProviderInput(intent) => {
            decide_submit_provider_input(snapshot, envelope, intent)
        }
        Command::PresentProviderQuestion(intent) => {
            decide_present_provider_question(snapshot, envelope, intent)
        }
        Command::PresentProviderApproval(intent) => {
            decide_present_provider_approval(snapshot, envelope, intent)
        }
        Command::SettleProviderWait(intent) => {
            decide_settle_provider_wait(snapshot, envelope, intent)
        }
        Command::PromptLibrary(_) => Err(RejectionCode::InvalidTransition),
        Command::Browser(request) => decide_browser(snapshot, envelope, request),
        Command::PrepareUpdate(_)
        | Command::ConfirmUpdateDrain(_)
        | Command::AbortUpdateHandoff
        | Command::ArmUpdateInstall(_) => Err(RejectionCode::InvalidTransition),
    }
}

fn decide_browser(
    snapshot: Option<&TaskSnapshot>,
    envelope: &CommandEnvelope,
    request: &BrowserRequest,
) -> Result<Vec<Event>, RejectionCode> {
    let snap = require_runtime_capable_task(snapshot, envelope)?;
    require_expected_revision(snap, envelope)?;
    if request.task_id != snap.task.id {
        return Err(RejectionCode::OwnershipConflict);
    }
    let mut accepted = snap
        .browser
        .plan_admit(request)
        .map_err(browser_rejection)?;
    accepted.bind_command(envelope.command_id, snap.task.action_epoch);
    Ok(accepted.facts.into_iter().map(Event::Browser).collect())
}

fn browser_rejection(error: BrowserContractError) -> RejectionCode {
    match error {
        BrowserContractError::CrossTask => RejectionCode::OwnershipConflict,
        BrowserContractError::GenerationMismatch
        | BrowserContractError::ClosedTask
        | BrowserContractError::IdempotencyConflict
        | BrowserContractError::BoundExceeded
        | BrowserContractError::InvalidRequest
        | BrowserContractError::HostEffectUnavailable => RejectionCode::InvalidTransition,
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

fn decide_submit_provider_input(
    snapshot: Option<&TaskSnapshot>,
    envelope: &CommandEnvelope,
    intent: &SubmitProviderInputIntent,
) -> Result<Vec<Event>, RejectionCode> {
    let snap = require_runtime_capable_task(snapshot, envelope)?;
    require_expected_revision(snap, envelope)?;
    intent
        .validate()
        .map_err(|_| RejectionCode::InvalidTransition)?;
    let agent = snap
        .agents
        .get(&intent.agent_session_id())
        .ok_or(RejectionCode::NotFound)?;
    if agent.task_id != snap.task.id {
        return Err(RejectionCode::OwnershipConflict);
    }
    require_open_agent(agent)?;
    let Some(provider_session_id) = agent.provider_session_id.clone() else {
        return Err(RejectionCode::UnsupportedCapability);
    };
    let provider_kind =
        crate::domain::provider_input::ProviderKind::new(agent.provider_kind.clone())
            .map_err(|_| RejectionCode::UnsupportedCapability)?;
    if agent.runtime_generation != intent.runtime_generation() {
        return Err(RejectionCode::InvalidTransition);
    }
    if snap.task.action_epoch != intent.action_epoch() {
        return Err(RejectionCode::InvalidTransition);
    }
    let session = snap
        .provider_sessions
        .get(&intent.agent_session_id())
        .cloned()
        .unwrap_or_default();
    match intent.action() {
        ProviderInputAction::SendNow { .. } => {
            if let Some(current) = session.current_turn {
                if current != intent.turn_id() {
                    return Err(RejectionCode::InvalidTransition);
                }
            }
        }
        ProviderInputAction::SteerCurrentTurn { .. }
        | ProviderInputAction::QueueFollowUp { .. }
        | ProviderInputAction::StopTurn => {
            if session.current_turn != Some(intent.turn_id()) {
                return Err(RejectionCode::InvalidTransition);
            }
        }
        ProviderInputAction::AnswerQuestion { question_id, .. } => {
            if session.question_winners.contains_key(question_id) {
                return Err(RejectionCode::AlreadyResolved);
            }
            if session.open_question != Some(*question_id) {
                return Err(RejectionCode::InvalidTransition);
            }
            if session.current_turn != Some(intent.turn_id()) {
                return Err(RejectionCode::InvalidTransition);
            }
        }
        ProviderInputAction::ResolveApproval { approval_id, .. } => {
            if session.approval_winners.contains_key(approval_id) {
                return Err(RejectionCode::AlreadyResolved);
            }
            if session.open_approval != Some(*approval_id) {
                return Err(RejectionCode::InvalidTransition);
            }
            if session.current_turn != Some(intent.turn_id()) {
                return Err(RejectionCode::InvalidTransition);
            }
        }
    }
    if intent.action().waits_for_turn()
        && !session.waits.contains_key(&envelope.command_id)
        && session
            .waits
            .values()
            .filter(|record| record.pending)
            .count()
            >= crate::domain::provider_input::MAX_PROVIDER_WAITS
    {
        return Err(RejectionCode::InvalidTransition);
    }
    Ok(vec![Event::ProviderInputAccepted {
        command_id: envelope.command_id,
        client_id: envelope.client_id,
        operation_id: OperationId::new(),
        agent_session_id: intent.agent_session_id(),
        provider_kind,
        provider_session_id,
        runtime_generation: intent.runtime_generation(),
        turn_id: intent.turn_id(),
        action_epoch: intent.action_epoch(),
        question_id: intent.question_id(),
        approval_id: intent.approval_id(),
        action: intent.action().clone(),
        wait: intent.action().waits_for_turn(),
        delivery: crate::domain::provider_input::ProviderDeliveryVisibility::hold_until_destination_adapter(),
    }])
}

fn decide_present_provider_question(
    snapshot: Option<&TaskSnapshot>,
    envelope: &CommandEnvelope,
    intent: &PresentProviderQuestionIntent,
) -> Result<Vec<Event>, RejectionCode> {
    let snap = require_runtime_capable_task(snapshot, envelope)?;
    require_expected_revision(snap, envelope)?;
    let agent = snap
        .agents
        .get(&intent.agent_session_id())
        .ok_or(RejectionCode::NotFound)?;
    require_open_agent(agent)?;
    let Some(provider_session_id) = agent.provider_session_id.clone() else {
        return Err(RejectionCode::UnsupportedCapability);
    };
    let provider_kind =
        crate::domain::provider_input::ProviderKind::new(agent.provider_kind.clone())
            .map_err(|_| RejectionCode::UnsupportedCapability)?;
    if agent.runtime_generation != intent.runtime_generation()
        || snap.task.action_epoch != intent.action_epoch()
    {
        return Err(RejectionCode::InvalidTransition);
    }
    let session = snap
        .provider_sessions
        .get(&intent.agent_session_id())
        .cloned()
        .unwrap_or_default();
    if let Some(current) = session.current_turn {
        if current != intent.turn_id() {
            return Err(RejectionCode::InvalidTransition);
        }
    }
    if session.question_winners.contains_key(&intent.question_id()) {
        return Err(RejectionCode::AlreadyResolved);
    }
    if let Some(open) = session.open_question {
        if open != intent.question_id() {
            return Err(RejectionCode::InvalidTransition);
        }
        return Err(RejectionCode::AlreadyExists);
    }
    Ok(vec![Event::ProviderQuestionPresented {
        agent_session_id: intent.agent_session_id(),
        provider_kind,
        provider_session_id,
        runtime_generation: intent.runtime_generation(),
        turn_id: intent.turn_id(),
        action_epoch: intent.action_epoch(),
        question_id: intent.question_id(),
    }])
}

fn decide_present_provider_approval(
    snapshot: Option<&TaskSnapshot>,
    envelope: &CommandEnvelope,
    intent: &PresentProviderApprovalIntent,
) -> Result<Vec<Event>, RejectionCode> {
    let snap = require_runtime_capable_task(snapshot, envelope)?;
    require_expected_revision(snap, envelope)?;
    let agent = snap
        .agents
        .get(&intent.agent_session_id())
        .ok_or(RejectionCode::NotFound)?;
    require_open_agent(agent)?;
    let Some(provider_session_id) = agent.provider_session_id.clone() else {
        return Err(RejectionCode::UnsupportedCapability);
    };
    let provider_kind =
        crate::domain::provider_input::ProviderKind::new(agent.provider_kind.clone())
            .map_err(|_| RejectionCode::UnsupportedCapability)?;
    if agent.runtime_generation != intent.runtime_generation()
        || snap.task.action_epoch != intent.action_epoch()
    {
        return Err(RejectionCode::InvalidTransition);
    }
    let session = snap
        .provider_sessions
        .get(&intent.agent_session_id())
        .cloned()
        .unwrap_or_default();
    if let Some(current) = session.current_turn {
        if current != intent.turn_id() {
            return Err(RejectionCode::InvalidTransition);
        }
    }
    if session.approval_winners.contains_key(&intent.approval_id()) {
        return Err(RejectionCode::AlreadyResolved);
    }
    if let Some(open) = session.open_approval {
        if open != intent.approval_id() {
            return Err(RejectionCode::InvalidTransition);
        }
        return Err(RejectionCode::AlreadyExists);
    }
    Ok(vec![Event::ProviderApprovalPresented {
        agent_session_id: intent.agent_session_id(),
        provider_kind,
        provider_session_id,
        runtime_generation: intent.runtime_generation(),
        turn_id: intent.turn_id(),
        action_epoch: intent.action_epoch(),
        approval_id: intent.approval_id(),
    }])
}

fn decide_settle_provider_wait(
    snapshot: Option<&TaskSnapshot>,
    envelope: &CommandEnvelope,
    intent: &SettleProviderWaitIntent,
) -> Result<Vec<Event>, RejectionCode> {
    let snap = require_open_or_closing_task(snapshot, envelope)?;
    require_expected_revision(snap, envelope)?;
    if intent.fence().task_id() != snap.task.id {
        return Err(RejectionCode::OwnershipConflict);
    }
    crate::domain::provider_input::validate_provider_fence(
        &intent.fence().identity(),
        None,
        None,
        None,
    )
    .map_err(|_| RejectionCode::InvalidTransition)?;
    let session = snap
        .provider_sessions
        .get(&intent.fence().agent_session_id())
        .ok_or(RejectionCode::NotFound)?;
    let record = session
        .waits
        .get(&intent.fence().command_id())
        .ok_or(RejectionCode::NotFound)?;
    if !record.fence.matches(intent.fence()) {
        return Err(RejectionCode::InvalidTransition);
    }
    let agent = snap
        .agents
        .get(&intent.fence().agent_session_id())
        .ok_or(RejectionCode::NotFound)?;
    require_live_agent(agent)?;
    if agent.runtime_generation != intent.fence().runtime_generation() {
        return Err(RejectionCode::InvalidTransition);
    }
    if snap.task.action_epoch != intent.fence().action_epoch() {
        return Err(RejectionCode::InvalidTransition);
    }
    if !record.pending {
        return Err(RejectionCode::AlreadyExists);
    }
    Ok(vec![Event::ProviderWaitSettled {
        fence: intent.fence().clone(),
    }])
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
        TaskLifecycle::Closing => Err(RejectionCode::Closing),
        TaskLifecycle::Archived => Err(RejectionCode::InvalidTransition),
    }
}

fn require_open_agent(agent: &AgentSessionFacts) -> Result<(), RejectionCode> {
    match agent.lifecycle {
        AgentSessionLifecycle::Open => Ok(()),
        AgentSessionLifecycle::Closing | AgentSessionLifecycle::Closed => {
            Err(RejectionCode::InvalidTransition)
        }
    }
}

fn require_live_agent(agent: &AgentSessionFacts) -> Result<(), RejectionCode> {
    match agent.lifecycle {
        AgentSessionLifecycle::Open | AgentSessionLifecycle::Closing => Ok(()),
        AgentSessionLifecycle::Closed => Err(RejectionCode::InvalidTransition),
    }
}
