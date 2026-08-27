//! Provider input action ids, capability gate, bounded sequencer, and availability.
//!
//! Durable acceptance lives on `Command::SubmitProviderInput` through the
//! kernel transaction/event path. This module does not keep an in-memory
//! authority and does not advertise actions that cannot execute.
//!
//! Bounded provider input may be shaped here. An in-memory plan or identity
//! bind is **not** delivery. Settlement requires a live managed-session
//! [`ProviderInputWriteReceipt`]. A digest or caller-bound identity must never
//! become `ProviderInputDelivered`.

use crate::domain::operation::ResourceFence;
use crate::domain::provider_input::ProviderInputAction;
use crate::domain::snapshot::TaskSnapshot;
use crate::domain::{
    AgentSessionId, ApprovalId, ClientId, CommandId, OperationId, ProviderKind, ProviderSessionId,
    QuestionId, TaskId, TurnId,
};
use crate::process::registry::ManagedProcessFence;
use crate::protocol::{Capability, CapabilitySet};
use crate::providers::adapter::{ProviderInput, ProviderInputError};
use crate::state::SessionKind;
use std::fmt;

pub const ACTION_PROVIDER_SEND_NOW: &str = "provider.send_now";
pub const ACTION_PROVIDER_STEER_CURRENT_TURN: &str = "provider.steer_current_turn";
pub const ACTION_PROVIDER_QUEUE_FOLLOW_UP: &str = "provider.queue_follow_up";
pub const ACTION_PROVIDER_ANSWER_QUESTION: &str = "provider.answer_question";
pub const ACTION_PROVIDER_RESOLVE_APPROVAL: &str = "provider.resolve_approval";
pub const ACTION_PROVIDER_STOP_TURN: &str = "provider.stop_turn";
pub const ACTION_PROVIDER_TERMINAL_INPUT: &str = "provider.terminal_input";
pub const ACTION_PROVIDER_NEW_CONVERSATION: &str = "provider.new_conversation";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderActionUnavailable {
    NewConversationRequiresProviderRuntime,
}

impl ProviderActionUnavailable {
    pub const fn action_id(self) -> &'static str {
        match self {
            Self::NewConversationRequiresProviderRuntime => ACTION_PROVIDER_NEW_CONVERSATION,
        }
    }

    pub const fn reason_code(self) -> &'static str {
        match self {
            Self::NewConversationRequiresProviderRuntime => "provider_runtime_not_wired",
        }
    }
}

pub fn new_conversation_availability() -> ProviderActionUnavailable {
    ProviderActionUnavailable::NewConversationRequiresProviderRuntime
}

pub fn provider_input_capability() -> Capability {
    Capability::ProviderInput
}

pub fn provider_input_capability_selected(capabilities: CapabilitySet) -> bool {
    capabilities.contains(Capability::ProviderInput)
}

/// When the new provider-input capability is selected, Claude/Codex composer
/// must not write raw PTY bytes. Shell/server/SSH stay on the legacy path.
pub fn raw_pty_composer_forbidden(capability_selected: bool, session_kind: SessionKind) -> bool {
    capability_selected && session_kind.is_ai()
}

pub const RAW_PTY_COMPOSER_FORBIDDEN_REASON: &str =
    "Provider input must use the fenced sequencer; raw PTY composer is disabled.";

/// Cross-layer destination/outbox settlement remains outside provider authority.
/// Missing owner: a provider-owned session write capability that returns a
/// typed receipt bound to the live process/session fence and the exact Effect
/// action/bytes. Raw PTY writes are not that owner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderInputBridgeHold {
    DestinationOutboxAbsent,
    ProviderRuntimeAuthorityAbsent,
}

impl ProviderInputBridgeHold {
    pub const fn reason_code(self) -> &'static str {
        match self {
            Self::DestinationOutboxAbsent => "destination_adapter_not_wired",
            Self::ProviderRuntimeAuthorityAbsent => "provider_runtime_write_authority_absent",
        }
    }
}

