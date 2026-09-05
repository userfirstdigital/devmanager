//! Capability-aware Task composer contract.
//!
//! The GPUI composer is a projection editor. It keeps only ephemeral in-memory
//! edits, reads availability from the shared ActionCatalog plus the current
//! host projection, and never owns filesystem or draft persistence.
//!
//! Production `bind` enables Send/Steer/Queue/Answer/Approval/Stop only when
//! `catalog()` registers those reserved ids **and** `Command` carries the
//! matching host variants. Do not add fixture actions to `ACTIONS`. The
//! compile gate is [`composer_host_command_union_gate`]; runtime RED lives in
//! `tests/ui_composer_production_union.rs`. Save-draft and upload stay typed
//! HOLDs until their host commands exist.

use super::super::shell::PromptLibraryUiError;
use crate::client::action::{
    catalog, ActionDescriptor, ActionRequest, ProviderInputActionRequest, ProviderInputArguments,
};
use crate::domain::id::{PromptChainLinkId, PromptVersionId};
use crate::domain::{
    AgentSessionId, ApprovalId, ArtifactId, CommandId, QuestionId, RequestId, TaskId,
    TurnId as DomainTurnId,
};
use crate::prompts::model::PromptVersion;
use crate::ui::components::interaction::{
    redacted_bounded_text, AccessibilityMetadata, AccessibleRole, ComponentError, FocusEpoch,
    InteractionStateModel, MAX_ACCESSIBLE_DESCRIPTION_SCALARS, MAX_ACCESSIBLE_NAME_SCALARS,
};
use crate::ui::components::text_field::{TextField, TextFieldError, TextFieldKey, TextFieldLimits};
use crate::ui::tokens::ThemeTokens;
use gpui::{div, px, AnyElement, InteractiveElement, IntoElement, ParentElement, Styled};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fmt::{Debug, Display, Formatter};
use std::ops::Range;

pub use crate::client::action::{
    ACTION_TASK_ANSWER_QUESTION as EXPECTED_ACTION_ANSWER,
    ACTION_TASK_QUEUE_FOLLOW_UP as EXPECTED_ACTION_QUEUE,
    ACTION_TASK_REMOVE_COMPOSER_ATTACHMENT as EXPECTED_ACTION_REMOVE_ATTACHMENT,
    ACTION_TASK_RESOLVE_APPROVAL as EXPECTED_ACTION_APPROVAL,
    ACTION_TASK_SAVE_COMPOSER_DRAFT as EXPECTED_ACTION_SAVE_DRAFT,
    ACTION_TASK_SEND_NOW as EXPECTED_ACTION_SEND_NOW,
    ACTION_TASK_STAGE_COMPOSER_ATTACHMENT as EXPECTED_ACTION_STAGE_ATTACHMENT,
    ACTION_TASK_STEER_CURRENT_TURN as EXPECTED_ACTION_STEER,
    ACTION_TASK_STOP_TURN as EXPECTED_ACTION_STOP_TURN,
};

// ---------------------------------------------------------------------------
// The composer's visual language (redesign rules 1-12).
//
// The composer's *painter* lives in `native_shell.rs`, because the draft is
// painted with a `canvas` that needs the shell's shaped-text and caret state.
// Its visual decisions live here, next to the model that owns the composer, so
// there is exactly one place that says what the composer looks like -- and the
// pinning test that used to restate these numbers in `native_shell.rs` now
// reads them from here.
//
// The density tokens cannot supply this scale: they top out at 11/12 px
// captions and 13/14 px body and carry no half-pixel step, while the redesign
// asks for 10.5 / 11 / 11.5.
// ---------------------------------------------------------------------------

/// Rule 3: the composer is an input, so it is `surfaces.sunken` behind a 1 px
/// `borders.default` rule -- `borders.focus` while it holds focus (rule 11) --
/// at the radius rule 3 gives a control. It replaces a 22 px pill whose
/// "border" was a one-pixel background ring under a drop shadow.
pub const COMPOSER_RADIUS: f32 = 6.0;
/// Rule 3: one pixel, like every other rule in the app.
pub const COMPOSER_BORDER_WIDTH: f32 = 1.0;
/// The mockup's `.compose { padding: 6px 10px }`.
pub const COMPOSER_PADDING_X: f32 = 10.0;
pub const COMPOSER_PADDING_Y: f32 = 6.0;
/// Rule 2: the draft is body text, and the placeholder is `text.muted`.
pub const COMPOSER_FONT_SIZE: f32 = 11.5;
/// 11.5 px at the mockup stream's 1.5 leading.
pub const COMPOSER_LINE_HEIGHT: f32 = 17.25;
/// Rule 2: the composer's captions -- the context strip, the hold line, the
/// key hints beside the send control.
pub const COMPOSER_CAPTION_FONT_SIZE: f32 = 10.5;
/// Rule 2: the composer's secondary rows -- the provider and model pills.
pub const COMPOSER_SECONDARY_FONT_SIZE: f32 = 11.0;
/// Rule 4: an icon button is a 24 px hit box around a 14 px lucide glyph, with
/// no border and no fill. The composer's send and stop are the only two.
pub const COMPOSER_ICON_BUTTON_SIZE: f32 = 24.0;
pub const COMPOSER_ICON_GLYPH_SIZE: f32 = 14.0;
/// Rule 7: a kbd chip -- 10.5 px, `borders.default`, radius 4, padding 1x6.
/// The attachment chips wear it, so an attachment reads as a token beside the
/// draft rather than as a card under it.
pub const COMPOSER_CHIP_FONT_SIZE: f32 = 10.5;
pub const COMPOSER_CHIP_RADIUS: f32 = 4.0;
pub const COMPOSER_CHIP_PADDING_X: f32 = 6.0;
pub const COMPOSER_CHIP_PADDING_Y: f32 = 1.0;
/// Rule 6: chip gap 6, control gap 8.
pub const COMPOSER_CHIP_GAP: f32 = 6.0;
pub const COMPOSER_CONTROL_GAP: f32 = 8.0;
/// Rule 5: a full-width row is 5 px above and below its line, with no side
/// margin. The composer's slash-command overlay rows are these.
pub const COMPOSER_ROW_PADDING_Y: f32 = 5.0;
/// Rule 6: region padding 10-12. The composer's footer takes 10, matching the
/// stream column above it so the two share one left edge.
pub const COMPOSER_REGION_PADDING: f32 = 10.0;
/// Rule 4: a default button -- 1 px `borders.default`, no fill, 11 px label,
/// padding 2x8, radius 6. The question card's answer options are these.
pub const COMPOSER_BUTTON_FONT_SIZE: f32 = 11.0;
pub const COMPOSER_BUTTON_PADDING_X: f32 = 8.0;
pub const COMPOSER_BUTTON_PADDING_Y: f32 = 2.0;
pub const COMPOSER_BUTTON_RADIUS: f32 = 6.0;
/// The attachment thumbnail. Rule 3's chip radius, small enough that a chip
/// stays one line tall beside the 24 px icon buttons.
pub const COMPOSER_ATTACHMENT_THUMBNAIL: f32 = 20.0;
/// The tallest a chip's label may get before it truncates.
pub const COMPOSER_CHIP_LABEL_MAX_WIDTH: f32 = 160.0;

/// The key hints that sit at the field's right, inside the sunken rule --
/// the mockup's `.compose .k`. Real key names rather than glyphs: the shell
/// has no key-cap font, and a bare arrow glyph is unreadable at 10.5 px.
pub const COMPOSER_KEY_HINTS: &str = "Enter send \u{b7} Shift+Enter newline";
/// The separator between the meta line's segments, and the one the panel and
/// the board already spend on the same job.
pub const COMPOSER_META_SEPARATOR: &str = "\u{b7}";
/// What the empty field invites, given the provider that would answer it
/// (the mockup's "Message Claude...").
///
/// A function rather than a constant because it names the provider, and a
/// placeholder that named the wrong one would be a claim about where the
/// draft is going.
pub fn composer_placeholder(provider_label: &str) -> String {
    format!("Message {provider_label}\u{2026}")
}

/// How many lines of draft the field shows before it scrolls inside itself.
///
/// The panel body is a column of [stream, composer]: the stream is what the
/// panel is FOR, so the composer is `flex_none` behind a real ceiling rather
/// than a box that grows with the draft. At one-of-eight height a panel body is
/// roughly 330 px, and an unbounded field took 250 of them -- which is the
/// clipped composer in the user's `5.png` and the 60 px stream in `4.png`.
pub const COMPOSER_MAX_VISIBLE_LINES: f32 = 6.0;
/// The field at one line of draft, and the field at [`COMPOSER_MAX_VISIBLE_LINES`].
/// Both include the field's own vertical padding, so they are the element's
/// `min_h`/`max_h` directly rather than numbers a painter has to add to.
pub const COMPOSER_INPUT_MIN_HEIGHT: f32 = COMPOSER_LINE_HEIGHT + 2.0 * COMPOSER_PADDING_Y;
pub const COMPOSER_INPUT_MAX_HEIGHT: f32 =
    COMPOSER_MAX_VISIBLE_LINES * COMPOSER_LINE_HEIGHT + 2.0 * COMPOSER_PADDING_Y;
/// The meta line under the field: one 10.5 px muted row whose right end is the
/// attach affordance, so it is rule 4's 24 px icon hit box tall.
pub const COMPOSER_META_ROW_HEIGHT: f32 = COMPOSER_ICON_BUTTON_SIZE;
/// How much room the stream above the composer yields to it.
///
/// Derived from the parts rather than pinned, so the reserve cannot drift from
/// the composer it is reserving for: the region's top gap, the card's two
/// border pixels and its tallest field, the gap to the meta line, that line,
/// and the region's bottom padding.
pub const COMPOSER_HEIGHT_RESERVE: f32 = COMPOSER_CONTROL_GAP
    + 2.0 * COMPOSER_BORDER_WIDTH
    + COMPOSER_INPUT_MAX_HEIGHT
    + COMPOSER_CHIP_GAP
    + COMPOSER_META_ROW_HEIGHT
    + COMPOSER_REGION_PADDING;

/// What the one control in the composer's send slot is saying right now.
/// Rule 4 gives an icon button one resting tint and one hover tint, and rule 1
/// allows exactly one colour here: red, for the destructive act of stopping a
/// running turn.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ComposerSendLook {
    /// There is something to send and the composer may send it.
    Ready,
    /// Nothing to send yet, or sending is unavailable, or a submission is in
    /// flight that cannot be stopped. Not a target.
    Idle,
    /// A turn is running and the composer may stop it: the slot is Stop.
    Busy,
}

/// One decision about what the send slot shows, taken from the three facts the
/// painter already has, in this precedence:
///
/// 1. `streaming` -- a turn is running *and* [`ComposerControl::StopTurn`] is
///    available for it. The slot becomes Stop, which is a target.
/// 2. `pending` -- a submission is in flight with nothing to stop. The slot
///    stays the send glyph but is not a target, so it cannot double-send.
/// 3. `enabled` -- there is something to send.
///
/// A function rather than an inline chain so the look, the painter and the
/// test read one rule. `streaming` outranks `pending` because a stoppable turn
/// is always also pending, and Stop is the useful half of that pair.
pub fn composer_send_look(enabled: bool, pending: bool, streaming: bool) -> ComposerSendLook {
    if streaming {
        ComposerSendLook::Busy
    } else if pending || !enabled {
        ComposerSendLook::Idle
    } else {
        ComposerSendLook::Ready
    }
}

/// The resting and hover tints for the slot in that state, as token colours.
///
/// `Busy` is Stop, and rule 4's destructive control is red text with no fill:
/// `status.destructive` at rest, `text.primary` on hover (the painter adds the
/// `surfaces.hover` ground under it). `Idle` does not brighten, because it is
/// not a target.
pub fn composer_send_tints(
    look: ComposerSendLook,
    tokens: ThemeTokens,
) -> (crate::ui::tokens::Color, crate::ui::tokens::Color) {
    match look {
        ComposerSendLook::Ready => (tokens.text.muted, tokens.text.primary),
        ComposerSendLook::Busy => (tokens.status.destructive, tokens.text.primary),
        ComposerSendLook::Idle => (tokens.text.disabled, tokens.text.disabled),
    }
}

/// Rule 10 asks for a lucide mark. These are characters instead, because
/// `crate::icons` carries no send or stop glyph and `app_icon` builds an
/// `svg()` whose colour is fixed at construction -- so it cannot brighten on
/// hover, which is the half of rule 4 that is behaviour rather than
/// decoration. Ledgered as deviation 1 in `lane-r1-report.md`.
pub const COMPOSER_SEND_GLYPH: &str = "↑";
pub const COMPOSER_STOP_GLYPH: &str = "■";

/// The glyph the slot wears in that state. One mapping, so the painted glyph
/// and the tint beside it cannot disagree about which control this is.
pub fn composer_send_glyph(look: ComposerSendLook) -> &'static str {
    match look {
        ComposerSendLook::Busy => COMPOSER_STOP_GLYPH,
        ComposerSendLook::Ready | ComposerSendLook::Idle => COMPOSER_SEND_GLYPH,
    }
}

/// The element and accessibility id the slot publishes in that state. Stop is
/// its own node so a screen reader is never told "Send" while the button under
/// the pointer stops the turn.
pub const COMPOSER_SEND_ELEMENT_ID: &str = "native-task-composer-send";
pub const COMPOSER_STOP_ELEMENT_ID: &str = "native-composer-stop";

/// Which id the slot carries in that state -- read by the painter and by the
/// accessibility tree, so the node and the button cannot drift apart.
pub fn composer_send_element_id(look: ComposerSendLook) -> &'static str {
    match look {
        ComposerSendLook::Busy => COMPOSER_STOP_ELEMENT_ID,
        ComposerSendLook::Ready | ComposerSendLook::Idle => COMPOSER_SEND_ELEMENT_ID,
    }
}

pub const MAX_QUESTION_OPTIONS: usize = 16;
pub const MAX_DISABLED_REASONS: usize = 9;
pub const MAX_COMPOSER_ATTACHMENTS: usize = 8;
pub const MAX_OWNED_ARTIFACTS: usize = 32;
pub const MAX_SEARCH_QUERY_SCALARS: usize = 64;
pub const MAX_SEARCH_RESULTS: usize = 32;
pub const MAX_PROMPT_ID_SCALARS: usize = 128;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub enum ComposerTurnMode {
    SendNow,
    Steer,
    QueueFollowUp,
}

