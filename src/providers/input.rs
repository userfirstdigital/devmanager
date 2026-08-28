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
use crate::domain::provider_input::{
    ProviderImageAttachment, ProviderInputAction, MAX_PROVIDER_IMAGE_ATTACHMENTS,
};
use crate::domain::snapshot::TaskSnapshot;
use crate::domain::{
    AgentSessionId, ApprovalId, ClientId, CommandId, OperationId, ProviderKind, ProviderSessionId,
    QuestionId, TaskId, TurnId,
};
use crate::process::registry::ManagedProcessFence;
use crate::protocol::{Capability, CapabilitySet};
use crate::providers::adapter::{ProviderInput, ProviderInputError};
use crate::state::SessionKind;
use crate::terminal::session::TerminalScreenSnapshot;
use std::fmt;

/// Additional startup safety check after host capability and runtime fencing.
/// Observing a prompt never authorizes input without those independent checks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodexIdentitylessStartupReadiness {
    /// A known composer is visible; this is not standalone input authority.
    ChatComposerReady,
    /// Known blocking trust/setup screen — user must act on that host.
    ProviderSetupRequired,
    /// Empty or unrecognized startup screen — still pending.
    StartupPending,
}

pub(crate) fn terminal_text_lines_from_screen(screen: &TerminalScreenSnapshot) -> Vec<String> {
    screen
        .lines
        .iter()
        .map(|cells| cells.iter().map(|cell| cell.character).collect::<String>())
        .collect()
}

pub fn classify_codex_identityless_startup_readiness(
    text_lines: &[String],
) -> CodexIdentitylessStartupReadiness {
    if is_codex_trust_directory_screen(text_lines) {
        return CodexIdentitylessStartupReadiness::ProviderSetupRequired;
    }
    if is_codex_chat_composer_ready(text_lines) {
        return CodexIdentitylessStartupReadiness::ChatComposerReady;
    }
    CodexIdentitylessStartupReadiness::StartupPending
}

fn is_codex_trust_directory_screen(text_lines: &[String]) -> bool {
    text_lines.iter().any(|line| {
        line.to_ascii_lowercase()
            .contains("do you trust the contents of this directory")
    })
}

fn is_codex_chat_composer_ready(text_lines: &[String]) -> bool {
    for (index, line) in text_lines.iter().enumerate() {
        if codex_composer_placeholder_from_line(line, None)
            .is_some_and(|text| text == CODEX_COMPOSER_PLACEHOLDER)
        {
            return true;
        }
        if index + 1 < text_lines.len()
            && codex_composer_placeholder_from_line(line, Some(&text_lines[index + 1]))
                .is_some_and(|text| text == CODEX_COMPOSER_PLACEHOLDER)
        {
            return true;
        }
    }
    false
}

const CODEX_COMPOSER_PLACEHOLDER: &str = "askcodextodoanything";