/// Bounded input accepted by the provider-owned sequencer. Never a raw PTY write
/// and never a delivery receipt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderInputDeliveryPlan {
    action_id: &'static str,
    input: ProviderInput,
}

impl ProviderInputDeliveryPlan {
    pub const fn action_id(&self) -> &'static str {
        self.action_id
    }

    pub fn input(&self) -> &ProviderInput {
        &self.input
    }

    pub fn as_bytes(&self) -> &[u8] {
        self.input.as_bytes()
    }

    /// Plans never claim delivery.
    pub const fn settlement_hold(&self) -> ProviderInputBridgeHold {
        ProviderInputBridgeHold::DestinationOutboxAbsent
    }
}

/// Claimed dispatch identity for a future provider-owned write. Binding this
/// value does not authorize settlement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderInputDeliveryIdentity {
    pub task_id: TaskId,
    pub operation_id: OperationId,
    pub command_id: CommandId,
    pub client_id: ClientId,
    pub agent_session_id: AgentSessionId,
    pub provider_kind: ProviderKind,
    pub provider_session_id: Option<ProviderSessionId>,
    pub runtime_generation: u64,
    pub action_epoch: u64,
    pub turn_id: TurnId,
    pub question_id: Option<QuestionId>,
    pub approval_id: Option<ApprovalId>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderInputDeliveryError {
    SessionNotBound,
    StaleGeneration,
    StaleFence,
    ProviderMismatch,
    ActionMismatch,
    BytesMismatch,
    RuntimeAuthorityAbsent,
    /// Action has no proven physical provider interaction yet.
    UnsupportedAction,
    /// At least one required physical write may have crossed; never retry.
    PostBoundaryFailure,
}

/// One distinct PTY write in a provider composer submit sequence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProviderComposerWriteStep {
    bytes: Vec<u8>,
    delay_after: Option<std::time::Duration>,
}

impl ProviderComposerWriteStep {
    pub(crate) fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    pub(crate) fn delay_after(&self) -> Option<std::time::Duration> {
        self.delay_after
    }
}

/// Proven Claude/Codex composer submit sequence. It mirrors the established
/// terminal-composer timing: distinct text and Enter writes, and
/// typed slash-command tokens that cannot be collapsed into a ConPTY paste.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProviderComposerSubmitPlan {
    steps: Vec<ProviderComposerWriteStep>,
}

impl ProviderComposerSubmitPlan {
    pub(crate) fn steps(&self) -> &[ProviderComposerWriteStep] {
        &self.steps
    }
}

