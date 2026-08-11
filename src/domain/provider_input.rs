//! Durable provider-input projection and bounded intent validation.
//!
//! Acceptance, first-answer-wins, and wait fences live in the kernel event
//! log. This module only defines the projection shape and fail-closed checks.

use std::collections::BTreeMap;
use std::fmt;

use serde::de::{self, Deserializer, MapAccess, Visitor};
use serde::ser;
use serde::{Deserialize, Serialize};

use crate::domain::agent::AgentSessionLifecycle;
use crate::domain::id::{
    AgentSessionId, ApprovalId, ClientId, CommandId, QuestionId, TaskId, TurnId,
};

pub const MAX_PROVIDER_INPUT_TEXT_BYTES: usize = 64 * 1024;
pub const MAX_PROVIDER_QUESTION_WINS: usize = 256;
pub const MAX_PROVIDER_APPROVAL_WINS: usize = 256;
pub const MAX_PROVIDER_WAITS: usize = 64;
pub const MAX_PROVIDER_SESSION_STATE_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderInputIntentError {
    EmptyText,
    TextTooLarge,
    InconsistentNestedIds,
    QuestionWinnerLimit,
    ApprovalWinnerLimit,
    WaitLimit,
}

impl fmt::Display for ProviderInputIntentError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyText => write!(f, "provider input text must be non-empty"),
            Self::TextTooLarge => write!(
                f,
                "provider input text exceeds {MAX_PROVIDER_INPUT_TEXT_BYTES} bytes"
            ),
            Self::InconsistentNestedIds => {
                write!(f, "provider input nested identities are inconsistent")
            }
            Self::QuestionWinnerLimit => write!(
                f,
                "provider question winner map exceeds {MAX_PROVIDER_QUESTION_WINS} entries"
            ),
            Self::ApprovalWinnerLimit => write!(
                f,
                "provider approval winner map exceeds {MAX_PROVIDER_APPROVAL_WINS} entries"
            ),
            Self::WaitLimit => write!(f, "provider wait map exceeds {MAX_PROVIDER_WAITS} entries"),
        }
    }
}

impl std::error::Error for ProviderInputIntentError {}

#[derive(Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum ProviderInputAction {
    SendNow {
        text: String,
        wait: bool,
    },
    SteerCurrentTurn {
        text: String,
    },
    QueueFollowUp {
        text: String,
    },
    AnswerQuestion {
        question_id: QuestionId,
        answer: String,
    },
    ResolveApproval {
        approval_id: ApprovalId,
        allow: bool,
    },
    StopTurn,
}

impl<'de> Deserialize<'de> for ProviderInputAction {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(rename_all = "snake_case", deny_unknown_fields)]
        enum Wire {
            SendNow {
                #[serde(deserialize_with = "deserialize_provider_text")]
                text: String,
                wait: bool,
            },
            SteerCurrentTurn {
                #[serde(deserialize_with = "deserialize_provider_text")]
                text: String,
            },
            QueueFollowUp {
                #[serde(deserialize_with = "deserialize_provider_text")]
                text: String,
            },
            AnswerQuestion {
                question_id: QuestionId,
                #[serde(deserialize_with = "deserialize_provider_text")]
                answer: String,
            },
            ResolveApproval {
                approval_id: ApprovalId,
                allow: bool,
            },
            StopTurn,
        }
        Ok(match Wire::deserialize(deserializer)? {
            Wire::SendNow { text, wait } => Self::SendNow { text, wait },
            Wire::SteerCurrentTurn { text } => Self::SteerCurrentTurn { text },
            Wire::QueueFollowUp { text } => Self::QueueFollowUp { text },
            Wire::AnswerQuestion {
                question_id,
                answer,
            } => Self::AnswerQuestion {
                question_id,
                answer,
            },
            Wire::ResolveApproval { approval_id, allow } => {
                Self::ResolveApproval { approval_id, allow }
            }
            Wire::StopTurn => Self::StopTurn,
        })
    }
}