fn codex_composer_placeholder_from_line(first: &str, second: Option<&str>) -> Option<String> {
    let first_trim = first.trim();
    let mut text = first_trim.strip_prefix('›')?.trim_start().to_string();
    if let Some(second) = second {
        text.push_str(second.trim());
    }
    Some(
        text.chars()
            .filter(|ch| !ch.is_whitespace())
            .flat_map(|ch| ch.to_lowercase())
            .collect(),
    )
}

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
///
/// Codex (and Claude, matching herdr's standalone image-path paste) attach
/// images when each absolute staged path is delivered as its own paste frame.
/// `@path` + prompt in one text write does not match that contract.
/// Claude may still treat the path as filesystem context rather than proven
/// inline vision; this path only mirrors the confirmed paste delivery shape.
///
/// Physical framing follows the live terminal's negotiated bracketed-paste
/// mode. Logical [`provider_input_action_bytes`] stay mode-independent.
pub(crate) fn provider_composer_submit_plan_for_mode(
    provider_kind: ProviderKind,
    action: &ProviderInputAction,
    bracketed_paste: bool,
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

    let (text, images) = match action {
        ProviderInputAction::SendNow { text, images, .. } => (text.as_str(), images.as_slice()),
        ProviderInputAction::SteerCurrentTurn { text }
        | ProviderInputAction::QueueFollowUp { text } => (text.as_str(), &[][..]),
        ProviderInputAction::AnswerQuestion { answer, .. } => (answer.as_str(), &[][..]),
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
    if images.len() > MAX_PROVIDER_IMAGE_ATTACHMENTS {
        return Err(ProviderInputDeliveryError::BytesMismatch);
    }
    if text.is_empty() && images.is_empty() {
        return Err(ProviderInputDeliveryError::BytesMismatch);
    }

    // Codex's Windows console input can turn an explicit paste into character
    // events. Its PasteBurst retains Enter-as-newline for 120ms after the last
    // character, even after the paste buffer flushes (60ms on Windows).
    // Keep the actual submit outside that window, with scheduling headroom.
    // Source: openai/codex, tui/src/bottom_pane/paste_burst.rs.
    let paste_settle =
        std::time::Duration::from_millis(if matches!(provider_kind, ProviderKind::Codex) {
            250
        } else {
            50
        });
    let mut steps = Vec::new();
    for image in images {
        steps.push(ProviderComposerWriteStep {
            bytes: encode_provider_paste_payload(image.path(), bracketed_paste)?,
            delay_after: Some(paste_settle),
        });
    }

    if !text.is_empty() {
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
            // Match herdr's encode_api_submission_parts: one complete paste
            // (bracketed only when the live terminal advertised the mode),
            // then a separate Enter. Never allow a prompt to terminate its
            // own paste frame when brackets are in use.
            let bytes = if matches!(provider_kind, ProviderKind::Codex) {
                encode_provider_paste_payload(text, bracketed_paste)?
            } else {
                text.as_bytes().to_vec()
            };
            steps.push(ProviderComposerWriteStep {
                bytes,
                delay_after: Some(paste_settle),
            });
        }

        let trimmed = text.trim_start();
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
    } else {
        // Each image paste is already terminated. Submit without a bare Escape.
        steps.push(ProviderComposerWriteStep {
            bytes: b"\r".to_vec(),
            delay_after: None,
        });
    }
    Ok(ProviderComposerSubmitPlan { steps })
}

/// Test/compat wrapper that assumes Codex advertises bracketed paste.
/// Production writers must call [`provider_composer_submit_plan_for_mode`] with
/// the live session's negotiated mode — never a fixed assumption.
#[cfg(test)]
pub(crate) fn provider_composer_submit_plan(
    provider_kind: ProviderKind,
    action: &ProviderInputAction,
) -> Result<ProviderComposerSubmitPlan, ProviderInputDeliveryError> {
    provider_composer_submit_plan_for_mode(
        provider_kind,
        action,
        matches!(provider_kind, ProviderKind::Codex),
    )
}

fn encode_provider_paste_payload(
    text: &str,
    bracketed_paste: bool,
) -> Result<Vec<u8>, ProviderInputDeliveryError> {
    // Reject paste-terminating escapes in both modes so a prompt cannot close
    // a later bracketed frame or inject mode-switch sequences.
    if text.contains(['\x1b', '\u{9b}']) {
        return Err(ProviderInputDeliveryError::BytesMismatch);
    }
    if bracketed_paste {
        Ok(bracketed_provider_paste(text))
    } else {
        Ok(text.as_bytes().to_vec())
    }
}

fn bracketed_provider_paste(text: &str) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(text.len().saturating_add(16));
    bytes.extend_from_slice(b"\x1b[200~");
    bytes.extend_from_slice(text.as_bytes());
    bytes.extend_from_slice(b"\x1b[201~");
    bytes
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
        // any managed-session write occurs. Framing mode is chosen later by the
        // sealed writer from the live terminal negotiation.
        provider_composer_submit_plan_for_mode(identity.provider_kind, action, false)?;
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
        ProviderInputAction::SendNow { text, images, .. } => {
            encode_send_now_action_bytes(text, images)?
        }
        ProviderInputAction::SteerCurrentTurn { text }
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