/// Build the physical write sequence for a provider input action. Approval
/// resolution fails closed because it has no provider-neutral key sequence;
/// StopTurn uses the standard interrupt byte instead of typing a placeholder.
pub(crate) fn provider_composer_submit_plan(
    provider_kind: ProviderKind,
    action: &ProviderInputAction,
) -> Result<ProviderComposerSubmitPlan, ProviderInputDeliveryError> {
    if matches!(provider_kind, ProviderKind::Cursor) {
        return Err(ProviderInputDeliveryError::UnsupportedAction);
    }
    if let ProviderInputAction::TerminalInput { text } = action {
        if text.is_empty() {
            return Err(ProviderInputDeliveryError::BytesMismatch);
        }
        return Ok(ProviderComposerSubmitPlan {
            steps: vec![ProviderComposerWriteStep {
                bytes: text.as_bytes().to_vec(),
                delay_after: None,
            }],
        });
    }
    let text = match action {
        ProviderInputAction::SendNow { text, .. }
        | ProviderInputAction::SteerCurrentTurn { text }
        | ProviderInputAction::QueueFollowUp { text } => text.as_bytes(),
        ProviderInputAction::AnswerQuestion { answer, .. } => answer.as_bytes(),
        ProviderInputAction::StopTurn => {
            return Ok(ProviderComposerSubmitPlan {
                steps: vec![ProviderComposerWriteStep {
                    bytes: vec![0x03],
                    delay_after: None,
                }],
            });
        }
        ProviderInputAction::ResolveApproval { .. } => {
            return Err(ProviderInputDeliveryError::UnsupportedAction);
        }
        ProviderInputAction::TerminalInput { .. } => unreachable!("handled above"),
    };
    if text.is_empty() {
        return Err(ProviderInputDeliveryError::BytesMismatch);
    }

    let mut steps = Vec::new();
    let text = std::str::from_utf8(text).map_err(|_| ProviderInputDeliveryError::BytesMismatch)?;
    let trimmed = text.trim_start();
    let slash_command = trimmed.starts_with('/');
    if slash_command {
        let leading_len = text.len() - trimmed.len();
        if leading_len > 0 {
            steps.push(ProviderComposerWriteStep {
                bytes: text.as_bytes()[..leading_len].to_vec(),
                delay_after: None,
            });
        }
        let token_end = trimmed.find(char::is_whitespace).unwrap_or(trimmed.len());
        let token = &trimmed[..token_end];
        let token_char_count = token.chars().count();
        for (index, character) in token.chars().enumerate() {
            let mut encoded = [0_u8; 4];
            let is_last = index + 1 == token_char_count;
            steps.push(ProviderComposerWriteStep {
                bytes: character.encode_utf8(&mut encoded).as_bytes().to_vec(),
                delay_after: Some(std::time::Duration::from_millis(
                    if is_last && token_end == trimmed.len() {
                        350
                    } else {
                        100
                    },
                )),
            });
        }
        if token_end < trimmed.len() {
            steps.push(ProviderComposerWriteStep {
                bytes: trimmed.as_bytes()[token_end..].to_vec(),
                delay_after: Some(std::time::Duration::from_millis(250)),
            });
        }
        steps.push(ProviderComposerWriteStep {
            bytes: b" ".to_vec(),
            delay_after: Some(std::time::Duration::from_millis(500)),
        });
    } else {
        steps.push(ProviderComposerWriteStep {
            bytes: text.as_bytes().to_vec(),
            delay_after: Some(std::time::Duration::from_millis(50)),
        });
    }
    if matches!(provider_kind, ProviderKind::Codex) && !slash_command {
        // A bulk write leaves the Codex TUI composer in multiline mode. Exit
        // that mode before Enter so the prompt is submitted instead of merely
        // gaining a newline. Do not send a leading Escape: that can dismiss an
        // unrelated live Codex surface before the prompt is written.
        steps.push(ProviderComposerWriteStep {
            bytes: b"\x1b".to_vec(),
            delay_after: Some(std::time::Duration::from_millis(120)),
        });
    }
    let claude_exact_slash = slash_command
        && matches!(provider_kind, ProviderKind::ClaudeCode)
        && trimmed[token_end(trimmed)..].trim().is_empty();
    steps.push(ProviderComposerWriteStep {
        bytes: b"\r".to_vec(),
        delay_after: claude_exact_slash.then_some(std::time::Duration::from_millis(180)),
    });
    if claude_exact_slash {
        steps.push(ProviderComposerWriteStep {
            bytes: b"\r".to_vec(),
            delay_after: None,
        });
    }
    Ok(ProviderComposerSubmitPlan { steps })
}

fn token_end(text: &str) -> usize {
    text.find(char::is_whitespace).unwrap_or(text.len())
}

/// Opaque receipt issued only after a live managed session writes the exact
/// bounded [`ProviderInputAction`] bytes through its provider runtime handle.
#[derive(Clone, PartialEq, Eq)]
pub struct ProviderInputWriteReceipt {
    identity: ProviderInputDeliveryIdentity,
    action: ProviderInputAction,
    bytes: Vec<u8>,
    resource_fence: ResourceFence,
}

impl fmt::Debug for ProviderInputWriteReceipt {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProviderInputWriteReceipt")
            .field("task_id", &self.identity.task_id)
            .field("operation_id", &self.identity.operation_id)
            .field("runtime_generation", &self.identity.runtime_generation)
            .field("action_epoch", &self.identity.action_epoch)
            .field("bytes", &self.bytes.len())
            .finish_non_exhaustive()
    }
}