impl fmt::Debug for ProviderInputAction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SendNow { text, wait } => f
                .debug_struct("SendNow")
                .field("text_bytes", &text.len())
                .field("wait", wait)
                .finish(),
            Self::SteerCurrentTurn { text } => f
                .debug_struct("SteerCurrentTurn")
                .field("text_bytes", &text.len())
                .finish(),
            Self::QueueFollowUp { text } => f
                .debug_struct("QueueFollowUp")
                .field("text_bytes", &text.len())
                .finish(),
            Self::AnswerQuestion {
                question_id,
                answer,
            } => f
                .debug_struct("AnswerQuestion")
                .field("question_id", question_id)
                .field("answer_bytes", &answer.len())
                .finish(),
            Self::ResolveApproval { approval_id, allow } => f
                .debug_struct("ResolveApproval")
                .field("approval_id", approval_id)
                .field("allow", allow)
                .finish(),
            Self::StopTurn => write!(f, "StopTurn"),
        }
    }
}

impl ProviderInputAction {
    pub fn waits_for_turn(&self) -> bool {
        matches!(self, Self::SendNow { wait: true, .. })
    }
}

/// Durable intent acceptance. This is not provider delivery.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum ProviderIntentPhase {
    Accepted,
}

/// Why delivery remains visible HOLD/Uncertain.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum ProviderDeliveryHoldReason {
    DestinationAdapterNotWired,
}

/// Delivery visibility. There is no `Delivered` variant until a real
/// destination/outbox adapter exists, so this cannot be misreported.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum ProviderDeliveryVisibility {
    Hold { reason: ProviderDeliveryHoldReason },
}

impl ProviderDeliveryVisibility {
    pub const fn hold_until_destination_adapter() -> Self {
        Self::Hold {
            reason: ProviderDeliveryHoldReason::DestinationAdapterNotWired,
        }
    }

    pub const fn is_delivered(self) -> bool {
        false
    }