/// Client-local insertion mode for an exact immutable prompt version.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ComposerInsertionMode {
    ReplaceDraft,
    InsertAtCursor,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PutPromptVersionInComposer {
    pub task_id: TaskId,
    pub agent_session_id: AgentSessionId,
    pub prompt_version_id: PromptVersionId,
    pub insertion: ComposerInsertionMode,
    pub chain_link_id: Option<PromptChainLinkId>,
    pub sends_provider_input: bool,
    pub advances_chain: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExactPromptPayload {
    pub version_id: PromptVersionId,
    pub body: String,
    pub body_sha256: [u8; 32],
}

impl ExactPromptPayload {
    pub fn from_version(version: &PromptVersion) -> Self {
        Self {
            version_id: version.id,
            body: version.body.clone(),
            body_sha256: version.body_sha256,
        }
    }

    pub fn matches(&self, version_id: PromptVersionId) -> bool {
        if self.version_id != version_id {
            return false;
        }
        let digest: [u8; 32] = Sha256::digest(self.body.as_bytes()).into();
        digest == self.body_sha256
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DraftProvenance {
    pub prompt_version_id: PromptVersionId,
    pub chain_link_id: Option<PromptChainLinkId>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComposerDraft {
    pub task_id: Option<TaskId>,
    pub agent_session_id: Option<AgentSessionId>,
    pub text: String,
    pub cursor: usize,
    pub provenance: Option<DraftProvenance>,
    pub sent: bool,
}

impl Default for ComposerDraft {
    fn default() -> Self {
        Self {
            task_id: None,
            agent_session_id: None,
            text: String::new(),
            cursor: 0,
            provenance: None,
            sent: false,
        }
    }
}

impl ComposerDraft {
    pub fn edit(&mut self, text: String, cursor: usize) {
        self.text = text;
        self.cursor = cursor.min(self.text.len());
        self.provenance = None;
        self.sent = false;
    }

    pub fn mark_sent(&mut self) {
        self.sent = true;
        self.provenance = None;
    }
}

/// Rust-owned provider slash-command catalog. TypeScript is a generated mirror.
pub use crate::ui::provider_catalog::{
    provider_command_catalog, provider_command_opens_terminal, suggest_provider_commands,
    ProviderCommandSuggestion,
};

pub fn apply_put_prompt_version(
    draft: &mut ComposerDraft,
    action: &PutPromptVersionInComposer,
    payload: &ExactPromptPayload,
) -> Result<(), PromptLibraryUiError> {
    if action.sends_provider_input || action.advances_chain {
        return Err(PromptLibraryUiError::PayloadMismatch);
    }
    if !payload.matches(action.prompt_version_id) {
        return Err(PromptLibraryUiError::PayloadMismatch);
    }
    match action.insertion {
        ComposerInsertionMode::ReplaceDraft => {
            draft.text = payload.body.clone();
            draft.cursor = draft.text.len();
        }
        ComposerInsertionMode::InsertAtCursor => {
            let cursor = draft.cursor.min(draft.text.len());
            draft.text.insert_str(cursor, &payload.body);
            draft.cursor = cursor.saturating_add(payload.body.len());
        }
    }
    draft.task_id = Some(action.task_id);
    draft.agent_session_id = Some(action.agent_session_id);
    draft.provenance = Some(DraftProvenance {
        prompt_version_id: action.prompt_version_id,
        chain_link_id: action.chain_link_id,
    });
    draft.sent = false;
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub enum ComposerControl {
    SendNow,
    Steer,
    QueueFollowUp,
    Answer,
    Approval,
    StopTurn,
    SaveDraft,
    StageAttachment,
    RemoveAttachment,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EnterPreference {
    EnterSends,
    EnterInsertsNewline,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum AttachmentKind {
    File,
    Image,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct TurnId(u64);

impl TurnId {
    pub const fn from_raw(raw: u64) -> Self {
        Self(raw)
    }

    pub const fn raw(self) -> u64 {
        self.0
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PromptVersionRef {
    pub prompt_id: String,
    pub version: u64,
    pub body: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ComposerFence {
    pub task_id: TaskId,
    pub agent_session_id: AgentSessionId,
    pub runtime_generation: u64,
    pub action_epoch: u64,
    pub turn_id: Option<TurnId>,
}

impl ComposerFence {
    pub fn with_runtime_generation(mut self, runtime_generation: u64) -> Self {
        self.runtime_generation = runtime_generation;
        self
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AnswerPayload {
    Text(String),
    Option { index: u16, label: String },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ApprovalDecision {
    Approve,
    Reject { reason: Option<String> },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ComposerPayload {
    SendNow {
        text: String,
        artifact_ids: Vec<ArtifactId>,
        prompt: Option<(String, u64)>,
    },
    Steer {
        text: String,
        artifact_ids: Vec<ArtifactId>,
        turn_id: TurnId,
    },
    QueueFollowUp {
        text: String,
        artifact_ids: Vec<ArtifactId>,
    },
    Answer {
        request_id: RequestId,
        state_revision: u64,
        answer: AnswerPayload,
    },
    Approval {
        request_id: RequestId,
        state_revision: u64,
        decision: ApprovalDecision,
    },
    StopTurn {
        turn_id: TurnId,
    },
    SaveDraft {
        text: String,
        artifact_ids: Vec<ArtifactId>,
    },
    StageAttachment {
        artifact_id: ArtifactId,
    },
    RemoveAttachment {
        artifact_id: ArtifactId,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ComposerIntent {
    pub command_id: CommandId,
    pub action_id: &'static str,
    pub fence: ComposerFence,
    pub payload: ComposerPayload,
}

impl ComposerIntent {
    pub fn writes_pty(&self) -> bool {
        false
    }

    /// Convert a pending composer intent into the existing typed host action.
    /// Exact resume / new-conversation is never fabricated here.
    pub fn to_provider_input_request(
        &self,
        turn_id: Option<DomainTurnId>,
        question_id: Option<QuestionId>,
        approval_id: Option<ApprovalId>,
    ) -> Result<ActionRequest, ComposerError> {
        self.to_provider_input_request_with_images(turn_id, question_id, approval_id, Vec::new())
    }

    pub fn to_provider_input_request_with_images(
        &self,
        turn_id: Option<DomainTurnId>,
        question_id: Option<QuestionId>,
        approval_id: Option<ApprovalId>,
        images: Vec<crate::domain::ProviderImageAttachment>,
    ) -> Result<ActionRequest, ComposerError> {
        if !images.is_empty() && !matches!(self.payload, ComposerPayload::SendNow { .. }) {
            return Err(ComposerError::Unavailable {
                control: ComposerControl::SendNow,
                reason: "Images require Send; wait for this turn to finish before sending them."
                    .into(),
            });
        }
        if self.action_id == crate::client::action::ACTION_PROVIDER_NEW_CONVERSATION {
            return Err(ComposerError::Unavailable {
                control: ComposerControl::SendNow,
                reason: bound_reason(
                    "exact resume failure stays visible; new conversation is not fabricated",
                )?,
            });
        }
        let turn_id = match turn_id {
            Some(turn_id) => turn_id,
            None if matches!(&self.payload, ComposerPayload::SendNow { .. }) => {
                // SendNow is the one provider action the kernel admits when
                // there is no current turn; its accepted event establishes
                // this identity as the current turn. Derive it from the
                // already-stable intent command so transport retries cannot
                // fork one click into multiple provider turns.
                DomainTurnId::from_bytes(*self.command_id.as_bytes())
                    .map_err(|_| ComposerError::UnknownTurn)?
            }
            None => return Err(ComposerError::UnknownTurn),
        };
        let text = match &self.payload {
            ComposerPayload::SendNow { text, .. }
            | ComposerPayload::Steer { text, .. }
            | ComposerPayload::QueueFollowUp { text, .. }
            | ComposerPayload::Answer {
                answer: AnswerPayload::Text(text),
                ..
            } => Some(text.clone()),
            ComposerPayload::StopTurn { .. } | ComposerPayload::Approval { .. } => None,
            ComposerPayload::SaveDraft { .. }
            | ComposerPayload::StageAttachment { .. }
            | ComposerPayload::RemoveAttachment { .. } => {
                return Err(ComposerError::Unavailable {
                    control: ComposerControl::SaveDraft,
                    reason: bound_reason("draft and upload remain typed HOLDs")?,
                });
            }
            ComposerPayload::Answer {
                answer: AnswerPayload::Option { label, .. },
                ..
            } => Some(label.clone()),
        };
        let allow = match &self.payload {
            ComposerPayload::Approval {
                decision: ApprovalDecision::Approve,
                ..
            } => Some(true),
            ComposerPayload::Approval {
                decision: ApprovalDecision::Reject { .. },
                ..
            } => Some(false),
            _ => None,
        };
        Ok(ActionRequest::ProviderInput(ProviderInputActionRequest {
            command_id: self.command_id,
            action_id: self.action_id,
            arguments: ProviderInputArguments {
                task_id: self.fence.task_id,
                agent_session_id: self.fence.agent_session_id,
                runtime_generation: self.fence.runtime_generation,
                action_epoch: self.fence.action_epoch,
                turn_id,
                question_id,
                approval_id,
                text,
                images,
                wait: Some(false),
                allow,
            },
        }))
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ComposerAttachmentProjection {
    pub artifact_id: ArtifactId,
    pub kind: AttachmentKind,
    pub label: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ComposerDraftProjection {
    pub text: String,
    pub attachments: Vec<ComposerAttachmentProjection>,
    pub prompt: Option<PromptVersionRef>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct QuestionProjection {
    pub request_id: RequestId,
    pub state_revision: u64,
    pub options: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ApprovalProjection {
    pub request_id: RequestId,
    pub state_revision: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ComposerHostProjection {
    pub fence: ComposerFence,
    pub draft: ComposerDraftProjection,
    pub owned_artifacts: Vec<ArtifactId>,
    pub question: Option<QuestionProjection>,
    pub approval: Option<ApprovalProjection>,
    pub disabled_reasons: Vec<(ComposerControl, String)>,
}

/// Build the composer projection for one selected task/agent pair.
///
/// The caller supplies identities from the canonical task snapshot. This
/// helper deliberately does not derive a turn id from a PTY, timestamp, or
/// transcript position; the exact provider turn is supplied separately at
/// the typed host-action boundary.
pub fn projection_for_task(
    fence: ComposerFence,
    owned_artifacts: Vec<ArtifactId>,
    question: Option<(RequestId, u64)>,
    approval: Option<(RequestId, u64)>,
) -> ComposerHostProjection {
    ComposerHostProjection {
        fence,
        draft: ComposerDraftProjection {
            text: String::new(),
            attachments: Vec::new(),
            prompt: None,
        },
        owned_artifacts,
        question: question.map(|(request_id, state_revision)| QuestionProjection {
            request_id,
            state_revision,
            options: Vec::new(),
        }),
        approval: approval.map(|(request_id, state_revision)| ApprovalProjection {
            request_id,
            state_revision,
        }),
        disabled_reasons: Vec::new(),
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ControlAvailability {
    available: bool,
    reason: Option<String>,
}

impl ControlAvailability {
    pub fn available() -> Self {
        Self {
            available: true,
            reason: None,
        }
    }

    pub fn unavailable(reason: impl Into<String>) -> Self {
        Self {
            available: false,
            reason: Some(reason.into()),
        }
    }

    pub fn is_available(&self) -> bool {
        self.available
    }

    pub fn reason(&self) -> Option<&str> {
        self.reason.as_deref()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ActionSearchHit {
    pub id: &'static str,
    pub title: &'static str,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ComposerError {
    TextBoundExceeded {
        max: usize,
        actual: usize,
    },
    Unavailable {
        control: ComposerControl,
        reason: String,
    },
    StaleFence {
        current: ComposerFence,
        attempted: ComposerFence,
    },
    StaleFocusEpoch {
        attempted: FocusEpoch,
    },
    StalePointer {
        captured: Option<FocusEpoch>,
        attempted: FocusEpoch,
    },
    PendingConflict {
        command_id: CommandId,
    },
    UnknownTurn,
    StaleRequest {
        request_id: RequestId,
        expected_revision: u64,
        attempted_revision: u64,
    },
    AttachmentRejected {
        reason: String,
    },
    Component(ComponentError),
    TextField(TextFieldError),
}

impl From<ComponentError> for ComposerError {
    fn from(error: ComponentError) -> Self {
        match error {
            ComponentError::TooManyScalars { max, actual, .. }
            | ComponentError::TooManyBytes { max, actual, .. } => {
                Self::TextBoundExceeded { max, actual }
            }
            other => Self::Component(other),
        }
    }
}

impl From<TextFieldError> for ComposerError {
    fn from(error: TextFieldError) -> Self {
        match error {
            TextFieldError::ScalarLimitExceeded { max, actual }
            | TextFieldError::ByteLimitExceeded { max, actual } => {
                Self::TextBoundExceeded { max, actual }
            }
            other => Self::TextField(other),
        }
    }
}

impl Display for ComposerError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TextBoundExceeded { max, actual } => {
                write!(formatter, "composer text exceeds {max} scalars ({actual})")
            }
            Self::Unavailable { control, reason } => {
                write!(formatter, "{control:?} unavailable: {reason}")
            }
            Self::StaleFence { .. } => write!(formatter, "composer fence is stale"),
            Self::StaleFocusEpoch { .. } => write!(formatter, "composer focus epoch is stale"),
            Self::StalePointer { .. } => write!(formatter, "composer pointer release is stale"),
            Self::PendingConflict { .. } => {
                write!(formatter, "pending composer command payload conflict")
            }
            Self::UnknownTurn => write!(formatter, "composer has no current turn"),
            Self::StaleRequest { .. } => write!(formatter, "composer request revision is stale"),
            Self::AttachmentRejected { reason } => {
                write!(formatter, "attachment rejected: {reason}")
            }
            Self::Component(error) => Display::fmt(error, formatter),
            Self::TextField(error) => Display::fmt(error, formatter),
        }
    }
}

impl std::error::Error for ComposerError {}

pub struct TaskComposer {
    fence: ComposerFence,
    catalog: &'static [ActionDescriptor],
    field: TextField,
    focus_epoch: Option<FocusEpoch>,
    turn_mode: ComposerTurnMode,
    enter_preference: EnterPreference,
    ime_composing: bool,
    dirty: bool,
    attachments: Vec<ComposerAttachmentProjection>,
    owned_artifacts: Vec<ArtifactId>,
    question: Option<QuestionProjection>,
    approval: Option<ApprovalProjection>,
    disabled_reasons: BTreeMap<ComposerControl, String>,
    pending: Option<ComposerIntent>,
    inserted_prompt: Option<(String, u64)>,
    auto_sent: bool,
    captured: Option<CapturedActivation>,
    controls: BTreeMap<ComposerControl, InteractionStateModel>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct CapturedActivation {
    control: ComposerControl,
    pointer_id: Option<u64>,
    focus_epoch: FocusEpoch,
    fence: ComposerFence,
    question: Option<(RequestId, u64)>,
    approval: Option<(RequestId, u64)>,
    turn_id: Option<TurnId>,
    available: bool,
}

impl Debug for TaskComposer {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("TaskComposer")
            .field("fence", &self.fence)
            .field("dirty", &self.dirty)
            .field(
                "pending_command_id",
                &self.pending.as_ref().map(|intent| intent.command_id),
            )
            .field("attachments", &self.attachments)
            .field("inserted_prompt", &self.inserted_prompt)
            .finish()
    }
}

/// Compile gate for the production catalog/command union.
///
/// Pins the **production** `domain::command::Command` enum (not a test double)
/// and `client::action::catalog()` (not `FIXTURE_CATALOG` / `bind_with_catalog`).
/// Exhaustive match: a parallel or `cfg(test)`-only variant cannot satisfy this
/// doctest, because rustdoc builds the library without `cfg(test)`.
/// Save-draft and upload must stay unmatched until their host commands exist.
///
/// Runtime RED: `tests/ui_composer_production_union.rs`.
/// Source fixture: `tests/fixtures/ui/production_command_union.rs`.
/// Later verify with `cargo test --doc composer_host_command_union_gate`.
///
/// ```compile_fail
/// use devmanager::ui::task_cockpit::composer::TaskComposer;
/// let _ = TaskComposer::bind_with_catalog;
/// ```
///
/// ```
/// fn accept_production_host_commands(command: devmanager::domain::command::Command) {
///     match command {
///         devmanager::domain::command::Command::CreateTask(_)
///         | devmanager::domain::command::Command::CreateTaskV2(_)
///         | devmanager::domain::command::Command::RenameTask(_)
///         | devmanager::domain::command::Command::SetTaskAttention(_)
///         | devmanager::domain::command::Command::BeginCloseTask
///         | devmanager::domain::command::Command::ReopenTask
///         | devmanager::domain::command::Command::RegisterAgentSession { .. }
///         | devmanager::domain::command::Command::SetPrimaryAgent { .. }
///         | devmanager::domain::command::Command::RegisterArtifact { .. }
///         | devmanager::domain::command::Command::RegisterResource { .. }
///         | devmanager::domain::command::Command::ReleaseResource { .. }
///         | devmanager::domain::command::Command::ConfirmHostQuit(_)
///         | devmanager::domain::command::Command::SubmitProviderInput(_)
///         | devmanager::domain::command::Command::PresentProviderQuestion(_)
///         | devmanager::domain::command::Command::PresentProviderApproval(_)
///         | devmanager::domain::command::Command::SettleProviderWait(_)
///         | devmanager::domain::command::Command::RequestSpecialist(_)
///         | devmanager::domain::command::Command::PromotePrimary(_)
///         | devmanager::domain::command::Command::CancelSpecialist(_)
///         | devmanager::domain::command::Command::AcceptSpecialistHandoff(_)
///         | devmanager::domain::command::Command::PromptLibrary(_)
///         | devmanager::domain::command::Command::ServiceControl(_)
///         | devmanager::domain::command::Command::Browser(_)
///         | devmanager::domain::command::Command::PrepareUpdate(_)
///         | devmanager::domain::command::Command::ConfirmUpdateDrain(_)
///         | devmanager::domain::command::Command::AbortUpdateHandoff
///         | devmanager::domain::command::Command::ArmUpdateInstall(_)
///         | devmanager::domain::command::Command::SendNow(_)
///         | devmanager::domain::command::Command::SteerCurrentTurn(_)
///         | devmanager::domain::command::Command::QueueFollowUp(_)
///         | devmanager::domain::command::Command::AnswerQuestion(_)
///         | devmanager::domain::command::Command::ResolveApproval(_)
///         | devmanager::domain::command::Command::StopTurn(_) => {}
///     }
/// }
/// fn production_catalog_registers_turn_ids_not_draft_upload() {
///     let catalog = devmanager::client::action::catalog;
///     assert!(std::ptr::eq(catalog(), devmanager::client::action::catalog()));
///     let ids: Vec<_> = catalog().iter().map(|descriptor| descriptor.id).collect();
///     assert!(ids.contains(&devmanager::client::action::ACTION_TASK_SEND_NOW));
///     assert!(ids.contains(&devmanager::client::action::ACTION_TASK_STEER_CURRENT_TURN));
///     assert!(ids.contains(&devmanager::client::action::ACTION_TASK_QUEUE_FOLLOW_UP));
///     assert!(ids.contains(&devmanager::client::action::ACTION_TASK_ANSWER_QUESTION));
///     assert!(ids.contains(&devmanager::client::action::ACTION_TASK_RESOLVE_APPROVAL));
///     assert!(ids.contains(&devmanager::client::action::ACTION_TASK_STOP_TURN));
///     assert!(!ids.contains(&devmanager::client::action::ACTION_TASK_SAVE_COMPOSER_DRAFT));
///     assert!(!ids.contains(&devmanager::client::action::ACTION_TASK_STAGE_COMPOSER_ATTACHMENT));
///     assert!(!ids.contains(&devmanager::client::action::ACTION_TASK_REMOVE_COMPOSER_ATTACHMENT));
///     let _ = accept_production_host_commands;
/// }
/// ```
pub fn composer_host_command_union_gate() {}

impl TaskComposer {
    /// Production constructor. Uses only the shared ActionCatalog.
    ///
    /// Turn controls stay unavailable until the catalog/command union lands
    /// ([`composer_host_command_union_gate`]).
    ///
    /// ```compile_fail
    /// use devmanager::ui::task_cockpit::composer::TaskComposer;
    /// let _ = TaskComposer::bind_with_catalog;
    /// ```
    pub fn bind(projection: ComposerHostProjection) -> Result<Self, ComposerError> {
        Self::bind_inner(projection, catalog())
    }

    /// Bind a task-owned composer and capture the current UI focus epoch
    /// before any pointer or keyboard gesture can submit it.
    pub fn bind_for_task(
        projection: ComposerHostProjection,
        focus_epoch: FocusEpoch,
    ) -> Result<Self, ComposerError> {
        let mut composer = Self::bind(projection)?;
        composer.set_focus_epoch(focus_epoch)?;
        Ok(composer)
    }

    #[cfg(test)]
    fn bind_with_catalog(
        projection: ComposerHostProjection,
        catalog: &'static [ActionDescriptor],
    ) -> Result<Self, ComposerError> {
        Self::bind_inner(projection, catalog)
    }

    fn bind_inner(
        projection: ComposerHostProjection,
        catalog: &'static [ActionDescriptor],
    ) -> Result<Self, ComposerError> {
        let projection = validate_projection(projection)?;
        let mut composer = Self {
            fence: projection.fence,
            catalog,
            field: TextField::new("Prompt")?,
            focus_epoch: None,
            turn_mode: ComposerTurnMode::SendNow,
            enter_preference: EnterPreference::EnterSends,
            ime_composing: false,
            dirty: false,
            attachments: Vec::new(),
            owned_artifacts: Vec::new(),
            question: None,
            approval: None,
            disabled_reasons: BTreeMap::new(),
            pending: None,
            inserted_prompt: None,
            auto_sent: false,
            captured: None,
            controls: BTreeMap::new(),
        };
        composer.ensure_controls();
        composer.install_projection(projection, true)?;
        composer.sync_control_availability();
        Ok(composer)
    }

    pub fn fence(&self) -> ComposerFence {
        self.fence
    }

    pub fn focus_epoch(&self) -> Option<FocusEpoch> {
        self.focus_epoch
    }

    pub fn pending_question_identity(&self) -> Option<(RequestId, u64)> {
        self.question
            .as_ref()
            .map(|question| (question.request_id, question.state_revision))
    }

    pub fn pending_approval_identity(&self) -> Option<(RequestId, u64)> {
        self.approval
            .as_ref()
            .map(|approval| (approval.request_id, approval.state_revision))
    }

    /// Render the task-owned composer status from the same projection used by
    /// typed input actions. This surface is intentionally presentation-only;
    /// submission still goes through `ComposerIntent` and the host action lane.
    pub fn surface(&self, tokens: ThemeTokens) -> AnyElement {
        let question = self
            .pending_question_identity()
            .map(|(_, revision)| format!("question rev {revision}"))
            .unwrap_or_else(|| "no pending question".to_string());
        let approval = self
            .pending_approval_identity()
            .map(|(_, revision)| format!("approval rev {revision}"))
            .unwrap_or_else(|| "no pending approval".to_string());
        let turn = if self.fence.turn_id.is_some() {
            "turn available"
        } else {
            "turn identity unavailable"
        };
        // Rules 2, 3 and 9: this seam paints the composer's *state*, not an
        // input, so it is a quiet 11.5 px `text.muted` block under the same
        // 1 px `borders.subtle` rule every region gets -- no raised surface
        // pretending to be a field.
        div()
            .id("native-task-composer")
            .w_full()
            .flex_col()
            .gap(px(COMPOSER_CHIP_PADDING_Y))
            .px(px(COMPOSER_REGION_PADDING))
            .py(px(COMPOSER_PADDING_Y))
            .border_t(px(COMPOSER_BORDER_WIDTH))
            .border_color(tokens.borders.subtle.to_gpui())
            .text_size(px(COMPOSER_FONT_SIZE))
            .line_height(px(COMPOSER_LINE_HEIGHT))
            .text_color(tokens.text.muted.to_gpui())
            .child("Task composer")
            .child(format!(
                "{} character draft · {} · {} · {}",
                self.draft_text().chars().count(),
                self.primary_submit_label(),
                question,
                approval
            ))
            .child(turn)
            .into_any_element()
    }

    pub fn text_limits(&self) -> TextFieldLimits {
        self.field.limits()
    }

    pub fn search_query_limit(&self) -> usize {
        MAX_SEARCH_QUERY_SCALARS
    }

    pub fn draft_text(&self) -> &str {
        self.field.value()
    }

    pub fn draft_cursor(&self) -> usize {
        self.field.cursor()
    }

    pub fn draft_selection_range(&self) -> Option<std::ops::Range<usize>> {
        self.field.selection_range()
    }

    pub fn draft_is_all_selected(&self) -> bool {
        self.field.is_all_selected()
    }

    pub fn select_all_draft(&mut self, focus_epoch: FocusEpoch) -> Result<(), ComposerError> {
        self.require_epoch(focus_epoch)?;
        self.field.select_all();
        Ok(())
    }

    pub fn set_draft_cursor(
        &mut self,
        cursor: usize,
        extend: bool,
        focus_epoch: FocusEpoch,
    ) -> Result<(), ComposerError> {
        self.require_epoch(focus_epoch)?;
        self.field.set_cursor(cursor, extend);
        Ok(())
    }

    pub fn replace_draft_range(
        &mut self,
        range: Range<usize>,
        text: &str,
        focus_epoch: FocusEpoch,
    ) -> Result<bool, ComposerError> {
        self.require_epoch(focus_epoch)?;
        let changed = self.field.replace_range(range, text, focus_epoch)?;
        if changed {
            self.dirty = true;
        }
        Ok(changed)
    }

    pub fn attachments(&self) -> &[ComposerAttachmentProjection] {
        &self.attachments
    }

    /// Snapshot the local, unsent input so a task switch can park and restore
    /// the exact draft without treating it as host-projected conversation
    /// state. Provider/runtime identity remains owned by the composer fence.
    pub fn draft_projection(&self) -> ComposerDraftProjection {
        ComposerDraftProjection {
            text: self.field.value().to_string(),
            attachments: self.attachments.clone(),
            prompt: self
                .inserted_prompt
                .as_ref()
                .map(|(prompt_id, version)| PromptVersionRef {
                    prompt_id: prompt_id.clone(),
                    version: *version,
                    body: self.field.value().to_string(),
                }),
        }
    }

    pub fn presented_question_options(&self) -> Result<Vec<String>, ComposerError> {
        let Some(question) = &self.question else {
            return Ok(Vec::new());
        };
        question
            .options
            .iter()
            .map(|option| {
                sanitize_display_text("question option", option, MAX_ACCESSIBLE_NAME_SCALARS)
            })
            .collect()
    }

    pub fn primary_submit_label(&self) -> &'static str {
        match self.turn_mode {
            ComposerTurnMode::SendNow => "Send Now",
            ComposerTurnMode::Steer => "Steer",
            ComposerTurnMode::QueueFollowUp => "Queue Follow-up",
        }
    }

    pub fn availability(
        &self,
        control: ComposerControl,
    ) -> Result<ControlAvailability, ComposerError> {
        let action_id = expected_action_id(control);
        if !self.catalog_has(action_id) {
            return Ok(ControlAvailability::unavailable(bound_reason(&format!(
                "action catalog does not expose {action_id}"
            ))?));
        }
        if let Some(reason) = self.disabled_reasons.get(&control) {
            return Ok(ControlAvailability::unavailable(reason.clone()));
        }
        match control {
            ComposerControl::Steer | ComposerControl::StopTurn if self.fence.turn_id.is_none() => {
                Ok(ControlAvailability::unavailable(bound_reason(
                    "no current turn",
                )?))
            }
            ComposerControl::Answer if self.question.is_none() => Ok(
                ControlAvailability::unavailable(bound_reason("no pending question")?),
            ),
            ComposerControl::Approval if self.approval.is_none() => Ok(
                ControlAvailability::unavailable(bound_reason("no pending approval")?),
            ),
            ComposerControl::StageAttachment if self.stageable_artifact().is_none() => Ok(
                ControlAvailability::unavailable(bound_reason("no owned artifact to stage")?),
            ),
            ComposerControl::RemoveAttachment if self.attachments.is_empty() => Ok(
                ControlAvailability::unavailable(bound_reason("no staged attachment to remove")?),
            ),
            _ => Ok(ControlAvailability::available()),
        }
    }

    pub fn set_focus_epoch(&mut self, focus_epoch: FocusEpoch) -> Result<(), ComposerError> {
        if !self.field.set_focus_epoch(focus_epoch) {
            return Err(ComposerError::StaleFocusEpoch {
                attempted: focus_epoch,
            });
        }
        for model in self.controls.values_mut() {
            if !model.set_focus_epoch(focus_epoch) {
                return Err(ComposerError::StaleFocusEpoch {
                    attempted: focus_epoch,
                });
            }
        }
        if self
            .focus_epoch
            .is_some_and(|current| current != focus_epoch)
        {
            self.captured = None;
        }
        self.focus_epoch = Some(focus_epoch);
        Ok(())
    }

    pub fn focus_input(&mut self, focus_epoch: FocusEpoch) -> Result<(), ComposerError> {
        self.require_epoch(focus_epoch)?;
        self.blur_controls();
        if !self.field.focus() {
            return Err(ComposerError::StaleFocusEpoch {
                attempted: focus_epoch,
            });
        }
        self.arm(control_for_mode(self.turn_mode), None, focus_epoch);
        Ok(())
    }

    pub fn focus_control(
        &mut self,
        control: ComposerControl,
        focus_epoch: FocusEpoch,
    ) -> Result<(), ComposerError> {
        self.require_epoch(focus_epoch)?;
        self.field.blur();
        for (id, model) in self.controls.iter_mut() {
            if *id != control {
                model.blur();
            }
        }
        let Some(model) = self.controls.get_mut(&control) else {
            return Err(ComposerError::Unavailable {
                control,
                reason: bound_reason("composer control is missing")?,
            });
        };
        if !model.focus() {
            return Err(ComposerError::StaleFocusEpoch {
                attempted: focus_epoch,
            });
        }
        self.arm(control, None, focus_epoch);
        Ok(())
    }

    pub fn handle_key(
        &mut self,
        key: TextFieldKey,
        focus_epoch: FocusEpoch,
    ) -> Result<bool, ComposerError> {
        self.require_epoch(focus_epoch)?;
        let changed = self.field.handle_key(key, focus_epoch)?;
        if changed {
            self.dirty = true;
        }
        Ok(changed)
    }

    pub fn insert_newline(&mut self, focus_epoch: FocusEpoch) -> Result<(), ComposerError> {
        self.require_epoch(focus_epoch)?;
        if self.field.paste("\n", focus_epoch)? {
            self.dirty = true;
        }
        Ok(())
    }

    pub fn replace_draft(
        &mut self,
        text: &str,
        focus_epoch: FocusEpoch,
    ) -> Result<(), ComposerError> {
        self.require_epoch(focus_epoch)?;
        reject_oversize("composer draft", text, self.field.limits().max_scalars)?;
        self.field.set_value(text)?;
        self.dirty = true;
        Ok(())
    }

    pub fn set_turn_mode(&mut self, mode: ComposerTurnMode) -> Result<(), ComposerError> {
        self.turn_mode = mode;
        Ok(())
    }

    pub fn activate(
        &mut self,
        control: ComposerControl,
        focus_epoch: FocusEpoch,
    ) -> Result<ComposerIntent, ComposerError> {
        self.activate_with_fence(control, self.fence, focus_epoch)
    }

    /// Submit an image-only prompt. The native shell calls this only after it
    /// has staged at least one image; it then prefixes the frozen provider
    /// payload with those exact prompt references before dispatch.
    pub fn activate_attachment_only_send(
        &mut self,
        focus_epoch: FocusEpoch,
    ) -> Result<ComposerIntent, ComposerError> {
        self.require_epoch(focus_epoch)?;
        self.require_available(ComposerControl::SendNow)?;
        if !self.field.value().trim().is_empty() {
            return Err(ComposerError::Unavailable {
                control: ComposerControl::SendNow,
                reason: bound_reason("attachment-only send requires an empty draft")?,
            });
        }
        let artifact_ids = self
            .attachments
            .iter()
            .map(|attachment| attachment.artifact_id)
            .collect();
        self.submit(
            ComposerControl::SendNow,
            self.fence,
            focus_epoch,
            ComposerPayload::SendNow {
                text: String::new(),
                artifact_ids,
                prompt: self.inserted_prompt.clone(),
            },
        )
    }

    pub fn activate_with_fence(
        &mut self,
        control: ComposerControl,
        fence: ComposerFence,
        focus_epoch: FocusEpoch,
    ) -> Result<ComposerIntent, ComposerError> {
        self.require_epoch(focus_epoch)?;
        if fence != self.fence {
            return Err(ComposerError::StaleFence {
                current: self.fence,
                attempted: fence,
            });
        }
        self.require_available(control)?;
        if Self::is_sealed_release(control) {
            return self.release_captured(control, fence, focus_epoch);
        }
        let payload = self.payload_for(control)?;
        self.submit(control, fence, focus_epoch, payload)
    }

    pub fn activate_answer(
        &mut self,
        request_id: RequestId,
        state_revision: u64,
        fence: ComposerFence,
        answer: AnswerPayload,
        focus_epoch: FocusEpoch,
    ) -> Result<ComposerIntent, ComposerError> {
        self.require_epoch(focus_epoch)?;
        if fence != self.fence {
            return Err(ComposerError::StaleFence {
                current: self.fence,
                attempted: fence,
            });
        }
        self.require_available(ComposerControl::Answer)?;
        let question = self.question.clone().ok_or(ComposerError::Unavailable {
            control: ComposerControl::Answer,
            reason: bound_reason("no pending question")?,
        })?;
        if question.request_id != request_id || question.state_revision != state_revision {
            return Err(ComposerError::StaleRequest {
                request_id: question.request_id,
                expected_revision: question.state_revision,
                attempted_revision: state_revision,
            });
        }
        let captured = self.take_armed_submit(ComposerControl::Answer, focus_epoch)?;
        let (captured_request, captured_revision) =
            captured.question.ok_or(ComposerError::StaleRequest {
                request_id,
                expected_revision: state_revision,
                attempted_revision: state_revision,
            })?;
        if captured_request != request_id
            || captured_revision != state_revision
            || captured.fence != fence
        {
            return Err(ComposerError::StaleRequest {
                request_id: captured_request,
                expected_revision: captured_revision,
                attempted_revision: state_revision,
            });
        }
        let request_id = captured_request;
        let state_revision = captured_revision;
        let fence = captured.fence;
        let answer = bound_outbound_answer(answer)?;
        if let AnswerPayload::Option { index, ref label } = answer {
            match question.options.get(usize::from(index)) {
                Some(expected) if expected == label => {}
                _ => {
                    return Err(ComposerError::Unavailable {
                        control: ComposerControl::Answer,
                        reason: bound_reason("answer option is not in the current question")?,
                    });
                }
            }
        }
        self.submit(
            ComposerControl::Answer,
            fence,
            focus_epoch,
            ComposerPayload::Answer {
                request_id,
                state_revision,
                answer,
            },
        )
    }

    pub fn activate_stage(
        &mut self,
        artifact_id: ArtifactId,
        focus_epoch: FocusEpoch,
    ) -> Result<ComposerIntent, ComposerError> {
        self.require_owned_stage(artifact_id)?;
        self.submit(
            ComposerControl::StageAttachment,
            self.fence,
            focus_epoch,
            ComposerPayload::StageAttachment { artifact_id },
        )
    }

    pub fn activate_remove(
        &mut self,
        artifact_id: ArtifactId,
        focus_epoch: FocusEpoch,
    ) -> Result<ComposerIntent, ComposerError> {
        self.require_owned_remove(artifact_id)?;
        self.submit(
            ComposerControl::RemoveAttachment,
            self.fence,
            focus_epoch,
            ComposerPayload::RemoveAttachment { artifact_id },
        )
    }

    pub fn activate_approval(
        &mut self,
        request_id: RequestId,
        state_revision: u64,
        fence: ComposerFence,
        decision: ApprovalDecision,
        focus_epoch: FocusEpoch,
    ) -> Result<ComposerIntent, ComposerError> {
        self.require_epoch(focus_epoch)?;
        if fence != self.fence {
            return Err(ComposerError::StaleFence {
                current: self.fence,
                attempted: fence,
            });
        }
        self.require_available(ComposerControl::Approval)?;
        let approval = self.approval.clone().ok_or(ComposerError::Unavailable {
            control: ComposerControl::Approval,
            reason: bound_reason("no pending approval")?,
        })?;
        if approval.request_id != request_id || approval.state_revision != state_revision {
            return Err(ComposerError::StaleRequest {
                request_id: approval.request_id,
                expected_revision: approval.state_revision,
                attempted_revision: state_revision,
            });
        }
        let captured = self.take_armed_submit(ComposerControl::Approval, focus_epoch)?;
        let (captured_request, captured_revision) =
            captured.approval.ok_or(ComposerError::StaleRequest {
                request_id,
                expected_revision: state_revision,
                attempted_revision: state_revision,
            })?;
        if captured_request != request_id
            || captured_revision != state_revision
            || captured.fence != fence
        {
            return Err(ComposerError::StaleRequest {
                request_id: captured_request,
                expected_revision: captured_revision,
                attempted_revision: state_revision,
            });
        }
        let request_id = captured_request;
        let state_revision = captured_revision;
        let fence = captured.fence;
        let decision = bound_outbound_decision(decision)?;
        self.submit(
            ComposerControl::Approval,
            fence,
            focus_epoch,
            ComposerPayload::Approval {
                request_id,
                state_revision,
                decision,
            },
        )
    }

    pub fn pending_intent(&self) -> Option<&ComposerIntent> {
        self.pending.as_ref()
    }

    pub fn retry_pending(
        &mut self,
        focus_epoch: FocusEpoch,
    ) -> Result<ComposerIntent, ComposerError> {
        self.require_epoch(focus_epoch)?;
        let pending = self.pending.clone().ok_or(ComposerError::Unavailable {
            control: ComposerControl::SendNow,
            reason: bound_reason("no pending composer command to retry")?,
        })?;
        if pending.fence != self.fence {
            return Err(ComposerError::StaleFence {
                current: self.fence,
                attempted: pending.fence,
            });
        }
        Ok(pending)
    }

    pub fn cancel_pending(&mut self, command_id: CommandId) -> Result<(), ComposerError> {
        match self.pending.as_ref() {
            Some(pending) if pending.command_id == command_id => {
                self.pending = None;
                Ok(())
            }
            Some(pending) => Err(ComposerError::PendingConflict {
                command_id: pending.command_id,
            }),
            None => Err(ComposerError::Unavailable {
                control: ComposerControl::SendNow,
                reason: bound_reason("no pending composer command to cancel")?,
            }),
        }
    }

    pub fn settle_pending(&mut self, command_id: CommandId) -> Result<(), ComposerError> {
        let pending = match self.pending.as_ref() {
            Some(pending) if pending.command_id == command_id => pending.clone(),
            Some(pending) => {
                return Err(ComposerError::PendingConflict {
                    command_id: pending.command_id,
                });
            }
            None => {
                return Err(ComposerError::Unavailable {
                    control: ComposerControl::SendNow,
                    reason: bound_reason("no pending composer command to settle")?,
                });
            }
        };
        let clear_consumed_draft = self.pending_consumes_current_draft(&pending);
        self.pending = None;
        if clear_consumed_draft {
            self.field.set_value("")?;
            self.attachments.clear();
            self.inserted_prompt = None;
            self.dirty = false;
            self.auto_sent = false;
        }
        Ok(())
    }

    /// Clear a host-accepted draft after the composer was rebound while the
    /// command was in flight. Exact equality protects any newer user edits.
    pub fn clear_draft_if_matches(
        &mut self,
        accepted: &ComposerDraftProjection,
    ) -> Result<bool, ComposerError> {
        if self.draft_projection() != *accepted {
            return Ok(false);
        }
        self.field.set_value("")?;
        self.attachments.clear();
        self.inserted_prompt = None;
        self.dirty = false;
        self.auto_sent = false;
        Ok(true)
    }

    pub fn action_search(&self) -> Result<Vec<ActionSearchHit>, ComposerError> {
        let Some(query) = self.field.value().strip_prefix('/') else {
            return Ok(Vec::new());
        };
        reject_oversize("composer search query", query, MAX_SEARCH_QUERY_SCALARS)?;
        let query = query.trim().to_ascii_lowercase();
        if query.is_empty() {
            return Ok(Vec::new());
        }
        let mut hits = Vec::new();
        for descriptor in catalog() {
            if hits.len() > MAX_SEARCH_RESULTS {
                break;
            }
            if descriptor.id.to_ascii_lowercase().contains(&query)
                || descriptor.title.to_ascii_lowercase().contains(&query)
                || descriptor
                    .keywords
                    .iter()
                    .any(|keyword| keyword.to_ascii_lowercase().contains(&query))
            {
                if !hits
                    .iter()
                    .any(|hit: &ActionSearchHit| hit.id == descriptor.id)
                {
                    hits.push(ActionSearchHit {
                        id: descriptor.id,
                        title: descriptor.title,
                    });
                }
            }
        }
        if hits.len() > MAX_SEARCH_RESULTS {
            hits.truncate(MAX_SEARCH_RESULTS);
        }
        Ok(hits)
    }

    pub fn paste_text(
        &mut self,
        text: &str,
        focus_epoch: FocusEpoch,
    ) -> Result<bool, ComposerError> {
        self.require_epoch(focus_epoch)?;
        let changed = self.field.paste(text, focus_epoch)?;
        if changed {
            self.dirty = true;
        }
        Ok(changed)
    }

    pub fn apply_projection(
        &mut self,
        projection: ComposerHostProjection,
        focus_epoch: FocusEpoch,
    ) -> Result<(), ComposerError> {
        let projection = validate_projection(projection)?;
        self.require_epoch(focus_epoch)?;
        self.reject_stale_projection(&projection.fence)?;
        if !same_task_agent(self.fence, projection.fence) {
            return Err(ComposerError::StaleFence {
                current: self.fence,
                attempted: projection.fence,
            });
        }
        if self.projection_invalidates_pending(&projection) {
            self.pending = None;
        }
        self.install_projection(projection, !self.dirty)?;
        self.sync_control_availability();
        self.invalidate_capture_if_availability_changed()?;
        Ok(())
    }

    pub fn retarget(
        &mut self,
        projection: ComposerHostProjection,
        focus_epoch: FocusEpoch,
    ) -> Result<(), ComposerError> {
        let projection = validate_projection(projection)?;
        if same_task_agent(self.fence, projection.fence) {
            return Err(ComposerError::StaleFence {
                current: self.fence,
                attempted: projection.fence,
            });
        }
        self.set_focus_epoch(focus_epoch)?;
        self.install_projection(projection, true)?;
        self.pending = None;
        self.captured = None;
        self.ime_composing = false;
        self.dirty = false;
        self.sync_control_availability();
        Ok(())
    }

    pub fn control_accessibility(
        &self,
        control: ComposerControl,
    ) -> Result<AccessibilityMetadata, ComposerError> {
        let mut metadata =
            AccessibilityMetadata::new(AccessibleRole::Button, control_label(control))?;
        let availability = self.availability(control)?;
        if !availability.is_available() {
            if let Some(reason) = availability.reason() {
                metadata.set_description(reason)?;
            }
            metadata.set_disabled(true);
        }
        if let Some(model) = self.controls.get(&control) {
            metadata.set_focused(model.state().focused());
            metadata.set_busy(model.state().is_loading() || self.pending.is_some());
        }
        Ok(metadata)
    }

    pub fn input_accessibility(&self) -> AccessibilityMetadata {
        let mut metadata = self.field.accessibility().clone();
        metadata.set_busy(self.pending.is_some() || self.ime_composing);
        metadata
    }

    pub fn pointer_down(
        &mut self,
        control: ComposerControl,
        pointer_id: u64,
        focus_epoch: FocusEpoch,
    ) -> Result<(), ComposerError> {
        self.require_epoch(focus_epoch)?;
        let Some(model) = self.controls.get_mut(&control) else {
            return Err(ComposerError::Unavailable {
                control,
                reason: bound_reason("composer control is missing")?,
            });
        };
        if !model.pointer_down(pointer_id, focus_epoch) {
            return Err(ComposerError::StalePointer {
                captured: self.focus_epoch,
                attempted: focus_epoch,
            });
        }
        self.arm(control, Some(pointer_id), focus_epoch);
        Ok(())
    }

    pub fn pointer_up(
        &mut self,
        control: ComposerControl,
        pointer_id: u64,
        focus_epoch: FocusEpoch,
    ) -> Result<Option<ComposerIntent>, ComposerError> {
        let captured = self.captured;
        let Some(model) = self.controls.get_mut(&control) else {
            return Err(ComposerError::Unavailable {
                control,
                reason: bound_reason("composer control is missing")?,
            });
        };
        let _ = model.pointer_up(pointer_id, focus_epoch);
        let matches_capture = captured.is_some_and(|captured| {
            captured.control == control
                && captured.pointer_id == Some(pointer_id)
                && captured.focus_epoch == focus_epoch
        });
        if !matches_capture || self.focus_epoch != Some(focus_epoch) {
            self.captured = None;
            return Err(ComposerError::StalePointer {
                captured: captured.map(|captured| captured.focus_epoch),
                attempted: focus_epoch,
            });
        }
        let captured = captured.expect("checked");
        if !captured.available || !self.identity_matches(captured) {
            self.captured = None;
            return Err(ComposerError::StalePointer {
                captured: Some(focus_epoch),
                attempted: focus_epoch,
            });
        }
        Ok(Some(self.activate(control, focus_epoch)?))
    }

    pub fn set_enter_preference(
        &mut self,
        preference: EnterPreference,
    ) -> Result<(), ComposerError> {
        self.enter_preference = preference;
        Ok(())
    }

    pub fn begin_ime(&mut self, focus_epoch: FocusEpoch) -> Result<(), ComposerError> {
        self.require_epoch(focus_epoch)?;
        self.ime_composing = true;
        Ok(())
    }

    pub fn end_ime(&mut self, focus_epoch: FocusEpoch) -> Result<(), ComposerError> {
        self.require_epoch(focus_epoch)?;
        self.ime_composing = false;
        Ok(())
    }

    pub fn handle_enter(
        &mut self,
        focus_epoch: FocusEpoch,
    ) -> Result<Option<ComposerIntent>, ComposerError> {
        self.require_epoch(focus_epoch)?;
        if self.ime_composing {
            return Ok(None);
        }
        match self.enter_preference {
            EnterPreference::EnterSends => {
                let control = control_for_mode(self.turn_mode);
                Ok(Some(self.activate(control, focus_epoch)?))
            }
            EnterPreference::EnterInsertsNewline => {
                self.insert_newline(focus_epoch)?;
                Ok(None)
            }
        }
    }

    pub fn handle_shift_enter(&mut self, focus_epoch: FocusEpoch) -> Result<(), ComposerError> {
        self.insert_newline(focus_epoch)
    }

    pub fn insert_prompt_version(
        &mut self,
        prompt: PromptVersionRef,
        focus_epoch: FocusEpoch,
    ) -> Result<String, ComposerError> {
        self.require_epoch(focus_epoch)?;
        let prompt_id = bound_optional_text("prompt id", &prompt.prompt_id, MAX_PROMPT_ID_SCALARS)?;
        reject_oversize("prompt body", &prompt.body, self.field.limits().max_scalars)?;
        self.field.set_value(&prompt.body)?;
        self.inserted_prompt = Some((prompt_id, prompt.version));
        self.dirty = true;
        self.auto_sent = false;
        Ok(self.field.value().to_string())
    }

    pub fn inserted_prompt_version(&self) -> Option<(&str, u64)> {
        self.inserted_prompt
            .as_ref()
            .map(|(id, version)| (id.as_str(), *version))
    }

    pub fn auto_sent(&self) -> bool {
        self.auto_sent
    }

    fn submit(
        &mut self,
        control: ComposerControl,
        fence: ComposerFence,
        focus_epoch: FocusEpoch,
        payload: ComposerPayload,
    ) -> Result<ComposerIntent, ComposerError> {
        self.require_epoch(focus_epoch)?;
        if fence != self.fence {
            return Err(ComposerError::StaleFence {
                current: self.fence,
                attempted: fence,
            });
        }
        if self.ime_composing
            && matches!(
                control,
                ComposerControl::SendNow | ComposerControl::Steer | ComposerControl::QueueFollowUp
            )
        {
            return Err(ComposerError::Unavailable {
                control,
                reason: bound_reason("IME composition is active")?,
            });
        }
        let availability = self.availability(control)?;
        if !availability.is_available() {
            return Err(ComposerError::Unavailable {
                control,
                reason: match availability.reason() {
                    Some(reason) => reason.to_string(),
                    None => bound_reason("unavailable")?,
                },
            });
        }
        let action_id = expected_action_id(control);
        if let Some(pending) = &self.pending {
            if pending.action_id == action_id
                && pending.fence == fence
                && pending.payload == payload
            {
                return Ok(pending.clone());
            }
            return Err(ComposerError::PendingConflict {
                command_id: pending.command_id,
            });
        }
        let intent = ComposerIntent {
            command_id: CommandId::new(),
            action_id,
            fence,
            payload,
        };
        self.pending = Some(intent.clone());
        self.auto_sent = false;
        Ok(intent)
    }

    fn payload_for(&self, control: ComposerControl) -> Result<ComposerPayload, ComposerError> {
        let artifact_ids = self
            .attachments
            .iter()
            .map(|attachment| attachment.artifact_id)
            .collect();
        match control {
            ComposerControl::SendNow => Ok(ComposerPayload::SendNow {
                text: require_text(control, self.field.value())?,
                artifact_ids,
                prompt: self.inserted_prompt.clone(),
            }),
            ComposerControl::Steer => Ok(ComposerPayload::Steer {
                text: require_text(control, self.field.value())?,
                artifact_ids,
                turn_id: self.fence.turn_id.ok_or(ComposerError::UnknownTurn)?,
            }),
            ComposerControl::QueueFollowUp => Ok(ComposerPayload::QueueFollowUp {
                text: require_text(control, self.field.value())?,
                artifact_ids,
            }),
            ComposerControl::StopTurn => Ok(ComposerPayload::StopTurn {
                turn_id: self.fence.turn_id.ok_or(ComposerError::UnknownTurn)?,
            }),
            ComposerControl::Answer => {
                let question = self.question.clone().ok_or(ComposerError::Unavailable {
                    control,
                    reason: bound_reason("no pending question")?,
                })?;
                Ok(ComposerPayload::Answer {
                    request_id: question.request_id,
                    state_revision: question.state_revision,
                    answer: AnswerPayload::Text(require_text(control, self.field.value())?),
                })
            }
            ComposerControl::Approval => Err(ComposerError::Unavailable {
                control,
                reason: bound_reason("approval requires an explicit decision")?,
            }),
            ComposerControl::SaveDraft => Ok(ComposerPayload::SaveDraft {
                text: self.field.value().to_string(),
                artifact_ids,
            }),
            ComposerControl::StageAttachment | ComposerControl::RemoveAttachment => {
                Err(ComposerError::Unavailable {
                    control,
                    reason: bound_reason("attachment mutation requires an ArtifactId")?,
                })
            }
        }
    }

    fn pending_consumes_current_draft(&self, pending: &ComposerIntent) -> bool {
        let attachment_ids = self
            .attachments
            .iter()
            .map(|attachment| attachment.artifact_id)
            .collect::<Vec<_>>();
        match &pending.payload {
            ComposerPayload::SendNow {
                text,
                artifact_ids,
                prompt,
            } => {
                self.field.value() == text
                    && attachment_ids == *artifact_ids
                    && self.inserted_prompt.as_ref() == prompt.as_ref()
            }
            ComposerPayload::Steer {
                text, artifact_ids, ..
            }
            | ComposerPayload::QueueFollowUp { text, artifact_ids } => {
                self.field.value() == text && attachment_ids == *artifact_ids
            }
            ComposerPayload::Answer {
                answer: AnswerPayload::Text(text),
                ..
            } => self.field.value() == text,
            ComposerPayload::Answer {
                answer: AnswerPayload::Option { .. },
                ..
            }
            | ComposerPayload::Approval { .. }
            | ComposerPayload::StopTurn { .. }
            | ComposerPayload::SaveDraft { .. }
            | ComposerPayload::StageAttachment { .. }
            | ComposerPayload::RemoveAttachment { .. } => false,
        }
    }

    fn arm(&mut self, control: ComposerControl, pointer_id: Option<u64>, focus_epoch: FocusEpoch) {
        let available = self
            .availability(control)
            .map(|availability| availability.is_available())
            .unwrap_or(false);
        self.captured = Some(CapturedActivation {
            control,
            pointer_id,
            focus_epoch,
            fence: self.fence,
            question: self
                .question
                .as_ref()
                .map(|question| (question.request_id, question.state_revision)),
            approval: self
                .approval
                .as_ref()
                .map(|approval| (approval.request_id, approval.state_revision)),
            turn_id: self.fence.turn_id,
            available,
        });
    }

    fn require_armed_submit(
        &self,
        control: ComposerControl,
        focus_epoch: FocusEpoch,
    ) -> Result<CapturedActivation, ComposerError> {
        let Some(captured) = self.captured else {
            return Err(ComposerError::StalePointer {
                captured: None,
                attempted: focus_epoch,
            });
        };
        if captured.control != control
            || captured.focus_epoch != focus_epoch
            || !captured.available
            || !self.identity_matches(captured)
        {
            return Err(if captured.fence == self.fence {
                ComposerError::StalePointer {
                    captured: Some(captured.focus_epoch),
                    attempted: focus_epoch,
                }
            } else {
                ComposerError::StaleFence {
                    current: self.fence,
                    attempted: captured.fence,
                }
            });
        }
        Ok(captured)
    }

    fn is_sealed_release(control: ComposerControl) -> bool {
        matches!(
            control,
            ComposerControl::SendNow
                | ComposerControl::Steer
                | ComposerControl::QueueFollowUp
                | ComposerControl::StopTurn
                | ComposerControl::Answer
        )
    }

    fn release_captured(
        &mut self,
        control: ComposerControl,
        fence: ComposerFence,
        focus_epoch: FocusEpoch,
    ) -> Result<ComposerIntent, ComposerError> {
        if self.pending.is_some() {
            if let Some(captured) = self.captured {
                if captured.control != control || !self.identity_matches(captured) {
                    return Err(if captured.fence == self.fence {
                        ComposerError::StalePointer {
                            captured: Some(captured.focus_epoch),
                            attempted: focus_epoch,
                        }
                    } else {
                        ComposerError::StaleFence {
                            current: self.fence,
                            attempted: captured.fence,
                        }
                    });
                }
            }
            let payload = self.payload_for(control)?;
            return self.submit(control, fence, focus_epoch, payload);
        }
        let captured = self.take_armed_submit(control, focus_epoch)?;
        if captured.fence != fence {
            return Err(ComposerError::StaleFence {
                current: captured.fence,
                attempted: fence,
            });
        }
        let payload = self.payload_from_captured(control, captured)?;
        self.submit(control, captured.fence, focus_epoch, payload)
    }

    fn take_armed_submit(
        &mut self,
        control: ComposerControl,
        focus_epoch: FocusEpoch,
    ) -> Result<CapturedActivation, ComposerError> {
        let captured = self.require_armed_submit(control, focus_epoch)?;
        self.captured = None;
        Ok(captured)
    }

    fn payload_from_captured(
        &self,
        control: ComposerControl,
        captured: CapturedActivation,
    ) -> Result<ComposerPayload, ComposerError> {
        match control {
            ComposerControl::Answer => self.payload_from_captured_answer(captured),
            ComposerControl::Steer => match self.payload_for(control)? {
                ComposerPayload::Steer {
                    text, artifact_ids, ..
                } => Ok(ComposerPayload::Steer {
                    text,
                    artifact_ids,
                    turn_id: captured.turn_id.ok_or(ComposerError::UnknownTurn)?,
                }),
                other => Ok(other),
            },
            ComposerControl::StopTurn => Ok(ComposerPayload::StopTurn {
                turn_id: captured.turn_id.ok_or(ComposerError::UnknownTurn)?,
            }),
            _ => self.payload_for(control),
        }
    }

    fn payload_from_captured_answer(
        &self,
        captured: CapturedActivation,
    ) -> Result<ComposerPayload, ComposerError> {
        let (request_id, state_revision) = captured.question.ok_or(ComposerError::Unavailable {
            control: ComposerControl::Answer,
            reason: bound_reason("captured answer is missing request identity")?,
        })?;
        Ok(ComposerPayload::Answer {
            request_id,
            state_revision,
            answer: AnswerPayload::Text(require_text(ComposerControl::Answer, self.field.value())?),
        })
    }

    fn blur_controls(&mut self) {
        for model in self.controls.values_mut() {
            model.blur();
        }
    }

    fn projection_invalidates_pending(&self, next: &ComposerHostProjection) -> bool {
        if next.fence != self.fence || next.owned_artifacts != self.owned_artifacts {
            return true;
        }
        let next_ids: Vec<ArtifactId> = next
            .draft
            .attachments
            .iter()
            .map(|attachment| attachment.artifact_id)
            .collect();
        let current_ids: Vec<ArtifactId> = self
            .attachments
            .iter()
            .map(|attachment| attachment.artifact_id)
            .collect();
        next_ids != current_ids
    }

    fn invalidate_capture_if_availability_changed(&mut self) -> Result<(), ComposerError> {
        let Some(captured) = self.captured else {
            return Ok(());
        };
        let available = self.availability(captured.control)?.is_available();
        if available != captured.available {
            self.captured = None;
        }
        Ok(())
    }

    fn identity_matches(&self, captured: CapturedActivation) -> bool {
        captured.fence == self.fence
            && captured.question
                == self
                    .question
                    .as_ref()
                    .map(|question| (question.request_id, question.state_revision))
            && captured.approval
                == self
                    .approval
                    .as_ref()
                    .map(|approval| (approval.request_id, approval.state_revision))
            && captured.turn_id == self.fence.turn_id
    }

    fn reject_stale_projection(&self, next: &ComposerFence) -> Result<(), ComposerError> {
        if next.runtime_generation < self.fence.runtime_generation
            || next.action_epoch < self.fence.action_epoch
        {
            return Err(ComposerError::StaleFence {
                current: self.fence,
                attempted: *next,
            });
        }
        Ok(())
    }

    fn stageable_artifact(&self) -> Option<ArtifactId> {
        self.owned_artifacts.iter().copied().find(|artifact_id| {
            !self
                .attachments
                .iter()
                .any(|attachment| attachment.artifact_id == *artifact_id)
        })
    }

    fn require_owned_stage(&self, artifact_id: ArtifactId) -> Result<(), ComposerError> {
        self.require_available(ComposerControl::StageAttachment)?;
        if !self.owned_artifacts.contains(&artifact_id) {
            return Err(ComposerError::AttachmentRejected {
                reason: bound_reason("artifact is not owned by this task or agent")?,
            });
        }
        if self
            .attachments
            .iter()
            .any(|attachment| attachment.artifact_id == artifact_id)
        {
            return Err(ComposerError::AttachmentRejected {
                reason: bound_reason("artifact is already staged")?,
            });
        }
        Ok(())
    }

    fn require_owned_remove(&self, artifact_id: ArtifactId) -> Result<(), ComposerError> {
        self.require_available(ComposerControl::RemoveAttachment)?;
        if !self
            .attachments
            .iter()
            .any(|attachment| attachment.artifact_id == artifact_id)
        {
            return Err(ComposerError::AttachmentRejected {
                reason: bound_reason("artifact is not staged on this task")?,
            });
        }
        Ok(())
    }

    fn require_available(&self, control: ComposerControl) -> Result<(), ComposerError> {
        let availability = self.availability(control)?;
        if availability.is_available() {
            return Ok(());
        }
        if matches!(control, ComposerControl::StopTurn | ComposerControl::Steer)
            && self.catalog_has(expected_action_id(control))
            && self.fence.turn_id.is_none()
        {
            return Err(ComposerError::UnknownTurn);
        }
        Err(ComposerError::Unavailable {
            control,
            reason: match availability.reason() {
                Some(reason) => reason.to_string(),
                None => bound_reason("unavailable")?,
            },
        })
    }

    fn require_epoch(&self, focus_epoch: FocusEpoch) -> Result<(), ComposerError> {
        if self.focus_epoch == Some(focus_epoch) {
            Ok(())
        } else {
            Err(ComposerError::StaleFocusEpoch {
                attempted: focus_epoch,
            })
        }
    }

    fn catalog_has(&self, action_id: &str) -> bool {
        self.catalog
            .iter()
            .any(|descriptor| descriptor.id == action_id)
    }

    fn ensure_controls(&mut self) {
        for control in all_controls() {
            self.controls.entry(control).or_default();
        }
    }

    fn sync_control_availability(&mut self) {
        for control in all_controls() {
            let disabled = self
                .availability(control)
                .map(|availability| !availability.is_available())
                .unwrap_or(true);
            if let Some(model) = self.controls.get_mut(&control) {
                model.set_disabled(disabled);
            }
        }
    }

    fn install_projection(
        &mut self,
        projection: ComposerHostProjection,
        replace_text: bool,
    ) -> Result<(), ComposerError> {
        if replace_text {
            self.field.set_value(projection.draft.text)?;
            self.inserted_prompt = projection
                .draft
                .prompt
                .map(|prompt| (prompt.prompt_id, prompt.version));
        }
        self.fence = projection.fence;
        self.owned_artifacts = projection.owned_artifacts;
        self.attachments = projection.draft.attachments;
        self.question = projection.question;
        self.approval = projection.approval;
        self.disabled_reasons = projection.disabled_reasons.into_iter().collect();
        Ok(())
    }
}

fn same_task_agent(left: ComposerFence, right: ComposerFence) -> bool {
    left.task_id == right.task_id && left.agent_session_id == right.agent_session_id
}

fn all_controls() -> [ComposerControl; 9] {
    [
        ComposerControl::SendNow,
        ComposerControl::Steer,
        ComposerControl::QueueFollowUp,
        ComposerControl::Answer,
        ComposerControl::Approval,
        ComposerControl::StopTurn,
        ComposerControl::SaveDraft,
        ComposerControl::StageAttachment,
        ComposerControl::RemoveAttachment,
    ]
}

fn control_for_mode(mode: ComposerTurnMode) -> ComposerControl {
    match mode {
        ComposerTurnMode::SendNow => ComposerControl::SendNow,
        ComposerTurnMode::Steer => ComposerControl::Steer,
        ComposerTurnMode::QueueFollowUp => ComposerControl::QueueFollowUp,
    }
}

fn control_label(control: ComposerControl) -> &'static str {
    match control {
        ComposerControl::SendNow => "Send Now",
        ComposerControl::Steer => "Steer",
        ComposerControl::QueueFollowUp => "Queue Follow-up",
        ComposerControl::Answer => "Answer",
        ComposerControl::Approval => "Approval",
        ComposerControl::StopTurn => "Stop turn",
        ComposerControl::SaveDraft => "Save draft",
        ComposerControl::StageAttachment => "Stage attachment",
        ComposerControl::RemoveAttachment => "Remove attachment",
    }
}

fn expected_action_id(control: ComposerControl) -> &'static str {
    match control {
        ComposerControl::SendNow => EXPECTED_ACTION_SEND_NOW,
        ComposerControl::Steer => EXPECTED_ACTION_STEER,
        ComposerControl::QueueFollowUp => EXPECTED_ACTION_QUEUE,
        ComposerControl::Answer => EXPECTED_ACTION_ANSWER,
        ComposerControl::Approval => EXPECTED_ACTION_APPROVAL,
        ComposerControl::StopTurn => EXPECTED_ACTION_STOP_TURN,
        ComposerControl::SaveDraft => EXPECTED_ACTION_SAVE_DRAFT,
        ComposerControl::StageAttachment => EXPECTED_ACTION_STAGE_ATTACHMENT,
        ComposerControl::RemoveAttachment => EXPECTED_ACTION_REMOVE_ATTACHMENT,
    }
}

fn require_text(control: ComposerControl, text: &str) -> Result<String, ComposerError> {
    if text.trim().is_empty() {
        return Err(ComposerError::Unavailable {
            control,
            reason: bound_reason("composer draft is empty")?,
        });
    }
    Ok(text.to_string())
}

fn reject_oversize(
    field: &'static str,
    value: &str,
    max_scalars: usize,
) -> Result<(), ComposerError> {
    let _ = field;
    let max_bytes = max_scalars.saturating_mul(4);
    if value.len() > max_bytes {
        return Err(ComposerError::TextBoundExceeded {
            max: max_bytes,
            actual: value.len(),
        });
    }
    let mut actual = 0usize;
    for _ in value.chars() {
        actual += 1;
        if actual > max_scalars {
            return Err(ComposerError::TextBoundExceeded {
                max: max_scalars,
                actual,
            });
        }
    }
    Ok(())
}

fn bound_reason(reason: &str) -> Result<String, ComposerError> {
    sanitize_display_text(
        "disabled reason",
        reason,
        MAX_ACCESSIBLE_DESCRIPTION_SCALARS,
    )
}

fn bound_optional_text(
    field: &'static str,
    value: &str,
    max: usize,
) -> Result<String, ComposerError> {
    let value = sanitize_display_text(field, value, max)?;
    if value.trim().is_empty() {
        return Err(ComposerError::Component(ComponentError::Empty { field }));
    }
    Ok(value)
}

fn sanitize_display_text(
    field: &'static str,
    value: &str,
    max_scalars: usize,
) -> Result<String, ComposerError> {
    reject_oversize(field, value, max_scalars)?;
    let stripped = redact_paths(&strip_bidi_and_controls(&strip_ansi(value)));
    Ok(redacted_bounded_text(
        field,
        stripped,
        max_scalars,
        max_scalars.saturating_mul(4),
    )?)
}

fn strip_ansi(value: &str) -> String {
    let mut out = String::new();
    let mut chars = value.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '\u{1b}' => match chars.peek().copied() {
                Some('[') => {
                    chars.next();
                    while let Some(next) = chars.next() {
                        if next.is_ascii_alphabetic() {
                            break;
                        }
                    }
                }
                Some(']') => {
                    chars.next();
                    while let Some(next) = chars.next() {
                        if next == '\u{07}' {
                            break;
                        }
                        if next == '\u{1b}' && chars.peek() == Some(&'\\') {
                            chars.next();
                            break;
                        }
                    }
                }
                Some('P') => {
                    chars.next();
                    while let Some(next) = chars.next() {
                        if next == '\u{1b}' && chars.peek() == Some(&'\\') {
                            chars.next();
                            break;
                        }
                    }
                }
                Some(_) => {
                    let _ = chars.next();
                }
                None => {}
            },
            '\u{9b}' => {
                while let Some(next) = chars.next() {
                    if next.is_ascii_alphabetic() {
                        break;
                    }
                }
            }
            '\u{9d}' | '\u{90}' => {
                while let Some(next) = chars.next() {
                    if next == '\u{9c}' || next == '\u{07}' {
                        break;
                    }
                }
            }
            _ => out.push(ch),
        }
    }
    out
}

fn strip_bidi_and_controls(value: &str) -> String {
    value
        .chars()
        .filter(|ch| {
            if matches!(*ch, '\n' | '\t') {
                return true;
            }
            if ch.is_control() {
                return false;
            }
            !matches!(
                *ch,
                '\u{061C}'
                    | '\u{200E}'
                    | '\u{200F}'
                    | '\u{202A}'..='\u{202E}'
                    | '\u{2066}'..='\u{2069}'
            )
        })
        .collect()
}

fn redact_paths(value: &str) -> String {
    value
        .split(' ')
        .map(|token| {
            if token_looks_like_path(token) {
                "[redacted-path]"
            } else {
                token
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn token_looks_like_path(token: &str) -> bool {
    let trimmed = token.trim();
    if trimmed.is_empty() {
        return false;
    }
    trimmed.contains('\\')
        || trimmed.starts_with('/')
        || trimmed.starts_with("~/")
        || trimmed.starts_with("file:")
        || trimmed.starts_with("\\\\")
        || trimmed.chars().nth(1) == Some(':')
        || trimmed.contains("/home/")
        || trimmed.contains("/Users/")
}

fn bound_outbound_answer(answer: AnswerPayload) -> Result<AnswerPayload, ComposerError> {
    match answer {
        AnswerPayload::Text(text) => {
            reject_oversize("answer text", &text, MAX_ACCESSIBLE_DESCRIPTION_SCALARS)?;
            if text.trim().is_empty() {
                return Err(ComposerError::Component(ComponentError::Empty {
                    field: "answer text",
                }));
            }
            Ok(AnswerPayload::Text(text))
        }
        AnswerPayload::Option { index, label } => {
            reject_oversize("answer option", &label, MAX_ACCESSIBLE_NAME_SCALARS)?;
            if label.trim().is_empty() {
                return Err(ComposerError::Component(ComponentError::Empty {
                    field: "answer option",
                }));
            }
            Ok(AnswerPayload::Option { index, label })
        }
    }
}

fn bound_outbound_decision(decision: ApprovalDecision) -> Result<ApprovalDecision, ComposerError> {
    match decision {
        ApprovalDecision::Approve => Ok(ApprovalDecision::Approve),
        ApprovalDecision::Reject {
            reason: Some(reason),
        } => {
            reject_oversize(
                "approval reason",
                &reason,
                MAX_ACCESSIBLE_DESCRIPTION_SCALARS,
            )?;
            Ok(ApprovalDecision::Reject {
                reason: Some(reason),
            })
        }
        ApprovalDecision::Reject { reason: None } => Ok(ApprovalDecision::Reject { reason: None }),
    }
}

fn validate_projection(
    mut projection: ComposerHostProjection,
) -> Result<ComposerHostProjection, ComposerError> {
    reject_oversize(
        "composer draft",
        &projection.draft.text,
        TextFieldLimits::default().max_scalars,
    )?;
    if projection.owned_artifacts.len() > MAX_OWNED_ARTIFACTS {
        return Err(ComposerError::AttachmentRejected {
            reason: bound_reason("owned artifact count exceeds the composer bound")?,
        });
    }
    if projection.draft.attachments.len() > MAX_COMPOSER_ATTACHMENTS {
        return Err(ComposerError::AttachmentRejected {
            reason: bound_reason("attachment count exceeds the composer bound")?,
        });
    }
    for attachment in &mut projection.draft.attachments {
        if !projection
            .owned_artifacts
            .iter()
            .any(|owned| *owned == attachment.artifact_id)
        {
            return Err(ComposerError::AttachmentRejected {
                reason: bound_reason("artifact is not owned by this task or agent")?,
            });
        }
        attachment.label = bound_optional_text(
            "attachment label",
            &attachment.label,
            MAX_ACCESSIBLE_NAME_SCALARS,
        )?;
    }
    if let Some(prompt) = &mut projection.draft.prompt {
        prompt.prompt_id =
            bound_optional_text("prompt id", &prompt.prompt_id, MAX_PROMPT_ID_SCALARS)?;
        reject_oversize(
            "prompt body",
            &prompt.body,
            TextFieldLimits::default().max_scalars,
        )?;
    }
    if let Some(question) = &mut projection.question {
        if question.options.len() > MAX_QUESTION_OPTIONS {
            return Err(ComposerError::TextBoundExceeded {
                max: MAX_QUESTION_OPTIONS,
                actual: question.options.len(),
            });
        }
        for option in &question.options {
            reject_oversize("question option", option, MAX_ACCESSIBLE_NAME_SCALARS)?;
            if option.trim().is_empty() {
                return Err(ComposerError::Component(ComponentError::Empty {
                    field: "question option",
                }));
            }
        }
    }
    if projection.disabled_reasons.len() > MAX_DISABLED_REASONS {
        return Err(ComposerError::TextBoundExceeded {
            max: MAX_DISABLED_REASONS,
            actual: projection.disabled_reasons.len(),
        });
    }
    let mut reasons = Vec::new();
    for (control, reason) in projection.disabled_reasons {
        reject_oversize(
            "disabled reason",
            &reason,
            MAX_ACCESSIBLE_DESCRIPTION_SCALARS,
        )?;
        reasons.push((control, bound_reason(&reason)?));
    }
    projection.disabled_reasons = reasons;
    Ok(projection)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::action::{ActionArgumentSchema, ActionRisk, ActionScope};
    use crate::protocol::Capability;
    use crate::ui::components::interaction::FocusEpochSource;

    fn redesign_tokens() -> ThemeTokens {
        crate::ui::tokens::dark(
            crate::ui::tokens::Density::Comfortable,
            crate::ui::tokens::Scale::Scale100,
        )
    }

    #[test]
    fn the_composer_type_scale_is_the_redesign_scale() {
        // Rule 2, and its ceiling: nothing the composer paints is over 13 px.
        assert_eq!(COMPOSER_CAPTION_FONT_SIZE, 10.5);
        assert_eq!(COMPOSER_CHIP_FONT_SIZE, 10.5);
        assert_eq!(COMPOSER_SECONDARY_FONT_SIZE, 11.0);
        assert_eq!(COMPOSER_BUTTON_FONT_SIZE, 11.0);
        assert_eq!(COMPOSER_FONT_SIZE, 11.5);
        for size in [
            COMPOSER_CAPTION_FONT_SIZE,
            COMPOSER_CHIP_FONT_SIZE,
            COMPOSER_SECONDARY_FONT_SIZE,
            COMPOSER_BUTTON_FONT_SIZE,
            COMPOSER_FONT_SIZE,
        ] {
            assert!(size <= 13.0, "found {size} px in the composer");
        }
    }

    #[test]
    fn the_composer_geometry_is_the_redesign_geometry() {
        // Rule 3: radius 4 for chips, 6 for controls and the input, one pixel
        // of rule. Rule 4: a 24 px icon button around a 14 px glyph. Rule 6:
        // the 4/8/10/12 grid, chip gap 6, control gap 8.
        assert_eq!(COMPOSER_RADIUS, 6.0);
        assert_eq!(COMPOSER_BUTTON_RADIUS, 6.0);
        assert_eq!(COMPOSER_CHIP_RADIUS, 4.0);
        assert_eq!(COMPOSER_BORDER_WIDTH, 1.0);
        assert_eq!(COMPOSER_ICON_BUTTON_SIZE, 24.0);
        assert_eq!(COMPOSER_ICON_GLYPH_SIZE, 14.0);
        assert_eq!(COMPOSER_CHIP_GAP, 6.0);
        assert_eq!(COMPOSER_CONTROL_GAP, 8.0);
        assert_eq!(COMPOSER_REGION_PADDING, 10.0);
        assert_eq!((COMPOSER_PADDING_X, COMPOSER_PADDING_Y), (10.0, 6.0));
        assert_eq!(
            (COMPOSER_BUTTON_PADDING_X, COMPOSER_BUTTON_PADDING_Y),
            (8.0, 2.0)
        );
        assert_eq!(
            (COMPOSER_CHIP_PADDING_X, COMPOSER_CHIP_PADDING_Y),
            (6.0, 1.0)
        );
    }

    /// Fix wave 1, F3: the stream is `flex_1`, the composer is `flex_none`
    /// behind a ceiling. The user's `5.png` shows the opposite -- the draft
    /// grew the composer until it overflowed the panel and was clipped at the
    /// bottom edge -- and `4.png` shows what that costs the stream: about
    /// 60 px of it, at the top of a 330 px body.
    #[test]
    fn the_composer_field_is_bounded_at_six_lines_and_the_reserve_follows_it() {
        // One line and six lines, each including the field's own padding.
        assert_eq!(COMPOSER_MAX_VISIBLE_LINES, 6.0);
        assert_eq!(COMPOSER_INPUT_MIN_HEIGHT, COMPOSER_LINE_HEIGHT + 12.0);
        assert_eq!(COMPOSER_INPUT_MAX_HEIGHT, 6.0 * COMPOSER_LINE_HEIGHT + 12.0);
        // The brief's "~110 px": six 11.5 px lines at 1.5 leading plus 6 px of
        // padding above and below.
        assert!(
            (COMPOSER_INPUT_MAX_HEIGHT - 115.5).abs() < 0.001,
            "six lines of 11.5/1.5 plus padding is 115.5, found {COMPOSER_INPUT_MAX_HEIGHT}"
        );
        // A field that can only ever be six lines tall must not be able to
        // start taller than it may end.
        assert!(COMPOSER_INPUT_MIN_HEIGHT < COMPOSER_INPUT_MAX_HEIGHT);

        // The reserve is the composer's real maximum, part for part. Written
        // as the sum rather than as a number so a change to any part moves it.
        assert_eq!(
            COMPOSER_HEIGHT_RESERVE,
            COMPOSER_CONTROL_GAP
                + 2.0 * COMPOSER_BORDER_WIDTH
                + COMPOSER_INPUT_MAX_HEIGHT
                + COMPOSER_CHIP_GAP
                + COMPOSER_META_ROW_HEIGHT
                + COMPOSER_REGION_PADDING
        );
        // And it is a real reduction on the 200 px the shipped composer took,
        // which is the whole point of collapsing the four stacked rows (F4).
        assert!(
            COMPOSER_HEIGHT_RESERVE < 200.0,
            "the collapsed composer must reserve less than the stacked one, found \
             {COMPOSER_HEIGHT_RESERVE}"
        );
        // The stream is what is left. At the 330 px body a panel gets as one
        // of eight, `4.png` left the stream about 60 px; the bound above has
        // to buy it multiples of that, which is the claim this fix makes.
        let body = 330.0_f32;
        let stream = body - COMPOSER_HEIGHT_RESERVE;
        assert!(
            stream >= 2.5 * 60.0,
            "the stream must keep multiples of the 60 px `4.png` showed; it keeps {stream}"
        );
    }

    #[test]
    fn the_send_control_is_quiet_and_only_brightens_when_it_can_send() {
        // Rule 4: an icon button rests at `text.muted` and reaches
        // `text.primary` on hover.
        let tokens = redesign_tokens();
        // (enabled, pending, streaming)
        assert_eq!(
            composer_send_look(true, false, false),
            ComposerSendLook::Ready
        );
        assert_eq!(
            composer_send_look(false, false, false),
            ComposerSendLook::Idle
        );
        assert_eq!(
            composer_send_look(true, true, false),
            ComposerSendLook::Idle,
            "a submission in flight with nothing to stop must not stay a send target"
        );
        assert_eq!(
            composer_send_look(true, true, true),
            ComposerSendLook::Busy,
            "a stoppable turn outranks everything else in the slot"
        );
        assert_eq!(
            composer_send_look(false, false, true),
            ComposerSendLook::Busy,
            "Stop does not need a sendable draft"
        );

        let (rest, hover) = composer_send_tints(ComposerSendLook::Ready, tokens);
        assert_eq!(rest, tokens.text.muted);
        assert_eq!(hover, tokens.text.primary);
        assert_ne!(rest, hover, "a target must show that it is one");

        let (rest, hover) = composer_send_tints(ComposerSendLook::Idle, tokens);
        assert_eq!(rest, tokens.text.disabled);
        assert_eq!(
            rest, hover,
            "a control that cannot be used must not brighten under the pointer"
        );

        // Rule 1: the only colour the slot may spend is red, and only on Stop.
        // Asserted as membership rather than as a list of inequalities,
        // because in this palette the primary action's background is
        // deliberately `text.primary` itself -- an inequality against
        // `actions.primary` would go red for the right colour.
        let text_family = [
            tokens.text.primary,
            tokens.text.emphasis,
            tokens.text.secondary,
            tokens.text.muted,
            tokens.text.disabled,
        ];
        for look in [ComposerSendLook::Ready, ComposerSendLook::Idle] {
            let (rest, hover) = composer_send_tints(look, tokens);
            for tint in [rest, hover] {
                assert!(
                    text_family.contains(&tint),
                    "only Stop is coloured; {look:?} painted {tint:?}"
                );
                assert_ne!(tint, tokens.status.attention);
                assert_ne!(tint, tokens.status.success);
                assert_ne!(tint, tokens.status.destructive);
            }
        }
    }

    #[test]
    fn a_streaming_turn_turns_the_send_slot_into_a_red_stop() {
        // The coordinator's ruling: while a turn is streaming the slot is a
        // 24 px red icon button that stops the turn, and it is its own
        // accessibility node so nothing announces "Send" over a Stop.
        let tokens = redesign_tokens();

        assert_eq!(
            composer_send_glyph(ComposerSendLook::Busy),
            COMPOSER_STOP_GLYPH
        );
        for look in [ComposerSendLook::Ready, ComposerSendLook::Idle] {
            assert_eq!(composer_send_glyph(look), COMPOSER_SEND_GLYPH);
        }
        assert_ne!(
            COMPOSER_STOP_GLYPH, COMPOSER_SEND_GLYPH,
            "Stop and Send must not be the same mark"
        );

        // Rule 4's destructive control: red text, no fill, `text.primary` on
        // hover. The ground under the hover is the painter's `surfaces.hover`.
        let (rest, hover) = composer_send_tints(ComposerSendLook::Busy, tokens);
        assert_eq!(rest, tokens.status.destructive);
        assert_eq!(hover, tokens.text.primary);
        assert_ne!(rest, hover, "Stop is a target and must show that it is one");

        // The node exists only while streaming, and it is the only state that
        // carries the Stop id.
        assert_eq!(
            composer_send_element_id(ComposerSendLook::Busy),
            COMPOSER_STOP_ELEMENT_ID
        );
        assert_eq!(COMPOSER_STOP_ELEMENT_ID, "native-composer-stop");
        for look in [ComposerSendLook::Ready, ComposerSendLook::Idle] {
            assert_eq!(composer_send_element_id(look), COMPOSER_SEND_ELEMENT_ID);
            assert_ne!(composer_send_element_id(look), COMPOSER_STOP_ELEMENT_ID);
        }
        // And the only state reached without a streaming turn is never Busy,
        // so the Stop id is unreachable while nothing is streaming.
        for enabled in [true, false] {
            for pending in [true, false] {
                let look = composer_send_look(enabled, pending, false);
                assert_ne!(look, ComposerSendLook::Busy);
                assert_eq!(composer_send_element_id(look), COMPOSER_SEND_ELEMENT_ID);
            }
        }
    }

    const FIXTURE_CATALOG: &[ActionDescriptor] = &[
        ActionDescriptor {
            id: EXPECTED_ACTION_SEND_NOW,
            title: "Send now",
            description: "Send now",
            keywords: &["send"],
            scope: ActionScope::Task,
            required_capability: Some(Capability::SemanticConversation),
            risk: ActionRisk::Mutating,
            argument_schema: ActionArgumentSchema::None,
        },
        ActionDescriptor {
            id: EXPECTED_ACTION_STEER,
            title: "Steer",
            description: "Steer",
            keywords: &["steer"],
            scope: ActionScope::Task,
            required_capability: Some(Capability::SemanticConversation),
            risk: ActionRisk::Mutating,
            argument_schema: ActionArgumentSchema::None,
        },
        ActionDescriptor {
            id: EXPECTED_ACTION_QUEUE,
            title: "Queue follow-up",
            description: "Queue follow-up",
            keywords: &["queue"],
            scope: ActionScope::Task,
            required_capability: Some(Capability::SemanticConversation),
            risk: ActionRisk::Mutating,
            argument_schema: ActionArgumentSchema::None,
        },
        ActionDescriptor {
            id: EXPECTED_ACTION_ANSWER,
            title: "Answer question",
            description: "Answer question",
            keywords: &["answer"],
            scope: ActionScope::Task,
            required_capability: Some(Capability::SemanticConversation),
            risk: ActionRisk::Mutating,
            argument_schema: ActionArgumentSchema::None,
        },
        ActionDescriptor {
            id: EXPECTED_ACTION_APPROVAL,
            title: "Resolve approval",
            description: "Resolve approval",
            keywords: &["approval"],
            scope: ActionScope::Task,
            required_capability: Some(Capability::SemanticConversation),
            risk: ActionRisk::Mutating,
            argument_schema: ActionArgumentSchema::None,
        },
        ActionDescriptor {
            id: EXPECTED_ACTION_STOP_TURN,
            title: "Stop turn",
            description: "Stop turn",
            keywords: &["stop"],
            scope: ActionScope::Task,
            required_capability: Some(Capability::SemanticConversation),
            risk: ActionRisk::Mutating,
            argument_schema: ActionArgumentSchema::None,
        },
        ActionDescriptor {
            id: EXPECTED_ACTION_SAVE_DRAFT,
            title: "Save composer draft",
            description: "Save composer draft",
            keywords: &["draft"],
            scope: ActionScope::Task,
            required_capability: Some(Capability::SemanticConversation),
            risk: ActionRisk::Mutating,
            argument_schema: ActionArgumentSchema::None,
        },
        ActionDescriptor {
            id: EXPECTED_ACTION_STAGE_ATTACHMENT,
            title: "Stage composer attachment",
            description: "Stage composer attachment",
            keywords: &["attach"],
            scope: ActionScope::Task,
            required_capability: Some(Capability::SemanticConversation),
            risk: ActionRisk::Mutating,
            argument_schema: ActionArgumentSchema::None,
        },
        ActionDescriptor {
            id: EXPECTED_ACTION_REMOVE_ATTACHMENT,
            title: "Remove composer attachment",
            description: "Remove composer attachment",
            keywords: &["detach"],
            scope: ActionScope::Task,
            required_capability: Some(Capability::SemanticConversation),
            risk: ActionRisk::Mutating,
            argument_schema: ActionArgumentSchema::None,
        },
    ];

    fn fence() -> ComposerFence {
        ComposerFence {
            task_id: TaskId::new(),
            agent_session_id: AgentSessionId::new(),
            runtime_generation: 4,
            action_epoch: 11,
            turn_id: Some(TurnId::from_raw(21)),
        }
    }

    fn projection_with(fence: ComposerFence, text: &str) -> ComposerHostProjection {
        ComposerHostProjection {
            fence,
            draft: ComposerDraftProjection {
                text: text.to_string(),
                attachments: Vec::new(),
                prompt: None,
            },
            owned_artifacts: Vec::new(),
            question: None,
            approval: None,
            disabled_reasons: Vec::new(),
        }
    }

    fn bind_granted(text: &str) -> TaskComposer {
        TaskComposer::bind_with_catalog(projection_with(fence(), text), FIXTURE_CATALOG)
            .expect("fixture catalog grants composer actions")
    }

    fn focus(composer: &mut TaskComposer, epochs: &mut FocusEpochSource) {
        let epoch = epochs.current();
        composer.set_focus_epoch(epoch).expect("focus epoch");
        composer.focus_input(epoch).expect("focus input");
    }

    #[test]
    fn attachment_only_send_requires_explicit_activation() {
        let mut composer = bind_granted("");
        let mut epochs = FocusEpochSource::new();
        focus(&mut composer, &mut epochs);
        let epoch = epochs.current();

        let ordinary = composer
            .activate(ComposerControl::SendNow, epoch)
            .expect_err("ordinary empty sends remain invalid");
        assert!(ordinary.to_string().contains("empty"));

        let intent = composer
            .activate_attachment_only_send(epoch)
            .expect("an explicitly attached image may supply the provider prompt later");
        assert!(matches!(
            intent.payload,
            ComposerPayload::SendNow { ref text, .. } if text.is_empty()
        ));
    }

    #[test]
    fn retry_returns_frozen_payload_after_adversarial_edits() {
        let artifact = ArtifactId::new();
        let mut projection = projection_with(fence(), "retry me");
        projection.owned_artifacts.push(artifact);
        projection
            .draft
            .attachments
            .push(ComposerAttachmentProjection {
                artifact_id: artifact,
                kind: AttachmentKind::File,
                label: "notes.md".into(),
            });
        let mut composer =
            TaskComposer::bind_with_catalog(projection, FIXTURE_CATALOG).expect("granted");
        let mut epochs = FocusEpochSource::new();
        focus(&mut composer, &mut epochs);
        let epoch = epochs.current();
        let first = composer
            .activate(ComposerControl::SendNow, epoch)
            .expect("first submit");
        let frozen = first.clone();
        composer
            .replace_draft("edited after submit", epoch)
            .expect("edit");
        composer
            .set_turn_mode(ComposerTurnMode::QueueFollowUp)
            .expect("mode");
        let mut changed = projection_with(composer.fence(), "edited after submit");
        changed.owned_artifacts.push(artifact);
        changed
            .draft
            .attachments
            .push(ComposerAttachmentProjection {
                artifact_id: artifact,
                kind: AttachmentKind::File,
                label: "notes.md".into(),
            });
        composer
            .apply_projection(changed, epoch)
            .expect("same ownership refresh");
        let retry = composer.retry_pending(epoch).expect("retry");
        assert_eq!(retry, frozen);
        match &retry.payload {
            ComposerPayload::SendNow {
                text, artifact_ids, ..
            } => {
                assert_eq!(text, "retry me");
                assert_eq!(artifact_ids.as_slice(), [artifact].as_slice());
            }
            other => panic!("expected frozen send payload, got {other:?}"),
        }
    }

    #[test]
    fn pending_conflict_and_settle_mint_distinct_command_ids() {
        let mut composer = bind_granted("original");
        let mut epochs = FocusEpochSource::new();
        focus(&mut composer, &mut epochs);
        let epoch = epochs.current();
        let first = composer
            .activate(ComposerControl::SendNow, epoch)
            .expect("pending");
        composer
            .replace_draft("different payload", epoch)
            .expect("edit");
        let conflict = composer
            .activate(ComposerControl::SendNow, epoch)
            .expect_err("conflict");
        assert!(matches!(
            conflict,
            ComposerError::PendingConflict { command_id } if command_id == first.command_id
        ));
        composer.cancel_pending(first.command_id).expect("cancel");
        composer.replace_draft("second turn", epoch).expect("edit");
        composer
            .focus_input(epoch)
            .expect("re-arm after take-then-consume");
        let second = composer
            .activate(ComposerControl::SendNow, epoch)
            .expect("new command");
        assert_ne!(first.command_id, second.command_id);
    }

    #[test]
    fn accepted_text_submission_clears_only_the_exact_consumed_draft() {
        let mut composer = bind_granted("ship this exact draft");
        let mut epochs = FocusEpochSource::new();
        focus(&mut composer, &mut epochs);
        let epoch = epochs.current();
        let submitted = composer
            .activate(ComposerControl::SendNow, epoch)
            .expect("submit");

        composer
            .settle_pending(submitted.command_id)
            .expect("matching host acceptance");

        assert_eq!(composer.draft_text(), "");
        assert!(composer.attachments().is_empty());
        assert!(composer.pending_intent().is_none());
    }

    #[test]
    fn accepted_text_submission_preserves_a_newer_local_edit() {
        let mut composer = bind_granted("first draft");
        let mut epochs = FocusEpochSource::new();
        focus(&mut composer, &mut epochs);
        let epoch = epochs.current();
        let submitted = composer
            .activate(ComposerControl::SendNow, epoch)
            .expect("submit");
        composer
            .replace_draft("follow-up typed while the host accepted", epoch)
            .expect("newer local edit");

        composer
            .settle_pending(submitted.command_id)
            .expect("matching host acceptance");

        assert_eq!(
            composer.draft_text(),
            "follow-up typed while the host accepted"
        );
        assert!(composer.pending_intent().is_none());
    }

    #[test]
    fn answer_approval_and_stop_carry_request_and_turn_identity() {
        let question = RequestId::new();
        let approval = RequestId::new();
        let mut projection = projection_with(fence(), "");
        projection.question = Some(QuestionProjection {
            request_id: question,
            state_revision: 3,
            options: vec!["Ship it".into()],
        });
        projection.approval = Some(ApprovalProjection {
            request_id: approval,
            state_revision: 8,
        });
        let mut composer =
            TaskComposer::bind_with_catalog(projection, FIXTURE_CATALOG).expect("granted");
        let mut epochs = FocusEpochSource::new();
        focus(&mut composer, &mut epochs);
        let epoch = epochs.current();
        composer
            .focus_control(ComposerControl::Answer, epoch)
            .expect("focus answer");
        let answered = composer
            .activate_answer(
                question,
                3,
                composer.fence(),
                AnswerPayload::Option {
                    index: 0,
                    label: "Ship it".into(),
                },
                epoch,
            )
            .expect("answer");
        match answered.payload {
            ComposerPayload::Answer {
                request_id,
                state_revision,
                answer: AnswerPayload::Option { index, label },
            } => {
                assert_eq!(
                    (request_id, state_revision, index, label.as_str()),
                    (question, 3, 0, "Ship it")
                );
            }
            other => panic!("{other:?}"),
        }
        composer
            .settle_pending(answered.command_id)
            .expect("settle");
        composer
            .focus_control(ComposerControl::Approval, epoch)
            .expect("focus approval");
        let decided = composer
            .activate_approval(
                approval,
                8,
                composer.fence(),
                ApprovalDecision::Approve,
                epoch,
            )
            .expect("approval");
        match decided.payload {
            ComposerPayload::Approval {
                request_id,
                state_revision,
                decision: ApprovalDecision::Approve,
            } => assert_eq!((request_id, state_revision), (approval, 8)),
            other => panic!("{other:?}"),
        }
        composer.settle_pending(decided.command_id).expect("settle");
        assert!(matches!(
            composer
                .activate_approval(
                    approval,
                    8,
                    composer.fence(),
                    ApprovalDecision::Approve,
                    epoch,
                )
                .expect_err("approval capture was consumed"),
            ComposerError::StalePointer { .. }
        ));
        composer
            .focus_control(ComposerControl::StopTurn, epoch)
            .expect("focus stop");
        let stopped = composer
            .activate(ComposerControl::StopTurn, epoch)
            .expect("stop");
        match stopped.payload {
            ComposerPayload::StopTurn { turn_id } => {
                assert_eq!(turn_id, composer.fence().turn_id.unwrap())
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn unknown_turn_and_stale_question_are_typed_and_non_writing() {
        let mut projection = projection_with(fence(), "nudge");
        projection.fence.turn_id = None;
        let mut composer =
            TaskComposer::bind_with_catalog(projection, FIXTURE_CATALOG).expect("granted");
        let mut epochs = FocusEpochSource::new();
        focus(&mut composer, &mut epochs);
        let epoch = epochs.current();
        assert!(matches!(
            composer
                .activate(ComposerControl::StopTurn, epoch)
                .expect_err("stop"),
            ComposerError::UnknownTurn
        ));
        let question = RequestId::new();
        let mut with_question = projection_with(composer.fence(), "");
        with_question.question = Some(QuestionProjection {
            request_id: question,
            state_revision: 2,
            options: vec!["A".into()],
        });
        composer
            .apply_projection(with_question, epoch)
            .expect("question");
        assert!(matches!(
            composer
                .activate_answer(
                    question,
                    1,
                    composer.fence(),
                    AnswerPayload::Text("late".into()),
                    epoch,
                )
                .expect_err("stale"),
            ComposerError::StaleRequest { .. }
        ));
        assert!(composer.pending_intent().is_none());
    }

    #[test]
    fn capability_refresh_disables_previously_enabled_controls() {
        let mut composer = bind_granted("still typing");
        let mut epochs = FocusEpochSource::new();
        focus(&mut composer, &mut epochs);
        let epoch = epochs.current();
        assert!(composer
            .availability(ComposerControl::SendNow)
            .expect("availability")
            .is_available());
        let mut refreshed = projection_with(composer.fence(), "still typing");
        refreshed.disabled_reasons.push((
            ComposerControl::SendNow,
            "provider cannot guarantee send semantics".into(),
        ));
        composer
            .apply_projection(refreshed, epoch)
            .expect("refresh");
        let availability = composer
            .availability(ComposerControl::SendNow)
            .expect("refreshed");
        assert!(!availability.is_available());
        assert_eq!(
            availability.reason(),
            Some("provider cannot guarantee send semantics")
        );
        assert!(composer.activate(ComposerControl::SendNow, epoch).is_err());
    }

    #[test]
    fn pointer_release_after_question_revision_change_never_targets_replacement() {
        let question = RequestId::new();
        let mut projection = projection_with(fence(), "answer later");
        projection.question = Some(QuestionProjection {
            request_id: question,
            state_revision: 1,
            options: vec!["A".into()],
        });
        let mut composer =
            TaskComposer::bind_with_catalog(projection, FIXTURE_CATALOG).expect("granted");
        let mut epochs = FocusEpochSource::new();
        focus(&mut composer, &mut epochs);
        let epoch = epochs.current();
        composer
            .pointer_down(ComposerControl::Answer, 3, epoch)
            .expect("down");
        let mut next = projection_with(composer.fence(), "answer later");
        next.question = Some(QuestionProjection {
            request_id: question,
            state_revision: 2,
            options: vec!["A".into()],
        });
        composer
            .apply_projection(next, epoch)
            .expect("revision advanced");
        let rejected = composer
            .pointer_up(ComposerControl::Answer, 3, epoch)
            .expect_err("delayed release");
        assert!(matches!(rejected, ComposerError::StalePointer { .. }));
        assert!(composer.pending_intent().is_none());
    }

    #[test]
    fn stage_and_remove_are_typed_intents_only_for_owned_artifacts() {
        let owned = ArtifactId::new();
        let foreign = ArtifactId::new();
        let mut projection = projection_with(fence(), "files");
        projection.owned_artifacts.push(owned);
        let mut composer =
            TaskComposer::bind_with_catalog(projection, FIXTURE_CATALOG).expect("granted");
        let mut epochs = FocusEpochSource::new();
        focus(&mut composer, &mut epochs);
        let epoch = epochs.current();
        assert!(composer
            .availability(ComposerControl::StageAttachment)
            .expect("stage")
            .is_available());
        let staged = composer.activate_stage(owned, epoch).expect("stage owned");
        assert!(matches!(
            staged.payload,
            ComposerPayload::StageAttachment { artifact_id } if artifact_id == owned
        ));
        composer.settle_pending(staged.command_id).expect("settle");
        assert!(matches!(
            composer
                .activate_stage(foreign, epoch)
                .expect_err("foreign"),
            ComposerError::AttachmentRejected { .. }
        ));

        let mut with_attachment = projection_with(composer.fence(), "files");
        with_attachment.owned_artifacts.push(owned);
        with_attachment
            .draft
            .attachments
            .push(ComposerAttachmentProjection {
                artifact_id: owned,
                kind: AttachmentKind::File,
                label: "notes.md".into(),
            });
        composer
            .apply_projection(with_attachment, epoch)
            .expect("host staged");
        let removed = composer.activate_remove(owned, epoch).expect("remove");
        assert!(matches!(
            removed.payload,
            ComposerPayload::RemoveAttachment { artifact_id } if artifact_id == owned
        ));
    }

    #[test]
    fn enter_after_same_epoch_retarget_does_not_submit_replacement() {
        let mut composer = bind_granted("original task");
        let mut epochs = FocusEpochSource::new();
        focus(&mut composer, &mut epochs);
        let epoch = epochs.current();
        composer
            .set_enter_preference(EnterPreference::EnterSends)
            .expect("pref");

        let mut next = composer.fence();
        next.task_id = TaskId::new();
        next.agent_session_id = AgentSessionId::new();
        composer
            .retarget(projection_with(next, "replacement task"), epoch)
            .expect("same-epoch retarget");
        assert_eq!(composer.draft_text(), "replacement task");

        let rejected = composer
            .handle_enter(epoch)
            .expect_err("unarmed Enter cannot submit the replacement");
        assert!(matches!(
            rejected,
            ComposerError::StalePointer { .. } | ComposerError::StaleFence { .. }
        ));
        assert!(composer.pending_intent().is_none());
    }

    #[test]
    fn pointer_release_after_enable_refresh_does_not_submit() {
        let mut projection = projection_with(fence(), "do not fire");
        projection.disabled_reasons.push((
            ComposerControl::SendNow,
            "provider cannot guarantee send semantics".into(),
        ));
        let mut composer =
            TaskComposer::bind_with_catalog(projection, FIXTURE_CATALOG).expect("granted");
        let mut epochs = FocusEpochSource::new();
        focus(&mut composer, &mut epochs);
        let epoch = epochs.current();
        assert!(!composer
            .availability(ComposerControl::SendNow)
            .expect("disabled at arm")
            .is_available());
        assert!(matches!(
            composer
                .pointer_down(ComposerControl::SendNow, 4, epoch)
                .expect_err("disabled press is not armed"),
            ComposerError::StalePointer { .. }
        ));

        composer
            .apply_projection(projection_with(composer.fence(), "do not fire"), epoch)
            .expect("same-fence enable refresh");
        assert!(composer
            .availability(ComposerControl::SendNow)
            .expect("now enabled")
            .is_available());
        let rejected = composer
            .pointer_up(ComposerControl::SendNow, 4, epoch)
            .expect_err("disabled-at-arm release cannot fire");
        assert!(matches!(rejected, ComposerError::StalePointer { .. }));
        assert!(composer.pending_intent().is_none());
    }

    #[test]
    fn host_reason_is_rejected_before_sanitizer_allocation() {
        let mut projection = projection_with(fence(), "safe");
        let oversize = format!(
            "{}ok",
            "\u{1b}[31m".repeat(MAX_ACCESSIBLE_DESCRIPTION_SCALARS.saturating_mul(4) / 4 + 1)
        );
        assert!(oversize.len() > MAX_ACCESSIBLE_DESCRIPTION_SCALARS.saturating_mul(4));
        projection
            .disabled_reasons
            .push((ComposerControl::SendNow, oversize));
        let error = TaskComposer::bind(projection)
            .expect_err("raw cap+1 must fail before ANSI stripping can shrink the reason");
        assert!(matches!(error, ComposerError::TextBoundExceeded { .. }));
    }

    #[test]
    fn host_reason_escape_plus_multibyte_does_not_panic() {
        let mut projection = projection_with(fence(), "safe");
        projection
            .disabled_reasons
            .push((ComposerControl::SendNow, "\u{1b}界safe".into()));
        let composer = TaskComposer::bind_with_catalog(projection, FIXTURE_CATALOG)
            .expect("escape plus multibyte must not panic in strip_ansi");
        let reason = composer
            .availability(ComposerControl::SendNow)
            .expect("reason")
            .reason()
            .expect("disabled")
            .to_string();
        assert!(!reason.contains('\u{1b}'));
        assert!(reason.contains("safe"));
    }

    #[test]
    fn rejected_projection_does_not_mutate_draft_pending_or_capture() {
        let mut composer = bind_granted("keep overlay");
        let mut epochs = FocusEpochSource::new();
        focus(&mut composer, &mut epochs);
        let epoch = epochs.current();
        composer
            .replace_draft("ephemeral", epoch)
            .expect("dirty overlay");
        let pending = composer
            .activate(ComposerControl::SendNow, epoch)
            .expect("pending");
        composer
            .pointer_down(ComposerControl::Steer, 8, epoch)
            .expect("capture");

        let mut bad = projection_with(composer.fence(), "keep overlay");
        bad.disabled_reasons.push((
            ComposerControl::SendNow,
            "x".repeat(MAX_ACCESSIBLE_DESCRIPTION_SCALARS.saturating_mul(4) + 1),
        ));
        assert!(matches!(
            composer.apply_projection(bad, epoch).expect_err("rejected"),
            ComposerError::TextBoundExceeded { .. }
        ));
        assert_eq!(composer.draft_text(), "ephemeral");
        assert_eq!(
            composer.pending_intent().map(|intent| intent.command_id),
            Some(pending.command_id)
        );
        let rejected = composer
            .pointer_up(ComposerControl::Steer, 8, epoch)
            .expect_err("capture survived the rejected apply");
        assert!(
            !matches!(rejected, ComposerError::StalePointer { captured: None, .. }),
            "rejected apply must not drop the armed capture: {rejected:?}"
        );
    }

    #[test]
    fn pending_cleared_when_fence_or_owned_artifacts_change() {
        let mut composer = bind_granted("retry me");
        let mut epochs = FocusEpochSource::new();
        focus(&mut composer, &mut epochs);
        let epoch = epochs.current();
        composer
            .activate(ComposerControl::SendNow, epoch)
            .expect("pending");
        let mut next = projection_with(composer.fence(), "retry me");
        next.fence.runtime_generation = composer.fence().runtime_generation + 1;
        composer
            .apply_projection(next, epoch)
            .expect("generation advanced");
        assert!(composer.pending_intent().is_none());
        assert!(matches!(
            composer.retry_pending(epoch).expect_err("cleared"),
            ComposerError::Unavailable { .. }
        ));
    }

    #[test]
    fn retry_revalidates_current_fence() {
        let mut composer = bind_granted("retry me");
        let mut epochs = FocusEpochSource::new();
        focus(&mut composer, &mut epochs);
        let epoch = epochs.current();
        let pending = composer
            .activate(ComposerControl::SendNow, epoch)
            .expect("pending");
        assert_eq!(
            composer.retry_pending(epoch).expect("same fence").fence,
            pending.fence
        );
    }

    #[test]
    fn steer_is_unavailable_without_a_current_turn() {
        let mut projection = projection_with(fence(), "nudge");
        projection.fence.turn_id = None;
        let composer =
            TaskComposer::bind_with_catalog(projection, FIXTURE_CATALOG).expect("granted");
        let availability = composer
            .availability(ComposerControl::Steer)
            .expect("steer");
        assert!(!availability.is_available());
        assert!(availability
            .reason()
            .is_some_and(|reason| reason.contains("no current turn")));
    }

    #[test]
    fn insert_newline_marks_dirty_only_after_accepted_mutation() {
        let mut composer = bind_granted("keep me");
        let epochs = FocusEpochSource::new();
        let epoch = epochs.current();
        composer.set_focus_epoch(epoch).expect("epoch");
        composer.insert_newline(epoch).expect("unfocused newline");
        composer
            .apply_projection(projection_with(composer.fence(), "host"), epoch)
            .expect("refresh");
        assert_eq!(composer.draft_text(), "host");
    }

    #[test]
    fn replacement_answer_request_cannot_be_targeted() {
        let original = RequestId::new();
        let replacement = RequestId::new();
        let mut projection = projection_with(fence(), "");
        projection.question = Some(QuestionProjection {
            request_id: original,
            state_revision: 1,
            options: vec!["A".into()],
        });
        let mut composer =
            TaskComposer::bind_with_catalog(projection, FIXTURE_CATALOG).expect("granted");
        let mut epochs = FocusEpochSource::new();
        focus(&mut composer, &mut epochs);
        let epoch = epochs.current();
        let mut next = projection_with(composer.fence(), "");
        next.question = Some(QuestionProjection {
            request_id: replacement,
            state_revision: 1,
            options: vec!["A".into()],
        });
        composer.apply_projection(next, epoch).expect("replaced");
        let rejected = composer
            .activate_answer(
                original,
                1,
                composer.fence(),
                AnswerPayload::Option {
                    index: 0,
                    label: "A".into(),
                },
                epoch,
            )
            .expect_err("old request");
        assert!(matches!(rejected, ComposerError::StaleRequest { .. }));
        assert!(composer.pending_intent().is_none());
    }

    #[test]
    fn activate_answer_cannot_target_replaced_request_between_render_and_release() {
        let original = RequestId::new();
        let replacement = RequestId::new();
        let mut projection = projection_with(fence(), "answer later");
        projection.question = Some(QuestionProjection {
            request_id: original,
            state_revision: 1,
            options: vec!["A".into()],
        });
        let mut composer =
            TaskComposer::bind_with_catalog(projection, FIXTURE_CATALOG).expect("granted");
        let mut epochs = FocusEpochSource::new();
        focus(&mut composer, &mut epochs);
        let epoch = epochs.current();
        composer
            .focus_control(ComposerControl::Answer, epoch)
            .expect("render-arm original question");

        let mut next = projection_with(composer.fence(), "answer later");
        next.question = Some(QuestionProjection {
            request_id: replacement,
            state_revision: 1,
            options: vec!["A".into()],
        });
        composer.apply_projection(next, epoch).expect("replaced");

        let rejected_control = composer
            .activate(ComposerControl::Answer, epoch)
            .expect_err("activate(Answer) must consume the captured request");
        assert!(matches!(
            rejected_control,
            ComposerError::StalePointer { .. } | ComposerError::StaleRequest { .. }
        ));
        assert!(composer.pending_intent().is_none());

        let rejected_api = composer
            .activate_answer(
                replacement,
                1,
                composer.fence(),
                AnswerPayload::Text("hijack".into()),
                epoch,
            )
            .expect_err("typed answer cannot use replacement ids without re-arm");
        assert!(matches!(
            rejected_api,
            ComposerError::StalePointer { .. } | ComposerError::StaleFence { .. }
        ));
        assert!(composer.pending_intent().is_none());
    }

    #[test]
    fn enter_and_typed_activate_cannot_release_replaced_generation() {
        let mut composer = bind_granted("ship this turn");
        let mut epochs = FocusEpochSource::new();
        focus(&mut composer, &mut epochs);
        let epoch = epochs.current();
        composer
            .set_enter_preference(EnterPreference::EnterSends)
            .expect("pref");
        let mut next = projection_with(composer.fence(), "ship this turn");
        next.fence.runtime_generation = composer.fence().runtime_generation + 1;
        composer
            .apply_projection(next, epoch)
            .expect("generation replaced");
        assert!(matches!(
            composer
                .handle_enter(epoch)
                .expect_err("Enter must take the armed fence, not the replacement"),
            ComposerError::StalePointer { .. } | ComposerError::StaleFence { .. }
        ));
        assert!(matches!(
            composer
                .activate(ComposerControl::SendNow, epoch)
                .expect_err("typed activate must take the armed fence"),
            ComposerError::StalePointer { .. } | ComposerError::StaleFence { .. }
        ));
        assert!(composer.pending_intent().is_none());
    }

    #[test]
    fn pointer_release_consumes_captured_question_and_rejects_second_release() {
        let question = RequestId::new();
        let mut projection = projection_with(fence(), "answer later");
        projection.question = Some(QuestionProjection {
            request_id: question,
            state_revision: 1,
            options: vec!["A".into()],
        });
        let mut composer =
            TaskComposer::bind_with_catalog(projection, FIXTURE_CATALOG).expect("granted");
        let mut epochs = FocusEpochSource::new();
        focus(&mut composer, &mut epochs);
        let epoch = epochs.current();
        composer
            .pointer_down(ComposerControl::Answer, 3, epoch)
            .expect("arm captured question");
        let first = composer
            .pointer_up(ComposerControl::Answer, 3, epoch)
            .expect("release consumes capture")
            .expect("intent");
        match first.payload {
            ComposerPayload::Answer {
                request_id,
                state_revision,
                ..
            } => assert_eq!((request_id, state_revision), (question, 1)),
            other => panic!("{other:?}"),
        }
        assert!(matches!(
            composer
                .pointer_up(ComposerControl::Answer, 3, epoch)
                .expect_err("capture was consumed"),
            ComposerError::StalePointer { .. }
        ));
        assert_eq!(
            composer.pending_intent().map(|intent| intent.command_id),
            Some(first.command_id)
        );
    }

    #[test]
    fn protocol_option_stays_exact_while_presentation_is_sanitized() {
        let question = RequestId::new();
        let raw = "\u{1b}[31mShip it";
        let mut projection = projection_with(fence(), "");
        projection.question = Some(QuestionProjection {
            request_id: question,
            state_revision: 1,
            options: vec![raw.into()],
        });
        let mut composer =
            TaskComposer::bind_with_catalog(projection, FIXTURE_CATALOG).expect("granted");
        let presented = composer.presented_question_options().expect("presentation");
        assert_eq!(presented, vec!["Ship it".to_string()]);
        let mut epochs = FocusEpochSource::new();
        focus(&mut composer, &mut epochs);
        let epoch = epochs.current();
        composer
            .focus_control(ComposerControl::Answer, epoch)
            .expect("arm captured question");
        let answered = composer
            .activate_answer(
                question,
                1,
                composer.fence(),
                AnswerPayload::Option {
                    index: 0,
                    label: raw.into(),
                },
                epoch,
            )
            .expect("exact option");
        match answered.payload {
            ComposerPayload::Answer {
                answer: AnswerPayload::Option { label, .. },
                ..
            } => assert_eq!(label, raw),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn presented_option_escape_plus_multibyte_does_not_panic_or_rewrite_protocol() {
        let question = RequestId::new();
        let raw = "\u{1b}界Ship it";
        let mut projection = projection_with(fence(), "");
        projection.question = Some(QuestionProjection {
            request_id: question,
            state_revision: 1,
            options: vec![raw.into()],
        });
        let mut composer =
            TaskComposer::bind_with_catalog(projection, FIXTURE_CATALOG).expect("granted");
        let presented = composer
            .presented_question_options()
            .expect("escape plus multibyte must not panic presentation");
        assert_eq!(presented, vec!["Ship it".to_string()]);
        let mut epochs = FocusEpochSource::new();
        focus(&mut composer, &mut epochs);
        let epoch = epochs.current();
        composer
            .focus_control(ComposerControl::Answer, epoch)
            .expect("arm captured question");
        let answered = composer
            .activate_answer(
                question,
                1,
                composer.fence(),
                AnswerPayload::Option {
                    index: 0,
                    label: raw.into(),
                },
                epoch,
            )
            .expect("protocol option stays exact");
        match answered.payload {
            ComposerPayload::Answer {
                answer: AnswerPayload::Option { label, .. },
                ..
            } => assert_eq!(label, raw),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn too_many_question_options_are_rejected_before_mapping() {
        let mut projection = projection_with(fence(), "");
        projection.question = Some(QuestionProjection {
            request_id: RequestId::new(),
            state_revision: 1,
            options: (0..=MAX_QUESTION_OPTIONS)
                .map(|index| format!("opt-{index}"))
                .collect(),
        });
        assert!(matches!(
            TaskComposer::bind(projection).expect_err("capped"),
            ComposerError::TextBoundExceeded { .. }
        ));
    }

    #[test]
    fn one_focus_owner_and_busy_accessibility() {
        let mut composer = bind_granted("busy");
        let mut epochs = FocusEpochSource::new();
        focus(&mut composer, &mut epochs);
        let epoch = epochs.current();
        assert!(composer.input_accessibility().focused());
        assert!(!composer
            .control_accessibility(ComposerControl::SendNow)
            .expect("send")
            .focused());
        composer
            .focus_control(ComposerControl::SendNow, epoch)
            .expect("control focus");
        assert!(!composer.input_accessibility().focused());
        let send = composer
            .control_accessibility(ComposerControl::SendNow)
            .expect("send");
        assert!(send.focused());
        assert!(!send.busy());
        composer
            .activate(ComposerControl::SendNow, epoch)
            .expect("pending");
        let busy = composer
            .control_accessibility(ComposerControl::SendNow)
            .expect("busy");
        assert!(busy.busy());
        assert!(composer.input_accessibility().busy());
    }

    #[test]
    fn pending_send_maps_to_provider_input_and_mints_a_stable_first_turn() {
        let mut composer = bind_granted("ship it");
        let mut epochs = FocusEpochSource::new();
        focus(&mut composer, &mut epochs);
        composer
            .activate(ComposerControl::SendNow, epochs.current())
            .expect("pending send");
        let intent = composer.pending_intent().expect("intent").clone();
        let turn = DomainTurnId::new();
        let request = intent
            .to_provider_input_request(Some(turn), None, None)
            .expect("typed provider input");
        match request {
            ActionRequest::ProviderInput(inner) => {
                assert_eq!(inner.action_id, EXPECTED_ACTION_SEND_NOW);
                assert_eq!(inner.arguments.text.as_deref(), Some("ship it"));
                assert_eq!(inner.arguments.turn_id, turn);
                assert_eq!(inner.arguments.task_id, intent.fence.task_id);
            }
            other => panic!("expected ProviderInput, got {other:?}"),
        }
        let expected_first_turn =
            DomainTurnId::from_bytes(*intent.command_id.as_bytes()).expect("command-backed turn");
        for _ in 0..2 {
            let request = intent
                .to_provider_input_request(None, None, None)
                .expect("Send Now must establish the first provider turn");
            let ActionRequest::ProviderInput(inner) = request else {
                panic!("expected ProviderInput");
            };
            assert_eq!(inner.arguments.turn_id, expected_first_turn);
        }

        let mut exact_resume = intent.clone();
        exact_resume.action_id = crate::client::action::ACTION_PROVIDER_NEW_CONVERSATION;
        assert!(matches!(
            exact_resume.to_provider_input_request(Some(turn), None, None),
            Err(ComposerError::Unavailable {
                control: ComposerControl::SendNow,
                ..
            })
        ));
    }

    #[test]
    fn task_binding_preserves_task_request_and_action_epochs() {
        let task_id = TaskId::new();
        let agent_session_id = AgentSessionId::new();
        let question_id = RequestId::new();
        let approval_id = RequestId::new();
        let fence = ComposerFence {
            task_id,
            agent_session_id,
            runtime_generation: 17,
            action_epoch: 23,
            turn_id: None,
        };
        let projection = projection_for_task(
            fence,
            vec![ArtifactId::new()],
            Some((question_id, 41)),
            Some((approval_id, 43)),
        );

        let focus_epoch = FocusEpochSource::new().current();
        let composer =
            TaskComposer::bind_for_task(projection, focus_epoch).expect("task composer binds");

        assert_eq!(composer.fence(), fence);
        assert_eq!(
            composer.pending_question_identity(),
            Some((question_id, 41))
        );
        assert_eq!(
            composer.pending_approval_identity(),
            Some((approval_id, 43))
        );
        assert_eq!(composer.focus_epoch(), Some(focus_epoch));
    }

    #[test]
    fn task_binding_does_not_infer_a_turn_id() {
        let fence = ComposerFence {
            task_id: TaskId::new(),
            agent_session_id: AgentSessionId::new(),
            runtime_generation: 5,
            action_epoch: 6,
            turn_id: None,
        };
        let composer = TaskComposer::bind_for_task(
            projection_for_task(fence, Vec::new(), None, None),
            FocusEpochSource::new().current(),
        )
        .expect("task composer binds");

        assert_eq!(composer.fence().turn_id, None);
    }

    #[test]
    fn ai_acceptance_slash_catalog_is_complete_and_provider_aware() {
        let claude = provider_command_catalog(&crate::providers::ProviderKind::ClaudeCode);
        let codex = provider_command_catalog(&crate::providers::ProviderKind::Codex);

        assert!(claude.len() >= 90, "Claude catalog unexpectedly incomplete");
        assert!(codex.len() >= 45, "Codex catalog unexpectedly incomplete");
        assert!(claude.iter().any(|command| command.command == "/agents"));
        assert!(codex.iter().any(|command| command.command == "/agent"));
        assert!(provider_command_opens_terminal(
            &crate::providers::ProviderKind::ClaudeCode,
            "/help"
        ));
        assert!(provider_command_opens_terminal(
            &crate::providers::ProviderKind::Codex,
            "/subagents"
        ));
        assert!(!provider_command_opens_terminal(
            &crate::providers::ProviderKind::ClaudeCode,
            "/compact focus on tests"
        ));
    }
}