impl ProviderInputWriteReceipt {
    fn issue(
        identity: ProviderInputDeliveryIdentity,
        action: ProviderInputAction,
        bytes: Vec<u8>,
        resource_fence: ResourceFence,
    ) -> Result<Self, ProviderInputDeliveryError> {
        let expected = provider_input_action_bytes(&action)
            .map_err(|_| ProviderInputDeliveryError::BytesMismatch)?;
        if expected != bytes {
            return Err(ProviderInputDeliveryError::BytesMismatch);
        }
        if resource_fence.runtime_generation != identity.runtime_generation
            || resource_fence.runtime_generation == 0
        {
            return Err(ProviderInputDeliveryError::StaleFence);
        }
        Ok(Self {
            identity,
            action,
            bytes,
            resource_fence,
        })
    }

    pub fn identity(&self) -> &ProviderInputDeliveryIdentity {
        &self.identity
    }

    pub fn action(&self) -> &ProviderInputAction {
        &self.action
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    pub const fn resource_fence(&self) -> ResourceFence {
        self.resource_fence
    }
}

/// Live provider-owned write handle bound to one exact managed session fence.
pub struct ProviderRuntimeWriteHandle {
    identity: ProviderInputDeliveryIdentity,
    fence: ManagedProcessFence,
    writer: Box<dyn ProviderRuntimeByteWriter>,
}

pub(crate) trait ProviderRuntimeByteWriter: Send {
    /// Deliver a bounded provider action through the live managed session.
    /// Implementations must perform every required physical write (prompt plus
    /// distinct submit, plus provider-specific control keys) before returning
    /// Ok. Logical `bytes` are the action payload, not the full physical PTY
    /// sequence.
    fn write_provider_action(
        &self,
        fence: &ManagedProcessFence,
        identity: &ProviderInputDeliveryIdentity,
        action: &ProviderInputAction,
        logical_bytes: &[u8],
    ) -> Result<(), ProviderInputDeliveryError>;
}

impl ProviderRuntimeWriteHandle {
    pub(crate) fn bind(
        identity: ProviderInputDeliveryIdentity,
        fence: ManagedProcessFence,
        writer: Box<dyn ProviderRuntimeByteWriter>,
    ) -> Result<Self, ProviderInputDeliveryError> {
        if identity.runtime_generation == 0
            || fence.resource().runtime_generation != identity.runtime_generation
        {
            return Err(ProviderInputDeliveryError::StaleFence);
        }
        Ok(Self {
            identity,
            fence,
            writer,
        })
    }

    pub fn bound_identity(&self) -> &ProviderInputDeliveryIdentity {
        &self.identity
    }

    pub fn fence(&self) -> &ManagedProcessFence {
        &self.fence
    }