/// Legacy empty-image SendNow digests remain exact text bytes. Nonempty image
/// identity is mixed into the logical action bytes used by receipts/digests.
fn encode_send_now_action_bytes(
    text: &str,
    images: &[ProviderImageAttachment],
) -> Result<Vec<u8>, ProviderInputError> {
    if images.is_empty() {
        return Ok(text.as_bytes().to_vec());
    }
    let mut bytes = Vec::new();
    let text_len = u32::try_from(text.len()).map_err(|_| ProviderInputError::TooLarge)?;
    bytes.extend_from_slice(&text_len.to_be_bytes());
    bytes.extend_from_slice(text.as_bytes());
    let image_count = u32::try_from(images.len()).map_err(|_| ProviderInputError::TooLarge)?;
    bytes.extend_from_slice(&image_count.to_be_bytes());
    for image in images {
        let path = image.path().as_bytes();
        let path_len = u32::try_from(path.len()).map_err(|_| ProviderInputError::TooLarge)?;
        bytes.extend_from_slice(&path_len.to_be_bytes());
        bytes.extend_from_slice(path);
        bytes.extend_from_slice(image.sha256());
        bytes.extend_from_slice(&image.byte_len().to_be_bytes());
    }
    Ok(bytes)
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
    let mut ids = vec![ACTION_PROVIDER_SEND_NOW, ACTION_PROVIDER_TERMINAL_INPUT];
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
            images: Vec::new(),
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
            vec![b"\x1b[200~hello\x1b[201~".as_slice(), b"\r".as_slice()]
        );
    }

    #[test]
    fn codex_plain_mode_preserves_reply_prefix_and_multiline() {
        let action = ProviderInputAction::SendNow {
            text: "Reply exactly: hello\nsecond line".into(),
            wait: false,
            images: Vec::new(),
        };
        let plain =
            provider_composer_submit_plan_for_mode(ProviderKind::Codex, &action, false).unwrap();
        assert_eq!(plain.steps().len(), 2);
        assert_eq!(
            plain.steps()[0].bytes(),
            b"Reply exactly: hello\nsecond line"
        );
        assert_eq!(plain.steps()[1].bytes(), b"\r");
        assert!(plain.steps()[0].delay_after().unwrap() > std::time::Duration::from_millis(120));
        assert_eq!(
            provider_input_action_bytes(&action).expect("logical"),
            b"Reply exactly: hello\nsecond line"
        );

        let bracketed =
            provider_composer_submit_plan_for_mode(ProviderKind::Codex, &action, true).unwrap();
        assert_eq!(
            bracketed.steps()[0].bytes(),
            b"\x1b[200~Reply exactly: hello\nsecond line\x1b[201~"
        );
        assert_eq!(bracketed.steps()[1].bytes(), b"\r");
    }

    #[test]
    fn codex_submit_preserves_multiline_prefix_and_rejects_paste_escape() {
        let mut action = ProviderInputAction::SendNow {
            text: "Reply exactly: hello\nsecond line".into(),
            wait: false,
            images: Vec::new(),
        };
        let plan = provider_composer_submit_plan(ProviderKind::Codex, &action).unwrap();
        assert_eq!(plan.steps().len(), 2);
        assert_eq!(
            plan.steps()[0].bytes(),
            b"\x1b[200~Reply exactly: hello\nsecond line\x1b[201~"
        );
        assert_eq!(plan.steps()[1].bytes(), b"\r");
        assert!(plan.steps()[0].delay_after().unwrap() > std::time::Duration::from_millis(120));
        if let ProviderInputAction::SendNow { text, .. } = &mut action {
            *text = "prefix\x1b[201~injected".into();
        }
        assert_eq!(
            provider_composer_submit_plan_for_mode(ProviderKind::Codex, &action, true),
            Err(ProviderInputDeliveryError::BytesMismatch),
        );
        assert_eq!(
            provider_composer_submit_plan_for_mode(ProviderKind::Codex, &action, false),
            Err(ProviderInputDeliveryError::BytesMismatch),
        );
        if let ProviderInputAction::SendNow { text, .. } = &mut action {
            *text = "prefix\u{9b}201~injected".into();
        }
        assert_eq!(
            provider_composer_submit_plan_for_mode(ProviderKind::Codex, &action, true),
            Err(ProviderInputDeliveryError::BytesMismatch),
        );
        assert_eq!(
            provider_composer_submit_plan_for_mode(ProviderKind::Codex, &action, false),
            Err(ProviderInputDeliveryError::BytesMismatch),
        );
    }

    #[test]
    fn composer_submit_plan_pastes_each_image_path_before_text() {
        let absolute_a = if cfg!(windows) {
            r"C:\repo\.devmanager\pasted-images\a.png"
        } else {
            "/repo/.devmanager/pasted-images/a.png"
        };
        let absolute_b = if cfg!(windows) {
            r"C:\repo\.devmanager\pasted-images\b.jpg"
        } else {
            "/repo/.devmanager/pasted-images/b.jpg"
        };
        let images = vec![
            ProviderImageAttachment::try_new(absolute_a, [1; 32], 32).expect("a"),
            ProviderImageAttachment::try_new(absolute_b, [2; 32], 64).expect("b"),
        ];
        let action = ProviderInputAction::SendNow {
            text: "caption".into(),
            wait: false,
            images,
        };
        let bracketed =
            provider_composer_submit_plan_for_mode(ProviderKind::Codex, &action, true)
                .expect("codex images bracketed");
        let steps: Vec<&[u8]> = bracketed
            .steps()
            .iter()
            .map(ProviderComposerWriteStep::bytes)
            .collect();
        let expected_a = format!("\x1b[200~{absolute_a}\x1b[201~");
        let expected_b = format!("\x1b[200~{absolute_b}\x1b[201~");
        assert_eq!(steps[0], expected_a.as_bytes());
        assert_eq!(steps[1], expected_b.as_bytes());
        assert_eq!(steps[2], b"\x1b[200~caption\x1b[201~");
        assert_eq!(steps[3], b"\r");
        assert_eq!(steps.len(), 4);
        for paste in &bracketed.steps()[..3] {
            assert!(paste.delay_after().unwrap() > std::time::Duration::from_millis(120));
        }

        let plain =
            provider_composer_submit_plan_for_mode(ProviderKind::Codex, &action, false)
                .expect("codex images plain");
        let plain_steps: Vec<&[u8]> = plain
            .steps()
            .iter()
            .map(ProviderComposerWriteStep::bytes)
            .collect();
        assert_eq!(plain_steps[0], absolute_a.as_bytes());
        assert_eq!(plain_steps[1], absolute_b.as_bytes());
        assert_eq!(plain_steps[2], b"caption");
        assert_eq!(plain_steps[3], b"\r");

        let with_images = provider_input_action_bytes(&action).expect("image bytes");
        assert_ne!(with_images, b"caption");
        assert!(with_images.len() > "caption".len());
    }

    #[test]
    fn composer_submit_plan_supports_image_only_send() {
        let absolute = if cfg!(windows) {
            r"C:\repo\.devmanager\pasted-images\only.png"
        } else {
            "/repo/.devmanager/pasted-images/only.png"
        };
        let action = ProviderInputAction::SendNow {
            text: String::new(),
            wait: false,
            images: vec![ProviderImageAttachment::try_new(absolute, [9; 32], 16).expect("img")],
        };
        let bracketed = provider_composer_submit_plan_for_mode(
            ProviderKind::ClaudeCode,
            &action,
            true,
        )
        .expect("image-only bracketed");
        let steps: Vec<&[u8]> = bracketed
            .steps()
            .iter()
            .map(ProviderComposerWriteStep::bytes)
            .collect();
        let expected = format!("\x1b[200~{absolute}\x1b[201~");
        assert_eq!(steps, vec![expected.as_bytes(), b"\r".as_slice()]);

        let plain = provider_composer_submit_plan_for_mode(
            ProviderKind::ClaudeCode,
            &action,
            false,
        )
        .expect("image-only plain");
        let plain_steps: Vec<&[u8]> = plain
            .steps()
            .iter()
            .map(ProviderComposerWriteStep::bytes)
            .collect();
        assert_eq!(plain_steps, vec![absolute.as_bytes(), b"\r".as_slice()]);
    }

    #[test]
    fn sealed_writer_path_documents_live_mode_plan_construction() {
        // Production write_sealed_provider_action captures mode_snapshot after
        // fence validation, then calls provider_composer_submit_plan_for_mode.
        // This source contract keeps the mode-less planner test-only.
        let source = include_str!("../services/process_manager.rs");
        let start = source
            .find("pub(crate) fn write_sealed_provider_action(")
            .expect("sealed writer");
        let remaining = &source[start..];
        let end = remaining
            .find("pub(crate) fn write_sealed_provider_bytes(")
            .expect("next sealed writer boundary");
        let body = &remaining[..end];
        assert!(
            body.contains("mode_snapshot().bracketed_paste")
                && body.contains("provider_composer_submit_plan_for_mode"),
            "production writer must encode from live session mode"
        );
        assert!(
            body.contains("classify_codex_identityless_startup_readiness"),
            "production writer must consult identityless Codex startup readiness"
        );
        assert!(
            !body.contains("provider_composer_submit_plan(identity")
                && !body.contains("provider_composer_submit_plan(Provider"),
            "production writer must not call the mode-less compat planner"
        );
        assert!(
            body.contains("write_provider_bytes") && !body.contains("paste_text("),
            "sealed writer must not flush via paste_text"
        );
    }

    #[test]
    fn send_now_without_images_keeps_legacy_action_bytes() {
        let action = ProviderInputAction::SendNow {
            text: "hello".into(),
            wait: false,
            images: Vec::new(),
        };
        assert_eq!(
            provider_input_action_bytes(&action).expect("bytes"),
            b"hello"
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
    fn codex_identityless_startup_readiness_classifies_trust_composer_and_pending() {
        let trust = vec![
            String::new(),
            "Do you trust the contents of this directory?".into(),
            "1. Yes, continue".into(),
            "2. No, quit".into(),
            "Press enter to continue".into(),
        ];
        assert_eq!(
            classify_codex_identityless_startup_readiness(&trust),
            CodexIdentitylessStartupReadiness::ProviderSetupRequired,
            "supplied trust screen must not be chat-ready despite bracketed paste elsewhere"
        );

        let composer = vec![
            String::new(),
            "  › Ask Codex to do anything".into(),
        ];
        assert_eq!(
            classify_codex_identityless_startup_readiness(&composer),
            CodexIdentitylessStartupReadiness::ChatComposerReady,
        );

        let wrapped_composer = vec![
            "  › Ask Codex to do any".into(),
            "  thing".into(),
        ];
        assert_eq!(
            classify_codex_identityless_startup_readiness(&wrapped_composer),
            CodexIdentitylessStartupReadiness::ChatComposerReady,
            "narrow-width placeholder wrap must still attestation-ready"
        );

        let empty_prompt = vec![String::new(), "› ".into()];
        assert_eq!(
            classify_codex_identityless_startup_readiness(&empty_prompt),
            CodexIdentitylessStartupReadiness::StartupPending,
            "bare composer glyph without observed placeholder remains pending"
        );

        let bare_glyph = vec![String::new(), "›".into()];
        assert_eq!(
            classify_codex_identityless_startup_readiness(&bare_glyph),
            CodexIdentitylessStartupReadiness::StartupPending,
        );

        let placeholder_in_prose = vec![
            "User: mention ask codex to do anything in prose".into(),
            "Assistant: noted".into(),
        ];
        assert_eq!(
            classify_codex_identityless_startup_readiness(&placeholder_in_prose),
            CodexIdentitylessStartupReadiness::StartupPending,
            "placeholder substring without anchored composer line must not match"
        );

        assert_eq!(
            classify_codex_identityless_startup_readiness(&[]),
            CodexIdentitylessStartupReadiness::StartupPending,
        );
        assert_eq!(
            classify_codex_identityless_startup_readiness(&[String::new(), "  ".into()]),
            CodexIdentitylessStartupReadiness::StartupPending,
        );

        let login_menu = vec![
            String::new(),
            "› 1. Sign in with ChatGPT".into(),
            "› 2. Continue without signing in".into(),
        ];
        assert_eq!(
            classify_codex_identityless_startup_readiness(&login_menu),
            CodexIdentitylessStartupReadiness::StartupPending,
        );

        let busy = vec![
            String::new(),
            "Working...".into(),
            "esc to interrupt".into(),
        ];
        assert_eq!(
            classify_codex_identityless_startup_readiness(&busy),
            CodexIdentitylessStartupReadiness::StartupPending,
        );

        let quoted_chevron = vec![
            "User: use the › character in prose".into(),
            "Assistant: noted".into(),
        ];
        assert_eq!(
            classify_codex_identityless_startup_readiness(&quoted_chevron),
            CodexIdentitylessStartupReadiness::StartupPending,
        );

        let legacy_ask_anything = vec![
            String::new(),
            "  › Ask anything".into(),
            "  esc to interrupt".into(),
        ];
        assert_eq!(
            classify_codex_identityless_startup_readiness(&legacy_ask_anything),
            CodexIdentitylessStartupReadiness::StartupPending,
        );

        let conversation = vec![
            "User: Can you explain trust models in machine learning?".into(),
            "Assistant: trust is a calibration concept...".into(),
            "› Ask Codex to do anything".into(),
        ];
        assert_eq!(
            classify_codex_identityless_startup_readiness(&conversation),
            CodexIdentitylessStartupReadiness::ChatComposerReady,
            "conversation mentioning trust must not blanket-gate when composer placeholder is visible"
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
            images: Vec::new(),
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
