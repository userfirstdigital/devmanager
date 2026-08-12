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
use crate::client::action::{ActionDescriptor, catalog};
use crate::domain::id::{PromptChainLinkId, PromptVersionId};
use crate::domain::{AgentSessionId, ArtifactId, CommandId, RequestId, TaskId};
use crate::prompts::model::PromptVersion;
use crate::ui::components::interaction::{
    AccessibilityMetadata, AccessibleRole, ComponentError, FocusEpoch, InteractionStateModel,
    MAX_ACCESSIBLE_DESCRIPTION_SCALARS, MAX_ACCESSIBLE_NAME_SCALARS, redacted_bounded_text,
};
use crate::ui::components::text_field::{TextField, TextFieldError, TextFieldKey, TextFieldLimits};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fmt::{Debug, Display, Formatter};

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderCommandSuggestion {
    pub label: String,
    pub command: String,
    pub provider_kind: String,
}

pub fn suggest_provider_commands<'a>(
    prefix: &str,
    catalog: &'a [ProviderCommandSuggestion],
) -> Vec<&'a ProviderCommandSuggestion> {
    let needle = prefix.trim();
    catalog
        .iter()
        .filter(|suggestion| needle.is_empty() || suggestion.command.starts_with(needle))
        .collect()
}

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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
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

#[derive(Clone, Debug, Eq, PartialEq)]
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
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ComposerAttachmentProjection {
    pub artifact_id: ArtifactId,
    pub kind: AttachmentKind,
    pub label: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
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

    pub fn text_limits(&self) -> TextFieldLimits {
        self.field.limits()
    }

    pub fn search_query_limit(&self) -> usize {
        MAX_SEARCH_QUERY_SCALARS
    }

    pub fn draft_text(&self) -> &str {
        self.field.value()
    }

    pub fn attachments(&self) -> &[ComposerAttachmentProjection] {
        &self.attachments
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
        self.cancel_pending(command_id)
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
        assert!(
            composer
                .availability(ComposerControl::SendNow)
                .expect("availability")
                .is_available()
        );
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
        assert!(
            composer
                .availability(ComposerControl::StageAttachment)
                .expect("stage")
                .is_available()
        );
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
        assert!(
            !composer
                .availability(ComposerControl::SendNow)
                .expect("disabled at arm")
                .is_available()
        );
        assert!(matches!(
            composer
                .pointer_down(ComposerControl::SendNow, 4, epoch)
                .expect_err("disabled press is not armed"),
            ComposerError::StalePointer { .. }
        ));

        composer
            .apply_projection(projection_with(composer.fence(), "do not fire"), epoch)
            .expect("same-fence enable refresh");
        assert!(
            composer
                .availability(ComposerControl::SendNow)
                .expect("now enabled")
                .is_available()
        );
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
        assert!(
            availability
                .reason()
                .is_some_and(|reason| reason.contains("no current turn"))
        );
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
        assert!(
            !composer
                .control_accessibility(ComposerControl::SendNow)
                .expect("send")
                .focused()
        );
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
}