    /// Writes bounded action bytes through the live managed session. A receipt
    /// is issued only after that write succeeds for the exact identity/action.
    pub fn write_action(
        &self,
        identity: &ProviderInputDeliveryIdentity,
        action: &ProviderInputAction,
        plan: &ProviderInputDeliveryPlan,
    ) -> Result<ProviderInputWriteReceipt, ProviderInputDeliveryError> {
        if identity.provider_kind != self.identity.provider_kind
            || identity.agent_session_id != self.identity.agent_session_id
            || identity.provider_session_id != self.identity.provider_session_id
        {
            return Err(ProviderInputDeliveryError::ProviderMismatch);
        }
        if identity.runtime_generation != self.identity.runtime_generation
            || self.fence.resource().runtime_generation != identity.runtime_generation
        {
            return Err(ProviderInputDeliveryError::StaleGeneration);
        }
        if identity != &self.identity {
            return Err(ProviderInputDeliveryError::StaleFence);
        }
        if plan.action_id() != provider_input_action_id(action) {
            return Err(ProviderInputDeliveryError::ActionMismatch);
        }
        let expected = provider_input_action_bytes(action)
            .map_err(|_| ProviderInputDeliveryError::BytesMismatch)?;
        if plan.as_bytes() != expected.as_slice() {
            return Err(ProviderInputDeliveryError::BytesMismatch);
        }
        // Fail closed for actions without a proven physical interaction before
        // any managed-session write occurs.
        provider_composer_submit_plan(identity.provider_kind, action)?;
        self.writer
            .write_provider_action(&self.fence, identity, action, plan.as_bytes())?;
        ProviderInputWriteReceipt::issue(
            identity.clone(),
            action.clone(),
            expected,
            self.fence.resource(),
        )
    }
}

pub fn provider_input_action_id(action: &ProviderInputAction) -> &'static str {
    match action {
        ProviderInputAction::SendNow { .. } => ACTION_PROVIDER_SEND_NOW,
        ProviderInputAction::SteerCurrentTurn { .. } => ACTION_PROVIDER_STEER_CURRENT_TURN,
        ProviderInputAction::QueueFollowUp { .. } => ACTION_PROVIDER_QUEUE_FOLLOW_UP,
        ProviderInputAction::AnswerQuestion { .. } => ACTION_PROVIDER_ANSWER_QUESTION,
        ProviderInputAction::ResolveApproval { .. } => ACTION_PROVIDER_RESOLVE_APPROVAL,
        ProviderInputAction::TerminalInput { .. } => ACTION_PROVIDER_TERMINAL_INPUT,
        ProviderInputAction::StopTurn => ACTION_PROVIDER_STOP_TURN,
    }
}

pub fn provider_input_action_bytes(
    action: &ProviderInputAction,
) -> Result<Vec<u8>, ProviderInputError> {
    let bytes = match action {
        ProviderInputAction::SendNow { text, .. }
        | ProviderInputAction::SteerCurrentTurn { text }
        | ProviderInputAction::QueueFollowUp { text } => text.as_bytes().to_vec(),
        ProviderInputAction::AnswerQuestion { answer, .. } => answer.as_bytes().to_vec(),
        ProviderInputAction::ResolveApproval { allow, .. } => {
            if *allow {
                b"allow".to_vec()
            } else {
                b"deny".to_vec()
            }
        }
        ProviderInputAction::TerminalInput { text } => text.as_bytes().to_vec(),
        ProviderInputAction::StopTurn => b"stop_turn".to_vec(),
    };
    Ok(ProviderInput::new(bytes)?.as_bytes().to_vec())
}

pub fn sequence_provider_action(
    action: &ProviderInputAction,
) -> Result<ProviderInputDeliveryPlan, ProviderInputError> {
    sequence_bounded_input(
        provider_input_action_id(action),
        provider_input_action_bytes(action)?,
    )
}

/// Identity bind only. It cannot issue delivery proof: there is no
/// provider-owned runtime write capability to acknowledge.
pub struct BoundProviderInputPort {
    bound: ProviderInputDeliveryIdentity,
}

impl BoundProviderInputPort {
    pub fn bind(bound: ProviderInputDeliveryIdentity) -> Self {
        Self { bound }
    }

    pub fn bound_identity(&self) -> &ProviderInputDeliveryIdentity {
        &self.bound
    }

    /// Rejects delivery. A bind/hash is not a write receipt.
    pub fn deliver(
        &mut self,
        identity: &ProviderInputDeliveryIdentity,
        _plan: &ProviderInputDeliveryPlan,
    ) -> Result<ProviderInputBridgeHold, ProviderInputDeliveryError> {
        if identity.provider_kind != self.bound.provider_kind
            || identity.agent_session_id != self.bound.agent_session_id
            || identity.provider_session_id != self.bound.provider_session_id
        {
            return Err(ProviderInputDeliveryError::ProviderMismatch);
        }
        if identity.runtime_generation != self.bound.runtime_generation {
            return Err(ProviderInputDeliveryError::StaleGeneration);
        }
        if identity != &self.bound {
            return Err(ProviderInputDeliveryError::StaleFence);
        }
        Err(ProviderInputDeliveryError::RuntimeAuthorityAbsent)
    }
}