    pub const fn reason_code(self) -> &'static str {
        match self {
            Self::Hold {
                reason: ProviderDeliveryHoldReason::DestinationAdapterNotWired,
            } => "destination_adapter_not_wired",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderInputSettlement {
    pub command_id: CommandId,
    pub intent: ProviderIntentPhase,
    pub delivery: ProviderDeliveryVisibility,
}

impl ProviderInputSettlement {
    pub fn intent_accepted_delivery_hold(command_id: CommandId) -> Self {
        Self {
            command_id,
            intent: ProviderIntentPhase::Accepted,
            delivery: ProviderDeliveryVisibility::hold_until_destination_adapter(),
        }
    }

    pub const fn is_delivered(self) -> bool {
        self.delivery.is_delivered()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderResolutionWinner {
    pub command_id: CommandId,
    pub client_id: ClientId,
    pub accepted_at_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderWaitRecord {
    pub fence: ProviderWaitFence,
    pub pending: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderWaitFence {
    command_id: CommandId,
    task_id: TaskId,
    action_epoch: u64,
    agent_session_id: AgentSessionId,
    runtime_generation: u64,
    turn_id: TurnId,
    question_id: Option<QuestionId>,
    approval_id: Option<ApprovalId>,
}

impl ProviderWaitFence {
    pub fn new(
        command_id: CommandId,
        task_id: TaskId,
        action_epoch: u64,
        agent_session_id: AgentSessionId,
        runtime_generation: u64,
        turn_id: TurnId,
        question_id: Option<QuestionId>,
        approval_id: Option<ApprovalId>,
    ) -> Self {
        Self {
            command_id,
            task_id,
            action_epoch,
            agent_session_id,
            runtime_generation,
            turn_id,
            question_id,
            approval_id,
        }
    }

    pub fn command_id(&self) -> CommandId {
        self.command_id
    }

    pub fn task_id(&self) -> TaskId {
        self.task_id
    }

    pub fn action_epoch(&self) -> u64 {
        self.action_epoch
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

    pub fn question_id(&self) -> Option<QuestionId> {
        self.question_id
    }

    pub fn approval_id(&self) -> Option<ApprovalId> {
        self.approval_id
    }

    pub fn matches(&self, other: &Self) -> bool {
        self == other
    }

    pub fn identity(&self) -> ProviderFenceIdentity {
        ProviderFenceIdentity::new(
            Some(self.command_id),
            Some(self.task_id),
            self.agent_session_id,
            self.runtime_generation,
            self.action_epoch,
            self.turn_id,
            self.question_id,
            self.approval_id,
        )
    }
}

/// Exact provider identity carried by every provider event/fence.
///
/// The command identity is optional only for provider-originated question and
/// approval presentation events; accepted input and wait settlement always
/// carry it. No identity is inferred from cwd, timestamps, or PTY state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProviderFenceIdentity {
    pub command_id: Option<CommandId>,
    pub task_id: Option<TaskId>,
    pub agent_session_id: AgentSessionId,
    pub runtime_generation: u64,
    pub action_epoch: u64,
    pub turn_id: TurnId,
    pub question_id: Option<QuestionId>,
    pub approval_id: Option<ApprovalId>,
}

impl ProviderFenceIdentity {
    pub const fn new(
        command_id: Option<CommandId>,
        task_id: Option<TaskId>,
        agent_session_id: AgentSessionId,
        runtime_generation: u64,
        action_epoch: u64,
        turn_id: TurnId,
        question_id: Option<QuestionId>,
        approval_id: Option<ApprovalId>,
    ) -> Self {
        Self {
            command_id,
            task_id,
            agent_session_id,
            runtime_generation,
            action_epoch,
            turn_id,
            question_id,
            approval_id,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProviderFenceContext {
    pub task_id: TaskId,
    pub agent_session_id: AgentSessionId,
    pub agent_task_id: TaskId,
    pub runtime_generation: u64,
    pub action_epoch: u64,
    pub lifecycle: AgentSessionLifecycle,
    pub current_turn: Option<TurnId>,
    pub allow_closing: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderFenceError {
    TaskMismatch,
    AgentSessionMismatch,
    AgentOwnershipMismatch,
    RuntimeGenerationMismatch,
    ActionEpochMismatch,
    TurnMismatch,
    AgentNotLive,
    InvalidNestedIds,
    WaitFlagMismatch,
}

impl fmt::Display for ProviderFenceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::TaskMismatch => "provider fence task identity mismatch",
            Self::AgentSessionMismatch => "provider fence agent session identity mismatch",
            Self::AgentOwnershipMismatch => "provider fence agent belongs to another task",
            Self::RuntimeGenerationMismatch => "provider fence runtime generation mismatch",
            Self::ActionEpochMismatch => "provider fence action epoch mismatch",
            Self::TurnMismatch => "provider fence turn identity mismatch",
            Self::AgentNotLive => "provider fence agent lifecycle is not live",
            Self::InvalidNestedIds => "provider fence nested identities are inconsistent",
            Self::WaitFlagMismatch => "provider input wait flag disagrees with action",
        };
        f.write_str(message)
    }
}

impl std::error::Error for ProviderFenceError {}

pub fn validate_provider_fence(
    fence: &ProviderFenceIdentity,
    action: Option<&ProviderInputAction>,
    wait: Option<bool>,
    context: Option<&ProviderFenceContext>,
) -> Result<(), ProviderFenceError> {
    if let Some(action) = action {
        validate_action_nested_ids(action, fence.question_id, fence.approval_id)
            .map_err(|_| ProviderFenceError::InvalidNestedIds)?;
        if wait != Some(action.waits_for_turn()) {
            return Err(ProviderFenceError::WaitFlagMismatch);
        }
    } else if wait.is_some() {
        return Err(ProviderFenceError::WaitFlagMismatch);
    }
    if let Some(context) = context {
        if fence.task_id != Some(context.task_id) {
            return Err(ProviderFenceError::TaskMismatch);
        }
        if fence.agent_session_id != context.agent_session_id {
            return Err(ProviderFenceError::AgentSessionMismatch);
        }
        if context.agent_task_id != context.task_id {
            return Err(ProviderFenceError::AgentOwnershipMismatch);
        }
        if fence.runtime_generation != context.runtime_generation {
            return Err(ProviderFenceError::RuntimeGenerationMismatch);
        }
        if fence.action_epoch != context.action_epoch {
            return Err(ProviderFenceError::ActionEpochMismatch);
        }
        let live = matches!(context.lifecycle, AgentSessionLifecycle::Open)
            || (context.allow_closing
                && matches!(context.lifecycle, AgentSessionLifecycle::Closing));
        if !live {
            return Err(ProviderFenceError::AgentNotLive);
        }
        if let Some(current_turn) = context.current_turn {
            if fence.turn_id != current_turn {
                return Err(ProviderFenceError::TurnMismatch);
            }
        }
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ProviderSessionProjection {
    pub current_turn: Option<TurnId>,
    pub open_question: Option<QuestionId>,
    pub open_approval: Option<ApprovalId>,
    pub question_winners: BTreeMap<QuestionId, ProviderResolutionWinner>,
    pub approval_winners: BTreeMap<ApprovalId, ProviderResolutionWinner>,
    pub waits: BTreeMap<CommandId, ProviderWaitRecord>,
    pub last_settlement: Option<ProviderInputSettlement>,
}

struct BoundedProjectionMap<K, V, const LIMIT: usize>(BTreeMap<K, V>);

impl<'de, K, V, const LIMIT: usize> Deserialize<'de> for BoundedProjectionMap<K, V, LIMIT>
where
    K: Ord + Deserialize<'de>,
    V: Deserialize<'de>,
{
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct BoundedMapVisitor<K, V, const LIMIT: usize>(std::marker::PhantomData<(K, V)>);

        impl<'de, K, V, const LIMIT: usize> Visitor<'de> for BoundedMapVisitor<K, V, LIMIT>
        where
            K: Ord + Deserialize<'de>,
            V: Deserialize<'de>,
        {
            type Value = BoundedProjectionMap<K, V, LIMIT>;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(
                    formatter,
                    "a provider projection map with at most {LIMIT} entries"
                )
            }

            fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
            where
                A: MapAccess<'de>,
            {
                if map.size_hint().is_some_and(|hint| hint > LIMIT) {
                    return Err(de::Error::custom(format!(
                        "provider projection map exceeds {LIMIT} entries"
                    )));
                }
                let mut values = BTreeMap::new();
                let mut entries_seen = 0usize;
                while let Some(key) = map.next_key::<K>()? {
                    if entries_seen >= LIMIT {
                        return Err(de::Error::custom(format!(
                            "provider projection map exceeds {LIMIT} entries"
                        )));
                    }
                    entries_seen += 1;
                    let value = map.next_value::<V>()?;
                    values.insert(key, value);
                }
                Ok(BoundedProjectionMap(values))
            }
        }

        deserializer.deserialize_map(BoundedMapVisitor::<K, V, LIMIT>(std::marker::PhantomData))
    }
}

impl Serialize for ProviderSessionProjection {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.validate_bounds().map_err(ser::Error::custom)?;
        #[derive(Serialize)]
        struct Wire<'a> {
            current_turn: &'a Option<TurnId>,
            open_question: &'a Option<QuestionId>,
            open_approval: &'a Option<ApprovalId>,
            question_winners: &'a BTreeMap<QuestionId, ProviderResolutionWinner>,
            approval_winners: &'a BTreeMap<ApprovalId, ProviderResolutionWinner>,
            waits: &'a BTreeMap<CommandId, ProviderWaitRecord>,
            last_settlement: &'a Option<ProviderInputSettlement>,
        }
        Wire {
            current_turn: &self.current_turn,
            open_question: &self.open_question,
            open_approval: &self.open_approval,
            question_winners: &self.question_winners,
            approval_winners: &self.approval_winners,
            waits: &self.waits,
            last_settlement: &self.last_settlement,
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for ProviderSessionProjection {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            current_turn: Option<TurnId>,
            open_question: Option<QuestionId>,
            open_approval: Option<ApprovalId>,
            question_winners: BoundedProjectionMap<
                QuestionId,
                ProviderResolutionWinner,
                MAX_PROVIDER_QUESTION_WINS,
            >,
            approval_winners: BoundedProjectionMap<
                ApprovalId,
                ProviderResolutionWinner,
                MAX_PROVIDER_APPROVAL_WINS,
            >,
            waits: BoundedProjectionMap<CommandId, ProviderWaitRecord, MAX_PROVIDER_WAITS>,
            #[serde(default)]
            last_settlement: Option<ProviderInputSettlement>,
        }
        let wire = Wire::deserialize(deserializer)?;
        let projection = Self {
            current_turn: wire.current_turn,
            open_question: wire.open_question,
            open_approval: wire.open_approval,
            question_winners: wire.question_winners.0,
            approval_winners: wire.approval_winners.0,
            waits: wire.waits.0,
            last_settlement: wire.last_settlement,
        };
        projection.validate_bounds().map_err(de::Error::custom)?;
        Ok(projection)
    }
}

impl ProviderSessionProjection {
    pub fn validate_bounds(&self) -> Result<(), ProviderInputIntentError> {
        if self.question_winners.len() > MAX_PROVIDER_QUESTION_WINS {
            return Err(ProviderInputIntentError::QuestionWinnerLimit);
        }
        if self.approval_winners.len() > MAX_PROVIDER_APPROVAL_WINS {
            return Err(ProviderInputIntentError::ApprovalWinnerLimit);
        }
        if self.waits.len() > MAX_PROVIDER_WAITS {
            return Err(ProviderInputIntentError::WaitLimit);
        }
        Ok(())
    }

    pub(crate) fn bounded_insert_question_winner(
        &mut self,
        question_id: QuestionId,
        winner: ProviderResolutionWinner,
    ) -> Result<(), ProviderInputIntentError> {
        if !self.question_winners.contains_key(&question_id)
            && self.question_winners.len() >= MAX_PROVIDER_QUESTION_WINS
        {
            return Err(ProviderInputIntentError::QuestionWinnerLimit);
        }
        self.question_winners.insert(question_id, winner);
        self.open_question = None;
        Ok(())
    }

    pub(crate) fn bounded_insert_approval_winner(
        &mut self,
        approval_id: ApprovalId,
        winner: ProviderResolutionWinner,
    ) -> Result<(), ProviderInputIntentError> {
        if !self.approval_winners.contains_key(&approval_id)
            && self.approval_winners.len() >= MAX_PROVIDER_APPROVAL_WINS
        {
            return Err(ProviderInputIntentError::ApprovalWinnerLimit);
        }
        self.approval_winners.insert(approval_id, winner);
        self.open_approval = None;
        Ok(())
    }

    pub(crate) fn bounded_insert_wait(
        &mut self,
        command_id: CommandId,
        record: ProviderWaitRecord,
    ) -> Result<(), ProviderInputIntentError> {
        if !self.waits.contains_key(&command_id) && record.pending {
            let pending = self.waits.values().filter(|record| record.pending).count();
            if pending >= MAX_PROVIDER_WAITS {
                return Err(ProviderInputIntentError::WaitLimit);
            }
            if self.waits.len() >= MAX_PROVIDER_WAITS {
                let reclaim = self
                    .waits
                    .iter()
                    .find(|(_, record)| !record.pending)
                    .map(|(id, _)| *id);
                if let Some(reclaim) = reclaim {
                    self.waits.remove(&reclaim);
                } else {
                    return Err(ProviderInputIntentError::WaitLimit);
                }
            }
        }
        self.waits.insert(command_id, record);
        Ok(())
    }
}

pub fn validate_provider_text(text: &str) -> Result<(), ProviderInputIntentError> {
    if text.is_empty() {
        return Err(ProviderInputIntentError::EmptyText);
    }
    if text.len() > MAX_PROVIDER_INPUT_TEXT_BYTES {
        return Err(ProviderInputIntentError::TextTooLarge);
    }
    Ok(())
}

fn deserialize_provider_text<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<String, D::Error> {
    struct TextVisitor;
    impl Visitor<'_> for TextVisitor {
        type Value = String;

        fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
            write!(
                formatter,
                "non-empty provider input text of at most {MAX_PROVIDER_INPUT_TEXT_BYTES} bytes"
            )
        }

        fn visit_str<E: de::Error>(self, value: &str) -> Result<Self::Value, E> {
            validate_provider_text(value).map_err(E::custom)?;
            Ok(value.to_string())
        }

        fn visit_string<E: de::Error>(self, value: String) -> Result<Self::Value, E> {
            validate_provider_text(&value).map_err(E::custom)?;
            Ok(value)
        }
    }
    deserializer.deserialize_str(TextVisitor)
}

pub(crate) fn deserialize_optional_provider_text<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<String>, D::Error> {
    let value = Option::<String>::deserialize(deserializer)?;
    if let Some(ref text) = value {
        validate_provider_text(text).map_err(de::Error::custom)?;
    }
    Ok(value)
}

pub fn validate_action_nested_ids(
    action: &ProviderInputAction,
    question_id: Option<QuestionId>,
    approval_id: Option<ApprovalId>,
) -> Result<(), ProviderInputIntentError> {
    match action {
        ProviderInputAction::SendNow { text, .. }
        | ProviderInputAction::SteerCurrentTurn { text }
        | ProviderInputAction::QueueFollowUp { text } => {
            validate_provider_text(text)?;
            if question_id.is_some() || approval_id.is_some() {
                return Err(ProviderInputIntentError::InconsistentNestedIds);
            }
        }
        ProviderInputAction::AnswerQuestion {
            question_id: nested,
            answer,
        } => {
            validate_provider_text(answer)?;
            if question_id != Some(*nested) || approval_id.is_some() {
                return Err(ProviderInputIntentError::InconsistentNestedIds);
            }
        }
        ProviderInputAction::ResolveApproval {
            approval_id: nested,
            ..
        } => {
            if approval_id != Some(*nested) || question_id.is_some() {
                return Err(ProviderInputIntentError::InconsistentNestedIds);
            }
        }
        ProviderInputAction::StopTurn => {
            if question_id.is_some() || approval_id.is_some() {
                return Err(ProviderInputIntentError::InconsistentNestedIds);
            }
        }
    }
    Ok(())
}

/// Journal-only present. Host `ClientRequest` rejects this command.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PresentProviderQuestionIntent {
    agent_session_id: AgentSessionId,
    runtime_generation: u64,
    turn_id: TurnId,
    action_epoch: u64,
    question_id: QuestionId,
}

impl PresentProviderQuestionIntent {
    pub fn try_new(
        agent_session_id: AgentSessionId,
        runtime_generation: u64,
        turn_id: TurnId,
        action_epoch: u64,
        question_id: QuestionId,
    ) -> Result<Self, ProviderInputIntentError> {
        Ok(Self {
            agent_session_id,
            runtime_generation,
            turn_id,
            action_epoch,
            question_id,
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

    pub fn question_id(&self) -> QuestionId {
        self.question_id
    }
}

impl<'de> Deserialize<'de> for PresentProviderQuestionIntent {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            agent_session_id: AgentSessionId,
            runtime_generation: u64,
            turn_id: TurnId,
            action_epoch: u64,
            question_id: QuestionId,
        }
        let wire = Wire::deserialize(deserializer)?;
        Self::try_new(
            wire.agent_session_id,
            wire.runtime_generation,
            wire.turn_id,
            wire.action_epoch,
            wire.question_id,
        )
        .map_err(de::Error::custom)
    }
}

/// Journal-only present. Host `ClientRequest` rejects this command.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PresentProviderApprovalIntent {
    agent_session_id: AgentSessionId,
    runtime_generation: u64,
    turn_id: TurnId,
    action_epoch: u64,
    approval_id: ApprovalId,
}

impl PresentProviderApprovalIntent {
    pub fn try_new(
        agent_session_id: AgentSessionId,
        runtime_generation: u64,
        turn_id: TurnId,
        action_epoch: u64,
        approval_id: ApprovalId,
    ) -> Result<Self, ProviderInputIntentError> {
        Ok(Self {
            agent_session_id,
            runtime_generation,
            turn_id,
            action_epoch,
            approval_id,
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

    pub fn approval_id(&self) -> ApprovalId {
        self.approval_id
    }
}

impl<'de> Deserialize<'de> for PresentProviderApprovalIntent {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            agent_session_id: AgentSessionId,
            runtime_generation: u64,
            turn_id: TurnId,
            action_epoch: u64,
            approval_id: ApprovalId,
        }
        let wire = Wire::deserialize(deserializer)?;
        Self::try_new(
            wire.agent_session_id,
            wire.runtime_generation,
            wire.turn_id,
            wire.action_epoch,
            wire.approval_id,
        )
        .map_err(de::Error::custom)
    }
}

/// Journal-only wait settle. Host `ClientRequest` rejects this command.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SettleProviderWaitIntent {
    fence: ProviderWaitFence,
}

impl SettleProviderWaitIntent {
    pub fn try_new(fence: ProviderWaitFence) -> Result<Self, ProviderInputIntentError> {
        Ok(Self { fence })
    }

    pub fn fence(&self) -> &ProviderWaitFence {
        &self.fence
    }
}

impl<'de> Deserialize<'de> for SettleProviderWaitIntent {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            fence: ProviderWaitFence,
        }
        let wire = Wire::deserialize(deserializer)?;
        Self::try_new(wire.fence).map_err(de::Error::custom)
    }
}
