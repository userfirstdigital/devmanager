//! Provider input action ids, capability gate, and availability.
//!
//! Durable acceptance lives on `Command::SubmitProviderInput` through the
//! kernel transaction/event path. This module does not keep an in-memory
//! authority and does not advertise actions that cannot execute.
//!
//! Delivery is never reported as Delivered here. Until a destination/outbox
//! adapter exists, accepted intent stays `ProviderDeliveryVisibility::Hold`.

use crate::domain::snapshot::TaskSnapshot;
use crate::domain::AgentSessionId;
use crate::protocol::{Capability, CapabilitySet};
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