/// Bind-only path: never returns a settlement credential.
pub fn deliver_through_capability(
    port: &mut BoundProviderInputPort,
    identity: ProviderInputDeliveryIdentity,
    plan: ProviderInputDeliveryPlan,
) -> Result<ProviderInputBridgeHold, ProviderInputDeliveryError> {
    port.deliver(&identity, &plan)
}

/// Sequence a bounded provider text action. Rejects empty/oversized payloads.
/// The returned plan is not delivered; call [`delivery_settlement_hold`].
pub fn sequence_bounded_input(
    action_id: &'static str,
    text: impl Into<Vec<u8>>,
) -> Result<ProviderInputDeliveryPlan, ProviderInputError> {
    match action_id {
        ACTION_PROVIDER_SEND_NOW
        | ACTION_PROVIDER_STEER_CURRENT_TURN
        | ACTION_PROVIDER_QUEUE_FOLLOW_UP
        | ACTION_PROVIDER_ANSWER_QUESTION
        | ACTION_PROVIDER_RESOLVE_APPROVAL
        | ACTION_PROVIDER_TERMINAL_INPUT
        | ACTION_PROVIDER_STOP_TURN => {}
        _ => return Err(ProviderInputError::Empty),
    }
    let input = ProviderInput::new(text)?;
    Ok(ProviderInputDeliveryPlan { action_id, input })
}

/// Sequencing alone never claims Delivered. Bind-only ports also stay HOLD.
pub fn delivery_settlement_hold() -> ProviderInputBridgeHold {
    ProviderInputBridgeHold::DestinationOutboxAbsent
}

/// Later destination/outbox union (not wired): a provider-owned write receipt
/// must bind live process/session fence plus Effect action kind and bounded
/// bytes before `KernelStore` may emit `ProviderInputDelivered`.

