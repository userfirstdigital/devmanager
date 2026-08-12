//! Provider input action ids, capability gate, bounded sequencer, and availability.
//!
//! Durable acceptance lives on `Command::SubmitProviderInput` through the
//! kernel transaction/event path. This module does not keep an in-memory
//! authority and does not advertise actions that cannot execute.
//!
//! Bounded provider input may be shaped here without a raw PTY composer bypass.
//! An in-memory delivery plan is **not** delivery: destination/outbox settlement
//! remains a cross-layer HOLD until the kernel effect path proves the write.
//! New conversation stays unavailable at this seam until host runtime wiring
//! exists beyond adapter registration alone.

use crate::domain::snapshot::TaskSnapshot;
use crate::domain::AgentSessionId;
use crate::protocol::{Capability, CapabilitySet};
use crate::providers::adapter::{ProviderInput, ProviderInputError};
use crate::state::SessionKind;

pub const ACTION_PROVIDER_SEND_NOW: &str = "provider.send_now";
pub const ACTION_PROVIDER_STEER_CURRENT_TURN: &str = "provider.steer_current_turn";
pub const ACTION_PROVIDER_QUEUE_FOLLOW_UP: &str = "provider.queue_follow_up";
pub const ACTION_PROVIDER_ANSWER_QUESTION: &str = "provider.answer_question";
pub const ACTION_PROVIDER_RESOLVE_APPROVAL: &str = "provider.resolve_approval";
pub const ACTION_PROVIDER_STOP_TURN: &str = "provider.stop_turn";
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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderInputBridgeHold {
    DestinationOutboxAbsent,
}

impl ProviderInputBridgeHold {
    pub const fn reason_code(self) -> &'static str {
        match self {
            Self::DestinationOutboxAbsent => "destination_adapter_not_wired",
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

    /// Plans never claim delivery; settlement stays a typed HOLD.
    pub const fn settlement_hold(&self) -> ProviderInputBridgeHold {
        ProviderInputBridgeHold::DestinationOutboxAbsent
    }
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
        | ACTION_PROVIDER_STOP_TURN => {}
        _ => return Err(ProviderInputError::Empty),
    }
    let input = ProviderInput::new(text)?;
    Ok(ProviderInputDeliveryPlan { action_id, input })
}

/// Delivery settlement is still a cross-layer HOLD; provider sequencing alone
/// never claims Delivered.
pub fn delivery_settlement_hold() -> ProviderInputBridgeHold {
    ProviderInputBridgeHold::DestinationOutboxAbsent
}

/// Later destination/outbox union (not wired):
/// `DestinationClass::ProviderInput` +
/// `Effect::DeliverProviderInput { task_id, action_epoch, agent_session_id,
/// runtime_generation, turn_id, command_id, question_id, approval_id }` +
/// `ReplayPolicy::NoAutomaticRetry` → dispatch ambiguity `Uncertain`.

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