pub fn available_action_ids(
    snapshot: &TaskSnapshot,
    agent_session_id: AgentSessionId,
) -> Vec<&'static str> {
    let Some(agent) = snapshot.agents.get(&agent_session_id) else {
        return Vec::new();
    };
    if !matches!(
        agent.lifecycle,
        crate::domain::agent::AgentSessionLifecycle::Open
    ) {
        return Vec::new();
    }
    let session = snapshot
        .provider_sessions
        .get(&agent_session_id)
        .cloned()
        .unwrap_or_default();
    let mut ids = vec![ACTION_PROVIDER_SEND_NOW];
    if session.current_turn.is_some() {
        ids.push(ACTION_PROVIDER_STEER_CURRENT_TURN);
        ids.push(ACTION_PROVIDER_QUEUE_FOLLOW_UP);
        ids.push(ACTION_PROVIDER_TERMINAL_INPUT);
        ids.push(ACTION_PROVIDER_STOP_TURN);
    }
    if session.open_question.is_some() && session.current_turn.is_some() {
        ids.push(ACTION_PROVIDER_ANSWER_QUESTION);
    }
    if session.open_approval.is_some() && session.current_turn.is_some() {
        ids.push(ACTION_PROVIDER_RESOLVE_APPROVAL);
    }
    ids
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::adapter::MAX_PROVIDER_INPUT_BYTES;

    fn test_identity(generation: u64) -> ProviderInputDeliveryIdentity {
        ProviderInputDeliveryIdentity {
            task_id: crate::domain::TaskId::parse("018f60b0-9c1a-7001-8000-00000000000b").unwrap(),
            operation_id: crate::domain::OperationId::parse("018f60b0-9c1a-7001-8000-000000000031")
                .unwrap(),
            command_id: crate::domain::CommandId::parse("018f60b0-9c1a-7001-8000-000000000032")
                .unwrap(),
            client_id: crate::domain::ClientId::parse("018f60b0-9c1a-7001-8000-000000000033")
                .unwrap(),
            agent_session_id: AgentSessionId::parse("018f60b0-9c1a-7001-8000-000000000021")
                .unwrap(),
            provider_kind: crate::providers::ProviderKind::Codex,
            provider_session_id: Some(
                crate::domain::ProviderSessionId::new("codex-session-1").unwrap(),
            ),
            runtime_generation: generation,
            action_epoch: 4,
            turn_id: crate::domain::TurnId::parse("018f60b0-9c1a-7001-8000-000000000034").unwrap(),
            question_id: None,
            approval_id: None,
        }
    }

    #[test]
    fn bind_and_plan_cannot_manufacture_delivery() {
        let plan = sequence_bounded_input(ACTION_PROVIDER_SEND_NOW, b"hello".to_vec())
            .expect("bounded input");
        assert_eq!(
            plan.settlement_hold(),
            ProviderInputBridgeHold::DestinationOutboxAbsent
        );
        let identity = test_identity(3);
        let mut port = BoundProviderInputPort::bind(identity.clone());
        assert_eq!(
            deliver_through_capability(&mut port, identity.clone(), plan.clone()),
            Err(ProviderInputDeliveryError::RuntimeAuthorityAbsent)
        );
        let mut stale = identity;
        stale.runtime_generation = 4;
        assert_eq!(
            deliver_through_capability(&mut port, stale, plan),
            Err(ProviderInputDeliveryError::StaleGeneration)
        );
    }

    #[test]
    fn sequencer_shapes_bounded_input_but_settlement_remains_hold() {
        let plan = sequence_bounded_input(ACTION_PROVIDER_SEND_NOW, b"hello".to_vec())
            .expect("bounded input");
        assert_eq!(plan.action_id(), ACTION_PROVIDER_SEND_NOW);
        assert_eq!(plan.as_bytes(), b"hello");
        assert_eq!(
            plan.settlement_hold(),
            ProviderInputBridgeHold::DestinationOutboxAbsent
        );
        assert_eq!(
            delivery_settlement_hold(),
            ProviderInputBridgeHold::DestinationOutboxAbsent
        );
    }

    #[test]
    fn sequencer_rejects_empty_and_oversized_input() {
        assert!(matches!(
            sequence_bounded_input(ACTION_PROVIDER_SEND_NOW, Vec::new()),
            Err(ProviderInputError::Empty)
        ));
        let oversized = vec![b'a'; MAX_PROVIDER_INPUT_BYTES + 1];
        assert!(matches!(
            sequence_bounded_input(ACTION_PROVIDER_SEND_NOW, oversized),
            Err(ProviderInputError::TooLarge)
        ));
    }

    #[test]
    fn composer_submit_plan_preserves_distinct_provider_key_boundaries() {
        let action = ProviderInputAction::SendNow {
            text: "hello".into(),
            wait: false,
        };
        let claude =
            provider_composer_submit_plan(ProviderKind::ClaudeCode, &action).expect("claude plan");
        assert_eq!(
            claude
                .steps()
                .iter()
                .map(ProviderComposerWriteStep::bytes)
                .collect::<Vec<_>>(),
            vec![b"hello".as_slice(), b"\r".as_slice()]
        );

        let codex =
            provider_composer_submit_plan(ProviderKind::Codex, &action).expect("codex plan");
        assert_eq!(
            codex
                .steps()
                .iter()
                .map(ProviderComposerWriteStep::bytes)
                .collect::<Vec<_>>(),
            vec![b"hello".as_slice(), b"\x1b".as_slice(), b"\r".as_slice()]
        );
    }

    #[test]
    fn composer_submit_plan_supports_interrupt_and_fails_closed_for_unproven_approval() {
        let stop =
            provider_composer_submit_plan(ProviderKind::ClaudeCode, &ProviderInputAction::StopTurn)
                .expect("stop plan");
        assert_eq!(stop.steps()[0].bytes(), &[0x03]);
        assert_eq!(
            provider_composer_submit_plan(
                ProviderKind::ClaudeCode,
                &ProviderInputAction::ResolveApproval {
                    approval_id: crate::domain::ApprovalId::new(),
                    allow: true,
                },
            ),
            Err(ProviderInputDeliveryError::UnsupportedAction)
        );
    }

    #[test]
    fn terminal_input_plan_preserves_interactive_control_bytes_without_submit() {
        let action = ProviderInputAction::TerminalInput {
            text: "\u{1b}[B".into(),
        };
        let plan = provider_composer_submit_plan(ProviderKind::ClaudeCode, &action)
            .expect("terminal input plan");
        assert_eq!(
            provider_input_action_id(&action),
            ACTION_PROVIDER_TERMINAL_INPUT
        );
        assert_eq!(provider_input_action_bytes(&action).unwrap(), b"\x1b[B");
        assert_eq!(plan.steps().len(), 1);
        assert_eq!(plan.steps()[0].bytes(), b"\x1b[B");
        assert_eq!(plan.steps()[0].delay_after(), None);
    }

    #[test]
    fn live_write_handle_rejects_stale_action_and_bytes() {
        struct RejectingWriter;
        impl ProviderRuntimeByteWriter for RejectingWriter {
            fn write_provider_action(
                &self,
                _fence: &crate::process::registry::ManagedProcessFence,
                _identity: &ProviderInputDeliveryIdentity,
                _action: &crate::domain::provider_input::ProviderInputAction,
                _logical_bytes: &[u8],
            ) -> Result<(), ProviderInputDeliveryError> {
                panic!("mismatched action/bytes must not write");
            }
        }
        let identity = test_identity(3);
        let resource = crate::domain::ResourceId::parse("018f60b0-9c1a-7001-8000-000000000057")
            .expect("resource");
        let fence = crate::process::registry::ManagedProcessFence::new(
            crate::domain::operation::ResourceFence::new(resource, 3),
            crate::process::identity::ProcessOwner::Task(identity.task_id),
            crate::process::identity::ManagedProcessIdentity::new(
                crate::process::identity::ManagedProcessId::new(7, 11).expect("pid"),
                std::env::current_exe().expect("exe"),
            )
            .expect("identity"),
        );
        let handle =
            ProviderRuntimeWriteHandle::bind(identity.clone(), fence, Box::new(RejectingWriter))
                .expect("bind");
        let action = crate::domain::provider_input::ProviderInputAction::SendNow {
            text: "hello".into(),
            wait: false,
        };
        let plan = sequence_bounded_input(ACTION_PROVIDER_SEND_NOW, b"hello").expect("plan");
        let mut stale = identity.clone();
        stale.runtime_generation = 4;
        assert_eq!(
            handle.write_action(&stale, &action, &plan),
            Err(ProviderInputDeliveryError::StaleGeneration)
        );
        let wrong_action = crate::domain::provider_input::ProviderInputAction::StopTurn;
        assert_eq!(
            handle.write_action(&identity, &wrong_action, &plan),
            Err(ProviderInputDeliveryError::ActionMismatch)
        );
        let wrong_plan =
            sequence_bounded_input(ACTION_PROVIDER_SEND_NOW, b"other").expect("wrong plan");
        assert_eq!(
            handle.write_action(&identity, &action, &wrong_plan),
            Err(ProviderInputDeliveryError::BytesMismatch)
        );
    }

    #[test]
    fn new_conversation_remains_unavailable_without_host_runtime_wiring() {
        assert_eq!(
            new_conversation_availability(),
            ProviderActionUnavailable::NewConversationRequiresProviderRuntime
        );
    }

    #[test]
    fn raw_pty_composer_forbidden_for_ai_when_capability_selected() {
        assert!(raw_pty_composer_forbidden(true, SessionKind::Claude));
        assert!(!raw_pty_composer_forbidden(false, SessionKind::Claude));
        assert!(!raw_pty_composer_forbidden(true, SessionKind::Shell));
    }
}
