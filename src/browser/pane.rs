use super::{
    model::browser_annotation_urls_equivalent, save_recipe, validate_browser_url,
    BrowserAnnotation, BrowserAnnotationCandidate, BrowserAnnotationDraft, BrowserAnnotationKind,
    BrowserApprovalRequest, BrowserBounds, BrowserCommand, BrowserDownloadState, BrowserElementRef,
    BrowserError, BrowserHostEvent, BrowserJournalEntry, BrowserPageLoadState, BrowserRecipeAction,
    BrowserRecipeAssertion, BrowserRecipeElementState, BrowserRecipeInput, BrowserRecipeInputKind,
    BrowserRecipeV1, BrowserRecipeValue, BrowserRecipeViewport, BrowserRecipeWait,
    BrowserRecordingActor, BrowserRecordingError, BrowserRecordingMetadata, BrowserRecordingReview,
    BrowserRecordingStatus, BrowserReplayInstance, BrowserReplayProjection,
    BrowserReplayRepairCandidate, BrowserReplayRepairPhase, BrowserReplayRepairProjection,
    BrowserReplaySecretError, BrowserReplaySecretSubmission, BrowserResponse, BrowserRevision,
    BrowserTabSnapshot, BrowserViewport, BrowserWorkflowCoordinator, BrowserWorkspaceKey,
    BrowserWorkspaceSnapshot, MAX_BROWSER_RECORDING_INPUTS, MAX_BROWSER_REPLAY_SECRET_INPUTS,
    MAX_BROWSER_REPLAY_SECRET_INPUT_NAME_BYTES, MAX_BROWSER_REPLAY_SECRET_VALUE_BYTES,
};
use std::collections::HashSet;
use std::fmt;
use std::net::{Ipv4Addr, Ipv6Addr};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use zeroize::Zeroizing;

use crate::theme;
use gpui::{
    canvas, div, prelude::*, px, rgb, App, Bounds, FocusHandle, IntoElement, KeyDownEvent,
    MouseButton, MouseDownEvent, ParentElement, Pixels, SharedString, StatefulInteractiveElement,
    Styled, Window,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BrowserPaneSurface {
    Server,
    Claude,
    Codex,
    Ssh,
}

pub const BROWSER_REPLAY_SECRET_MASK: &str = "••••••••";

#[derive(Debug, Clone, Copy, serde::Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum BrowserReplaySecretPromptOperation {
    Installed,
    Edited,
    Backspaced,
    Focused,
    Submitted,
    Cancelled,
    RouteSwitched,
    ReplayReplaced,
}

#[derive(Debug, Clone, serde::Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct BrowserReplaySecretPromptEvent {
    pub workspace_key: BrowserWorkspaceKey,
    pub instance_id: u64,
    pub operation: BrowserReplaySecretPromptOperation,
    pub input_name: Option<String>,
    pub focused_input: Option<String>,
    pub is_set: Option<bool>,
}

#[derive(Debug, Clone, serde::Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct BrowserReplaySecretPromptProjection {
    pub workspace_key: BrowserWorkspaceKey,
    pub instance_id: u64,
    pub input_names: Vec<String>,
    pub focused_input: Option<String>,
    pub is_set: Vec<bool>,
}

impl BrowserReplaySecretPromptProjection {
    pub fn mask_for(&self, input_name: &str) -> Option<&'static str> {
        self.input_names
            .iter()
            .position(|name| name == input_name)
            .and_then(|index| self.is_set.get(index).copied())
            .map(browser_replay_secret_mask)
    }
}

pub fn browser_replay_secret_mask(is_set: bool) -> &'static str {
    if is_set {
        BROWSER_REPLAY_SECRET_MASK
    } else {
        ""
    }
}

struct BrowserReplaySecretPromptValue {
    name: String,
    value: Zeroizing<String>,
}

impl BrowserReplaySecretPromptValue {
    fn new(name: String) -> Self {
        Self {
            name,
            value: Zeroizing::new(String::with_capacity(MAX_BROWSER_REPLAY_SECRET_VALUE_BYTES)),
        }
    }

    fn append(&mut self, text: &str) -> Result<(), BrowserReplaySecretError> {
        let next_len = self
            .value
            .len()
            .checked_add(text.len())
            .ok_or(BrowserReplaySecretError::InvalidSubmission)?;
        if next_len > MAX_BROWSER_REPLAY_SECRET_VALUE_BYTES || next_len > self.value.capacity() {
            return Err(BrowserReplaySecretError::InvalidSubmission);
        }
        let pointer = self.value.as_ptr();
        let capacity = self.value.capacity();
        self.value.push_str(text);
        debug_assert_eq!(self.value.as_ptr(), pointer);
        debug_assert_eq!(self.value.capacity(), capacity);
        Ok(())
    }
}

pub struct BrowserReplaySecretPromptVault {
    instance: BrowserReplayInstance,
    values: Vec<BrowserReplaySecretPromptValue>,
    focused_index: usize,
}

impl BrowserReplaySecretPromptVault {
    pub fn install(
        instance: BrowserReplayInstance,
        input_names: Vec<String>,
    ) -> Result<(Self, BrowserReplaySecretPromptEvent), BrowserReplaySecretError> {
        if !valid_prompt_input_names(&input_names) {
            return Err(BrowserReplaySecretError::InvalidSubmission);
        }
        let values = input_names
            .into_iter()
            .map(BrowserReplaySecretPromptValue::new)
            .collect();
        let vault = Self {
            instance,
            values,
            focused_index: 0,
        };
        let event = vault.event(BrowserReplaySecretPromptOperation::Installed, None, None);
        Ok((vault, event))
    }

    pub fn projection(&self) -> BrowserReplaySecretPromptProjection {
        BrowserReplaySecretPromptProjection {
            workspace_key: self.instance.workspace_key().clone(),
            instance_id: self.instance.id(),
            input_names: self.values.iter().map(|entry| entry.name.clone()).collect(),
            focused_input: self
                .values
                .get(self.focused_index)
                .map(|entry| entry.name.clone()),
            is_set: self
                .values
                .iter()
                .map(|entry| !entry.value.is_empty())
                .collect(),
        }
    }

    pub fn edit(
        &mut self,
        instance: &BrowserReplayInstance,
        input_name: &str,
        text: &str,
    ) -> Result<BrowserReplaySecretPromptEvent, BrowserReplaySecretError> {
        self.ensure_exact(instance)?;
        if text.is_empty() || text.chars().any(char::is_control) {
            return Err(BrowserReplaySecretError::InvalidSubmission);
        }
        let index = self
            .input_index(input_name)
            .ok_or(BrowserReplaySecretError::InvalidSubmission)?;
        let entry = &mut self.values[index];
        entry.append(text)?;
        self.focused_index = index;
        Ok(self.event(
            BrowserReplaySecretPromptOperation::Edited,
            Some(input_name),
            Some(true),
        ))
    }

    pub fn backspace(
        &mut self,
        instance: &BrowserReplayInstance,
        input_name: &str,
    ) -> Result<BrowserReplaySecretPromptEvent, BrowserReplaySecretError> {
        self.ensure_exact(instance)?;
        let index = self
            .input_index(input_name)
            .ok_or(BrowserReplaySecretError::InvalidSubmission)?;
        let entry = &mut self.values[index];
        entry.value.pop();
        let is_set = !entry.value.is_empty();
        self.focused_index = index;
        Ok(self.event(
            BrowserReplaySecretPromptOperation::Backspaced,
            Some(input_name),
            Some(is_set),
        ))
    }

    pub fn focus(
        &mut self,
        instance: &BrowserReplayInstance,
        input_name: &str,
    ) -> Result<BrowserReplaySecretPromptEvent, BrowserReplaySecretError> {
        self.ensure_exact(instance)?;
        let index = self
            .input_index(input_name)
            .ok_or(BrowserReplaySecretError::InvalidSubmission)?;
        self.focused_index = index;
        Ok(self.event(
            BrowserReplaySecretPromptOperation::Focused,
            Some(input_name),
            Some(!self.values[index].value.is_empty()),
        ))
    }

    pub fn submit(
        mut self,
        instance: &BrowserReplayInstance,
    ) -> Result<
        (
            BrowserReplaySecretSubmission,
            BrowserReplaySecretPromptEvent,
        ),
        BrowserReplaySecretError,
    > {
        self.ensure_exact(instance)?;
        if self.values.iter().any(|entry| entry.value.is_empty()) {
            return Err(BrowserReplaySecretError::InvalidSubmission);
        }
        let event = self.event(BrowserReplaySecretPromptOperation::Submitted, None, None);
        let values = self
            .values
            .iter_mut()
            .map(|entry| (entry.name.clone(), std::mem::take(&mut *entry.value)))
            .collect();
        Ok((
            BrowserReplaySecretSubmission::from_user_prompt(values),
            event,
        ))
    }

    pub fn cancel(
        self,
        instance: &BrowserReplayInstance,
    ) -> Result<BrowserReplaySecretPromptEvent, BrowserReplaySecretError> {
        self.consume_event(instance, BrowserReplaySecretPromptOperation::Cancelled)
    }

    pub fn route_switch(
        self,
        instance: &BrowserReplayInstance,
    ) -> Result<BrowserReplaySecretPromptEvent, BrowserReplaySecretError> {
        self.consume_event(instance, BrowserReplaySecretPromptOperation::RouteSwitched)
    }

    pub fn replay_replaced(
        self,
        instance: &BrowserReplayInstance,
    ) -> Result<BrowserReplaySecretPromptEvent, BrowserReplaySecretError> {
        self.consume_event(instance, BrowserReplaySecretPromptOperation::ReplayReplaced)
    }

    pub fn same_instance(&self, instance: &BrowserReplayInstance) -> bool {
        self.instance == *instance
    }

    pub fn workspace_key(&self) -> &BrowserWorkspaceKey {
        self.instance.workspace_key()
    }

    pub(crate) fn instance(&self) -> &BrowserReplayInstance {
        &self.instance
    }

    fn consume_event(
        self,
        instance: &BrowserReplayInstance,
        operation: BrowserReplaySecretPromptOperation,
    ) -> Result<BrowserReplaySecretPromptEvent, BrowserReplaySecretError> {
        self.ensure_exact(instance)?;
        Ok(self.event(operation, None, None))
    }

    fn ensure_exact(
        &self,
        instance: &BrowserReplayInstance,
    ) -> Result<(), BrowserReplaySecretError> {
        if self.instance != *instance {
            return Err(BrowserReplaySecretError::StaleAuthority);
        }
        Ok(())
    }

    fn input_index(&self, input_name: &str) -> Option<usize> {
        self.values
            .iter()
            .position(|entry| entry.name == input_name)
    }

    fn event(
        &self,
        operation: BrowserReplaySecretPromptOperation,
        input_name: Option<&str>,
        is_set: Option<bool>,
    ) -> BrowserReplaySecretPromptEvent {
        BrowserReplaySecretPromptEvent {
            workspace_key: self.instance.workspace_key().clone(),
            instance_id: self.instance.id(),
            operation,
            input_name: input_name.map(str::to_string),
            focused_input: self
                .values
                .get(self.focused_index)
                .map(|entry| entry.name.clone()),
            is_set,
        }
    }
}

fn valid_prompt_input_names(input_names: &[String]) -> bool {
    if input_names.is_empty() || input_names.len() > MAX_BROWSER_REPLAY_SECRET_INPUTS {
        return false;
    }
    let mut names = HashSet::with_capacity(input_names.len());
    input_names.iter().all(|name| {
        !name.is_empty()
            && name.len() <= MAX_BROWSER_REPLAY_SECRET_INPUT_NAME_BYTES
            && name.trim() == name
            && !name.chars().any(char::is_control)
            && !super::automation::browser_text_contains_secret(name)
            && names.insert(name.as_str())
    })
}

#[cfg(test)]
mod replay_secret_prompt_memory_tests {
    use super::{BrowserReplaySecretPromptValue, MAX_BROWSER_REPLAY_SECRET_VALUE_BYTES};

    #[test]
    fn secret_prompt_value_preallocates_once_and_never_moves_while_filling_to_limit() {
        let mut value = BrowserReplaySecretPromptValue::new("credential".to_string());
        let initial_pointer = value.value.as_ptr();
        let initial_capacity = value.value.capacity();
        assert!(initial_capacity >= MAX_BROWSER_REPLAY_SECRET_VALUE_BYTES);

        while value.value.len() < MAX_BROWSER_REPLAY_SECRET_VALUE_BYTES {
            let remaining = MAX_BROWSER_REPLAY_SECRET_VALUE_BYTES - value.value.len();
            let chunk = "x".repeat(remaining.min(257));
            value.append(&chunk).expect("bounded append");
            assert_eq!(value.value.as_ptr(), initial_pointer);
            assert_eq!(value.value.capacity(), initial_capacity);
        }

        assert_eq!(value.value.len(), MAX_BROWSER_REPLAY_SECRET_VALUE_BYTES);
        assert!(value.append("x").is_err());
        assert_eq!(value.value.as_ptr(), initial_pointer);
        assert_eq!(value.value.capacity(), initial_capacity);
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BrowserWorkflowReviewUiState {
    Inactive,
    Recording { instance_id: u64 },
    Review { instance_id: u64 },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrowserWorkflowReviewStepProjection {
    pub id: String,
    pub index: usize,
    pub actor: BrowserRecordingActor,
    pub summary: String,
    pub convertible_kind: Option<BrowserRecipeInputKind>,
    pub has_wait: bool,
    pub assertion_count: usize,
    pub has_assertion_locator: bool,
    pub can_move_up: bool,
    pub can_move_down: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrowserWorkflowReviewInputProjection {
    pub name: String,
    pub kind: BrowserRecipeInputKind,
    pub unset: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrowserWorkflowReviewMetadataProjection {
    pub id: String,
    pub name: String,
    pub description: String,
    pub start_url: String,
    pub viewport: BrowserRecipeViewport,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BrowserWorkflowReviewEditorField {
    Id,
    Name,
    Description,
    StartUrl,
    InputName {
        input_name: String,
    },
    InputDefault {
        input_name: String,
    },
    Assertion {
        step_id: String,
        kind: BrowserWorkflowReviewAssertionKind,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BrowserWorkflowReviewAssertionKind {
    Url,
    Title,
    Text,
    Element,
    Value,
}

#[derive(Clone, PartialEq, Eq)]
pub struct BrowserWorkflowReviewEditor {
    pub instance_id: u64,
    pub field: BrowserWorkflowReviewEditorField,
    pub draft: String,
    pub cursor: usize,
    pub focused: bool,
}

impl fmt::Debug for BrowserWorkflowReviewEditor {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("BrowserWorkflowReviewEditor")
            .field("instance_id", &self.instance_id)
            .field("field", &self.field)
            .field("cursor", &self.cursor)
            .field("focused", &self.focused)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrowserWorkflowReviewProjection {
    pub workspace_key: BrowserWorkspaceKey,
    pub state: BrowserWorkflowReviewUiState,
    pub metadata: Option<BrowserWorkflowReviewMetadataProjection>,
    pub steps: Vec<BrowserWorkflowReviewStepProjection>,
    pub inputs: Vec<BrowserWorkflowReviewInputProjection>,
}

#[derive(Clone, PartialEq, Eq)]
pub enum BrowserWorkflowReviewMutation {
    SetMetadata {
        id: String,
        name: String,
        description: String,
        start_url: String,
        viewport: BrowserRecipeViewport,
    },
    DeleteStep {
        step_id: String,
    },
    MoveStep {
        step_id: String,
        new_index: usize,
    },
    ConvertActionValueToInput {
        step_id: String,
        input_name: String,
        kind: BrowserRecipeInputKind,
    },
    AddInput {
        input: BrowserRecipeInput,
    },
    RenameInput {
        previous_name: String,
        new_name: String,
    },
    SetInputDefault {
        input_name: String,
        default_value: Option<String>,
    },
    RemoveInput {
        input_name: String,
    },
    SetStepWait {
        step_id: String,
        wait: Option<BrowserRecipeWait>,
    },
    AddStepAssertion {
        step_id: String,
        assertion: BrowserRecipeAssertion,
    },
    AddStepAssertionDraft {
        step_id: String,
        kind: BrowserWorkflowReviewAssertionKind,
        expected: Option<String>,
    },
    RemoveStepAssertion {
        step_id: String,
        assertion_index: usize,
    },
}

impl fmt::Debug for BrowserWorkflowReviewMutation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::SetMetadata { .. } => "SetMetadata",
            Self::DeleteStep { .. } => "DeleteStep",
            Self::MoveStep { .. } => "MoveStep",
            Self::ConvertActionValueToInput { .. } => "ConvertActionValueToInput",
            Self::AddInput { .. } => "AddInput",
            Self::RenameInput { .. } => "RenameInput",
            Self::SetInputDefault { .. } => "SetInputDefault",
            Self::RemoveInput { .. } => "RemoveInput",
            Self::SetStepWait { .. } => "SetStepWait",
            Self::AddStepAssertion { .. } => "AddStepAssertion",
            Self::AddStepAssertionDraft { .. } => "AddStepAssertionDraft",
            Self::RemoveStepAssertion { .. } => "RemoveStepAssertion",
        })
    }
}

pub fn browser_workflow_review_editor_for_field(
    projection: &BrowserWorkflowReviewProjection,
    instance_id: u64,
    field: BrowserWorkflowReviewEditorField,
) -> Result<BrowserWorkflowReviewEditor, BrowserRecordingError> {
    if !matches!(
        projection.state,
        BrowserWorkflowReviewUiState::Review {
            instance_id: active_instance_id
        } if active_instance_id == instance_id
    ) {
        return Err(BrowserRecordingError::StaleInstance);
    }

    let draft = match &field {
        BrowserWorkflowReviewEditorField::Id => projection
            .metadata
            .as_ref()
            .ok_or(BrowserRecordingError::InvalidMutation)?
            .id
            .clone(),
        BrowserWorkflowReviewEditorField::Name => projection
            .metadata
            .as_ref()
            .ok_or(BrowserRecordingError::InvalidMutation)?
            .name
            .clone(),
        BrowserWorkflowReviewEditorField::Description => projection
            .metadata
            .as_ref()
            .ok_or(BrowserRecordingError::InvalidMutation)?
            .description
            .clone(),
        BrowserWorkflowReviewEditorField::StartUrl => projection
            .metadata
            .as_ref()
            .ok_or(BrowserRecordingError::InvalidMutation)?
            .start_url
            .clone(),
        BrowserWorkflowReviewEditorField::InputName { input_name } => projection
            .inputs
            .iter()
            .find(|input| input.name == *input_name)
            .map(|input| input.name.clone())
            .ok_or(BrowserRecordingError::InvalidMutation)?,
        BrowserWorkflowReviewEditorField::InputDefault { input_name } => {
            let input = projection
                .inputs
                .iter()
                .find(|input| input.name == *input_name)
                .ok_or(BrowserRecordingError::InvalidMutation)?;
            if !matches!(
                input.kind,
                BrowserRecipeInputKind::Text | BrowserRecipeInputKind::Url
            ) {
                return Err(BrowserRecordingError::InvalidMutation);
            }
            String::new()
        }
        BrowserWorkflowReviewEditorField::Assertion { step_id, kind } => {
            if !projection.steps.iter().any(|step| {
                step.id == *step_id
                    && (!matches!(
                        kind,
                        BrowserWorkflowReviewAssertionKind::Element
                            | BrowserWorkflowReviewAssertionKind::Value
                    ) || step.has_assertion_locator)
            }) {
                return Err(BrowserRecordingError::InvalidMutation);
            }
            String::new()
        }
    };
    let cursor = draft.chars().count();
    Ok(BrowserWorkflowReviewEditor {
        instance_id,
        field,
        draft,
        cursor,
        focused: true,
    })
}

pub fn browser_workflow_review_editor_mutation(
    projection: &BrowserWorkflowReviewProjection,
    editor: &BrowserWorkflowReviewEditor,
) -> Result<BrowserWorkflowReviewMutation, BrowserRecordingError> {
    if !matches!(
        projection.state,
        BrowserWorkflowReviewUiState::Review {
            instance_id: active_instance_id
        } if active_instance_id == editor.instance_id
    ) {
        return Err(BrowserRecordingError::StaleInstance);
    }

    let metadata = projection
        .metadata
        .as_ref()
        .ok_or(BrowserRecordingError::InvalidMutation)?;
    Ok(match &editor.field {
        BrowserWorkflowReviewEditorField::Id => BrowserWorkflowReviewMutation::SetMetadata {
            id: editor.draft.clone(),
            name: metadata.name.clone(),
            description: metadata.description.clone(),
            start_url: metadata.start_url.clone(),
            viewport: metadata.viewport.clone(),
        },
        BrowserWorkflowReviewEditorField::Name => BrowserWorkflowReviewMutation::SetMetadata {
            id: metadata.id.clone(),
            name: editor.draft.clone(),
            description: metadata.description.clone(),
            start_url: metadata.start_url.clone(),
            viewport: metadata.viewport.clone(),
        },
        BrowserWorkflowReviewEditorField::Description => {
            BrowserWorkflowReviewMutation::SetMetadata {
                id: metadata.id.clone(),
                name: metadata.name.clone(),
                description: editor.draft.clone(),
                start_url: metadata.start_url.clone(),
                viewport: metadata.viewport.clone(),
            }
        }
        BrowserWorkflowReviewEditorField::StartUrl => BrowserWorkflowReviewMutation::SetMetadata {
            id: metadata.id.clone(),
            name: metadata.name.clone(),
            description: metadata.description.clone(),
            start_url: editor.draft.clone(),
            viewport: metadata.viewport.clone(),
        },
        BrowserWorkflowReviewEditorField::InputName { input_name } => {
            if !projection
                .inputs
                .iter()
                .any(|input| input.name == *input_name)
            {
                return Err(BrowserRecordingError::InvalidMutation);
            }
            BrowserWorkflowReviewMutation::RenameInput {
                previous_name: input_name.clone(),
                new_name: editor.draft.clone(),
            }
        }
        BrowserWorkflowReviewEditorField::InputDefault { input_name } => {
            let input = projection
                .inputs
                .iter()
                .find(|input| input.name == *input_name)
                .ok_or(BrowserRecordingError::InvalidMutation)?;
            if !matches!(
                input.kind,
                BrowserRecipeInputKind::Text | BrowserRecipeInputKind::Url
            ) {
                return Err(BrowserRecordingError::InvalidMutation);
            }
            BrowserWorkflowReviewMutation::SetInputDefault {
                input_name: input_name.clone(),
                default_value: (!editor.draft.trim().is_empty()).then(|| editor.draft.clone()),
            }
        }
        BrowserWorkflowReviewEditorField::Assertion { step_id, kind } => {
            if !projection.steps.iter().any(|step| {
                step.id == *step_id
                    && (!matches!(
                        kind,
                        BrowserWorkflowReviewAssertionKind::Element
                            | BrowserWorkflowReviewAssertionKind::Value
                    ) || step.has_assertion_locator)
            }) || editor.draft.trim().is_empty()
                || matches!(kind, BrowserWorkflowReviewAssertionKind::Element)
            {
                return Err(BrowserRecordingError::InvalidMutation);
            }
            BrowserWorkflowReviewMutation::AddStepAssertionDraft {
                step_id: step_id.clone(),
                kind: *kind,
                expected: Some(editor.draft.clone()),
            }
        }
    })
}

fn browser_workflow_review_assertion(
    review: &BrowserRecordingReview,
    step_id: &str,
    kind: BrowserWorkflowReviewAssertionKind,
    expected: Option<&str>,
) -> Result<BrowserRecipeAssertion, BrowserRecordingError> {
    let literal = || {
        let expected = expected.ok_or(BrowserRecordingError::InvalidMutation)?;
        if expected.trim().is_empty() {
            return Err(BrowserRecordingError::InvalidMutation);
        }
        Ok(BrowserRecipeValue::Literal {
            value: expected.to_string(),
        })
    };
    Ok(match kind {
        BrowserWorkflowReviewAssertionKind::Url => BrowserRecipeAssertion::Url {
            value: literal()?,
            exact: true,
        },
        BrowserWorkflowReviewAssertionKind::Title => BrowserRecipeAssertion::Title {
            value: literal()?,
            exact: false,
        },
        BrowserWorkflowReviewAssertionKind::Text => BrowserRecipeAssertion::Text {
            value: literal()?,
            present: true,
        },
        BrowserWorkflowReviewAssertionKind::Element => {
            if expected.is_some() {
                return Err(BrowserRecordingError::InvalidMutation);
            }
            BrowserRecipeAssertion::Element {
                locator: review
                    .primary_locator_for_step(step_id)
                    .ok_or(BrowserRecordingError::InvalidMutation)?,
                state: BrowserRecipeElementState::Visible,
            }
        }
        BrowserWorkflowReviewAssertionKind::Value => BrowserRecipeAssertion::Value {
            locator: review
                .primary_locator_for_step(step_id)
                .ok_or(BrowserRecordingError::InvalidMutation)?,
            value: literal()?,
        },
    })
}

pub fn apply_browser_workflow_review_mutation(
    coordinator: &BrowserWorkflowCoordinator,
    active_workspace: Option<&BrowserWorkspaceKey>,
    action_workspace: &BrowserWorkspaceKey,
    surface: BrowserPaneSurface,
    instance_id: u64,
    mutation: BrowserWorkflowReviewMutation,
) -> Result<BrowserWorkflowReviewProjection, BrowserRecordingError> {
    if active_workspace != Some(action_workspace)
        || !matches!(
            surface,
            BrowserPaneSurface::Claude | BrowserPaneSurface::Codex
        )
    {
        return Err(BrowserRecordingError::InvalidMutation);
    }

    coordinator.with_recorder(|recorder| {
        let review = recorder
            .review_for_workspace(action_workspace)
            .ok_or(BrowserRecordingError::StaleInstance)?;
        if review.instance().id() != instance_id {
            return Err(BrowserRecordingError::StaleInstance);
        }
        let instance = review.instance().clone();
        match mutation {
            BrowserWorkflowReviewMutation::SetMetadata {
                id,
                name,
                description,
                start_url,
                viewport,
            } => recorder.set_metadata(
                &instance,
                BrowserRecordingMetadata {
                    id,
                    name,
                    description,
                    start_url,
                    viewport,
                },
            ),
            BrowserWorkflowReviewMutation::DeleteStep { step_id } => {
                recorder.delete_step(&instance, &step_id)
            }
            BrowserWorkflowReviewMutation::MoveStep { step_id, new_index } => {
                recorder.move_step(&instance, &step_id, new_index)
            }
            BrowserWorkflowReviewMutation::ConvertActionValueToInput {
                step_id,
                input_name,
                kind,
            } => recorder.convert_action_value_to_input(&instance, &step_id, &input_name, kind),
            BrowserWorkflowReviewMutation::AddInput { input } => {
                recorder.add_input(&instance, input)
            }
            BrowserWorkflowReviewMutation::RenameInput {
                previous_name,
                new_name,
            } => recorder.rename_input(&instance, &previous_name, &new_name),
            BrowserWorkflowReviewMutation::SetInputDefault {
                input_name,
                default_value,
            } => recorder.set_input_default(&instance, &input_name, default_value),
            BrowserWorkflowReviewMutation::RemoveInput { input_name } => {
                recorder.remove_input(&instance, &input_name)
            }
            BrowserWorkflowReviewMutation::SetStepWait { step_id, wait } => {
                recorder.set_step_wait(&instance, &step_id, wait)
            }
            BrowserWorkflowReviewMutation::AddStepAssertion { step_id, assertion } => {
                recorder.add_step_assertion(&instance, &step_id, assertion)
            }
            BrowserWorkflowReviewMutation::AddStepAssertionDraft {
                step_id,
                kind,
                expected,
            } => {
                let assertion = browser_workflow_review_assertion(
                    &review,
                    &step_id,
                    kind,
                    expected.as_deref(),
                )?;
                recorder.add_step_assertion(&instance, &step_id, assertion)
            }
            BrowserWorkflowReviewMutation::RemoveStepAssertion {
                step_id,
                assertion_index,
            } => recorder.remove_step_assertion(&instance, &step_id, assertion_index),
        }
        .map(|_| ())
    })?;

    browser_workflow_review_projection(coordinator, action_workspace, surface)
        .ok_or(BrowserRecordingError::InvalidMutation)
}

pub fn preview_browser_workflow_review(
    coordinator: &BrowserWorkflowCoordinator,
    active_workspace: Option<&BrowserWorkspaceKey>,
    action_workspace: &BrowserWorkspaceKey,
    surface: BrowserPaneSurface,
    instance_id: u64,
) -> Result<BrowserRecipeV1, BrowserError> {
    validate_workflow_review_route(active_workspace, action_workspace, surface)?;
    coordinator.with_recorder(|recorder| {
        let review = recorder
            .review_for_workspace(action_workspace)
            .ok_or_else(stale_workflow_review)?;
        if review.instance().id() != instance_id {
            return Err(stale_workflow_review());
        }
        recorder.recipe_for_save(review.instance())
    })
}

pub fn save_browser_workflow_review(
    coordinator: &BrowserWorkflowCoordinator,
    active_workspace: Option<&BrowserWorkspaceKey>,
    action_workspace: &BrowserWorkspaceKey,
    surface: BrowserPaneSurface,
    instance_id: u64,
    project_root: impl AsRef<Path>,
    remote_client: bool,
) -> Result<PathBuf, BrowserError> {
    validate_workflow_review_route(active_workspace, action_workspace, surface)?;
    if remote_client {
        return Err(BrowserError::InvalidInvocation {
            field: "localProjectRoot".to_string(),
        });
    }
    let project_root = project_root.as_ref();
    coordinator.with_recorder(|recorder| {
        let review = recorder
            .review_for_workspace(action_workspace)
            .ok_or_else(stale_workflow_review)?;
        if review.instance().id() != instance_id {
            return Err(stale_workflow_review());
        }
        let instance = review.instance().clone();
        let recipe = recorder.recipe_for_save(&instance)?;
        let path = save_recipe(project_root, &recipe)?;
        recorder
            .discard(&instance)
            .map_err(|_| stale_workflow_review())?;
        Ok(path)
    })
}

pub fn discard_browser_workflow_review(
    coordinator: &BrowserWorkflowCoordinator,
    active_workspace: Option<&BrowserWorkspaceKey>,
    action_workspace: &BrowserWorkspaceKey,
    surface: BrowserPaneSurface,
    instance_id: u64,
) -> Result<(), BrowserError> {
    validate_workflow_review_route(active_workspace, action_workspace, surface)?;
    coordinator.with_recorder(|recorder| {
        let review = recorder
            .review_for_workspace(action_workspace)
            .ok_or_else(stale_workflow_review)?;
        if review.instance().id() != instance_id {
            return Err(stale_workflow_review());
        }
        let instance = review.instance().clone();
        recorder
            .discard(&instance)
            .map_err(|_| stale_workflow_review())
    })
}

fn validate_workflow_review_route(
    active_workspace: Option<&BrowserWorkspaceKey>,
    action_workspace: &BrowserWorkspaceKey,
    surface: BrowserPaneSurface,
) -> Result<(), BrowserError> {
    if active_workspace == Some(action_workspace)
        && matches!(
            surface,
            BrowserPaneSurface::Claude | BrowserPaneSurface::Codex
        )
    {
        Ok(())
    } else {
        Err(BrowserError::InvalidInvocation {
            field: "activeBrowserWorkspace".to_string(),
        })
    }
}

fn stale_workflow_review() -> BrowserError {
    BrowserError::InvalidRecipe {
        message: "recording review instance is not active".to_string(),
    }
}

pub fn browser_workflow_review_projection(
    coordinator: &BrowserWorkflowCoordinator,
    workspace_key: &BrowserWorkspaceKey,
    surface: BrowserPaneSurface,
) -> Option<BrowserWorkflowReviewProjection> {
    if !matches!(
        surface,
        BrowserPaneSurface::Claude | BrowserPaneSurface::Codex
    ) {
        return None;
    }

    Some(coordinator.with_recorder(|recorder| {
        let status = recorder.status(workspace_key);
        let empty = |state| BrowserWorkflowReviewProjection {
            workspace_key: workspace_key.clone(),
            state,
            metadata: None,
            steps: Vec::new(),
            inputs: Vec::new(),
        };
        match status {
            BrowserRecordingStatus::Inactive => empty(BrowserWorkflowReviewUiState::Inactive),
            BrowserRecordingStatus::Recording => {
                let instance_id = recorder
                    .active_instance(workspace_key)
                    .map(|instance| instance.id())
                    .unwrap_or_default();
                empty(BrowserWorkflowReviewUiState::Recording { instance_id })
            }
            BrowserRecordingStatus::Review => {
                let Some(review) = recorder.review_for_workspace(workspace_key) else {
                    return empty(BrowserWorkflowReviewUiState::Inactive);
                };
                let recipe = review.recipe();
                let inputs = recipe
                    .inputs
                    .iter()
                    .take(MAX_BROWSER_RECORDING_INPUTS)
                    .map(|input| BrowserWorkflowReviewInputProjection {
                        name: input.name.clone(),
                        kind: input.kind,
                        unset: input.default_value.is_none(),
                    })
                    .collect();
                let steps = recipe
                    .steps
                    .iter()
                    .take(256)
                    .enumerate()
                    .filter_map(|(index, step)| {
                        Some(BrowserWorkflowReviewStepProjection {
                            id: step.id.clone(),
                            index,
                            actor: review.actor_for_step(&step.id)?,
                            summary: browser_workflow_step_summary(&review, &step.action)
                                .to_string(),
                            convertible_kind: browser_workflow_convertible_kind(
                                recipe,
                                &step.action,
                            ),
                            has_wait: step.wait.is_some(),
                            assertion_count: step.assertions.len(),
                            has_assertion_locator: review
                                .primary_locator_for_step(&step.id)
                                .is_some(),
                            can_move_up: index > 0 && review.can_move_step(&step.id, index - 1),
                            can_move_down: index + 1 < recipe.steps.len()
                                && review.can_move_step(&step.id, index + 1),
                        })
                    })
                    .collect();
                BrowserWorkflowReviewProjection {
                    workspace_key: workspace_key.clone(),
                    state: BrowserWorkflowReviewUiState::Review {
                        instance_id: review.instance().id(),
                    },
                    metadata: Some(BrowserWorkflowReviewMetadataProjection {
                        id: recipe.id.clone(),
                        name: recipe.name.clone(),
                        description: recipe.description.clone(),
                        start_url: recipe.start_url.clone(),
                        viewport: recipe.viewport.clone(),
                    }),
                    steps,
                    inputs,
                }
            }
        }
    }))
}

fn browser_workflow_convertible_kind(
    recipe: &BrowserRecipeV1,
    action: &BrowserRecipeAction,
) -> Option<BrowserRecipeInputKind> {
    let (value, kind) = match action {
        BrowserRecipeAction::CreateTab {
            url: Some(value), ..
        }
        | BrowserRecipeAction::Navigate { url: value } => (value, BrowserRecipeInputKind::Url),
        BrowserRecipeAction::Type { value, .. }
        | BrowserRecipeAction::Keypress { key: value, .. } => (value, BrowserRecipeInputKind::Text),
        BrowserRecipeAction::Upload { file, .. } => (file, BrowserRecipeInputKind::File),
        _ => return None,
    };
    if let BrowserRecipeValue::Input { name } = value {
        let existing_kind = recipe
            .inputs
            .iter()
            .find(|input| input.name == *name)
            .map(|input| input.kind)?;
        if matches!(
            existing_kind,
            BrowserRecipeInputKind::File | BrowserRecipeInputKind::Secret
        ) {
            return None;
        }
    }
    Some(kind)
}

fn browser_workflow_step_summary(
    review: &super::BrowserRecordingReview,
    action: &BrowserRecipeAction,
) -> &'static str {
    match action {
        BrowserRecipeAction::CreateTab { .. } => "Create tab",
        BrowserRecipeAction::SelectTab { .. } => "Select tab",
        BrowserRecipeAction::CloseTab { .. } => "Close tab",
        BrowserRecipeAction::Back => "Go back",
        BrowserRecipeAction::Forward => "Go forward",
        BrowserRecipeAction::Reload => "Reload",
        BrowserRecipeAction::SetViewport { .. } => "Set viewport",
        BrowserRecipeAction::CdpMarker { .. } => "Run browser method",
        BrowserRecipeAction::Navigate { .. } => "Navigate",
        BrowserRecipeAction::Click { .. } => "Click",
        BrowserRecipeAction::Hover { .. } => "Hover",
        BrowserRecipeAction::Focus { .. } => "Focus",
        BrowserRecipeAction::Type {
            value: BrowserRecipeValue::Input { name },
            ..
        } if review
            .recipe()
            .inputs
            .iter()
            .any(|input| input.name == *name && input.kind == BrowserRecipeInputKind::Secret) =>
        {
            "Type secret input"
        }
        BrowserRecipeAction::Type {
            value: BrowserRecipeValue::Input { .. },
            ..
        } => "Type input",
        BrowserRecipeAction::Type { .. } => "Type text",
        BrowserRecipeAction::Clear { .. } => "Clear field",
        BrowserRecipeAction::Select { .. } => "Select option",
        BrowserRecipeAction::Keypress { .. } => "Press key",
        BrowserRecipeAction::Scroll { .. } => "Scroll",
        BrowserRecipeAction::DragDrop { .. } => "Drag and drop",
        BrowserRecipeAction::Upload { .. } => "Upload file input",
        BrowserRecipeAction::Download { .. } => "Download",
        BrowserRecipeAction::Wait { .. } => "Wait",
        BrowserRecipeAction::Screenshot { .. } => "Take screenshot",
    }
}

fn browser_workflow_next_input_name(
    inputs: &[BrowserWorkflowReviewInputProjection],
    prefix: &str,
) -> Option<String> {
    if inputs.len() >= MAX_BROWSER_RECORDING_INPUTS {
        return None;
    }
    (1..=MAX_BROWSER_RECORDING_INPUTS)
        .map(|index| format!("{prefix}_input_{index}"))
        .find(|candidate| !inputs.iter().any(|input| input.name == *candidate))
}

#[cfg(test)]
mod workflow_review_ui_tests {
    use super::{
        browser_workflow_next_input_name, BrowserWorkflowReviewInputProjection,
        MAX_BROWSER_RECORDING_INPUTS,
    };
    use crate::browser::BrowserRecipeInputKind;

    #[test]
    fn generated_review_input_names_fill_holes_without_colliding_and_stop_at_capacity() {
        let existing = vec![
            BrowserWorkflowReviewInputProjection {
                name: "text_input_1".to_string(),
                kind: BrowserRecipeInputKind::Text,
                unset: true,
            },
            BrowserWorkflowReviewInputProjection {
                name: "text_input_3".to_string(),
                kind: BrowserRecipeInputKind::Text,
                unset: true,
            },
        ];
        assert_eq!(
            browser_workflow_next_input_name(&existing, "text"),
            Some("text_input_2".to_string())
        );

        let full = (1..=MAX_BROWSER_RECORDING_INPUTS)
            .map(|index| BrowserWorkflowReviewInputProjection {
                name: format!("text_input_{index}"),
                kind: BrowserRecipeInputKind::Text,
                unset: true,
            })
            .collect::<Vec<_>>();
        assert_eq!(browser_workflow_next_input_name(&full, "text"), None);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BrowserPaneContext {
    pub browser_enabled: bool,
    pub platform_supported: bool,
    pub active_surface: Option<BrowserPaneSurface>,
    pub editor_open: bool,
    pub modal_open: bool,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BrowserSplitLayout {
    pub total_width: f32,
    pub terminal_width: f32,
    pub divider_width: f32,
    pub pane_width: f32,
    pub split_percent: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BrowserViewportPreset {
    Desktop,
    Tablet,
    Mobile,
}

impl BrowserViewportPreset {
    pub fn viewport(self) -> BrowserViewport {
        match self {
            Self::Desktop => BrowserViewport {
                width: 1280,
                height: 720,
                scale_percent: 100,
            },
            Self::Tablet => BrowserViewport {
                width: 768,
                height: 1024,
                scale_percent: 100,
            },
            Self::Mobile => BrowserViewport {
                width: 390,
                height: 844,
                scale_percent: 100,
            },
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Desktop => "Desktop 1280x720",
            Self::Tablet => "Tablet 768x1024",
            Self::Mobile => "Mobile 390x844",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum BrowserPaneAction {
    Open,
    Collapse,
    DividerBegin {
        pointer_x: f32,
    },
    DividerUpdate {
        pointer_x: f32,
    },
    DividerEnd,
    CreateTab,
    SelectTab(String),
    CloseTab(String),
    Back,
    Forward,
    Reload,
    FocusAddress,
    FocusAnnotation,
    EditAddress(String),
    SubmitAddress,
    FocusReplaySecret {
        workspace_key: BrowserWorkspaceKey,
        instance_id: u64,
        input_name: String,
    },
    SubmitReplaySecrets {
        workspace_key: BrowserWorkspaceKey,
        instance_id: u64,
    },
    CancelReplaySecrets {
        workspace_key: BrowserWorkspaceKey,
        instance_id: u64,
    },
    CancelReplay {
        instance_id: u64,
    },
    BeginReplayRepairSelection {
        instance_id: u64,
        repair_id: u64,
    },
    ApplyReplayRepair {
        instance_id: u64,
        repair_id: u64,
        resume: bool,
    },
    SetViewport(BrowserViewportPreset),
    ToggleAnnotation,
    SaveAnnotation,
    CancelAnnotation,
    StartRecording,
    StopRecording {
        instance_id: u64,
    },
    PreviewRecordingReview {
        instance_id: u64,
    },
    SaveRecordingReview {
        instance_id: u64,
    },
    DiscardRecordingReview {
        instance_id: u64,
    },
    MutateRecordingReview {
        instance_id: u64,
        mutation: BrowserWorkflowReviewMutation,
    },
    FocusRecordingReviewField {
        instance_id: u64,
        field: BrowserWorkflowReviewEditorField,
    },
    CancelRecordingReviewEdit,
    OpenDevTools,
    OpenDownloads,
    Stop,
    ResetWorkspace,
    ClearProjectProfile,
}

#[derive(Clone, PartialEq, Eq)]
pub struct BrowserReplayPaneProjection {
    pub replay: BrowserReplayProjection,
    pub repair: Option<BrowserReplayRepairProjection>,
    pub selecting_replacement: bool,
    pub repair_apply_ready: bool,
}

impl fmt::Debug for BrowserReplayPaneProjection {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let repair = self.repair.as_ref();
        formatter
            .debug_struct("BrowserReplayPaneProjection")
            .field("recipe_id", &self.replay.recipe_id)
            .field("status", &self.replay.status)
            .field("current_step_index", &self.replay.current_step_index)
            .field("total_steps", &self.replay.total_steps)
            .field(
                "unresolved_secret_inputs",
                &self.replay.unresolved_secret_inputs,
            )
            .field("repair_step_id", &repair.map(|repair| &repair.step_id))
            .field("repair_step_index", &repair.map(|repair| repair.step_index))
            .field(
                "repair_locator_slot",
                &repair.map(|repair| repair.locator_slot),
            )
            .field("repair_phase", &repair.map(|repair| repair.phase))
            .field("repair_snapshot_available", &repair.is_some())
            .field("repair_screenshot_available", &repair.is_some())
            .field("selecting_replacement", &self.selecting_replacement)
            .field("repair_apply_ready", &self.repair_apply_ready)
            .finish()
    }
}

impl BrowserPaneAction {
    pub fn is_annotation_editor_action(&self) -> bool {
        matches!(
            self,
            Self::FocusAnnotation
                | Self::ToggleAnnotation
                | Self::SaveAnnotation
                | Self::CancelAnnotation
        )
    }
}

#[derive(Clone, Default, PartialEq, Eq)]
pub struct BrowserPaneTransient {
    pub address_draft: Option<String>,
    pub address_cursor: usize,
    pub address_focused: bool,
    pub loading: bool,
    pub diagnostic: Option<String>,
    pub action_status: Option<String>,
    pub divider_dragging: bool,
    pub annotation_mode: bool,
    pub annotation_draft: Option<BrowserAnnotationDraft>,
    pub annotation_comment: String,
    pub annotation_cursor: usize,
    pub annotation_focused: bool,
    pub workflow_review: Option<BrowserWorkflowReviewProjection>,
    pub workflow_preview: Option<String>,
    pub workflow_editor: Option<BrowserWorkflowReviewEditor>,
    pub replay_secret_prompt: Option<BrowserReplaySecretPromptProjection>,
    pub replay: Option<BrowserReplayPaneProjection>,
}

impl fmt::Debug for BrowserPaneTransient {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("BrowserPaneTransient")
            .field("address_draft_set", &self.address_draft.is_some())
            .field("address_cursor", &self.address_cursor)
            .field("address_focused", &self.address_focused)
            .field("loading", &self.loading)
            .field("diagnostic_set", &self.diagnostic.is_some())
            .field("action_status_set", &self.action_status.is_some())
            .field("divider_dragging", &self.divider_dragging)
            .field("annotation_mode", &self.annotation_mode)
            .field("annotation_draft_set", &self.annotation_draft.is_some())
            .field(
                "annotation_comment_set",
                &!self.annotation_comment.is_empty(),
            )
            .field("annotation_cursor", &self.annotation_cursor)
            .field("annotation_focused", &self.annotation_focused)
            .field("workflow_review_set", &self.workflow_review.is_some())
            .field("workflow_preview_set", &self.workflow_preview.is_some())
            .field("workflow_editor_set", &self.workflow_editor.is_some())
            .field("replay_secret_prompt", &self.replay_secret_prompt)
            .field("replay", &self.replay)
            .finish()
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct BrowserPaneModel {
    pub workspace_key: BrowserWorkspaceKey,
    pub eligible: bool,
    pub pane_open: bool,
    pub split_percent: u8,
    pub tabs: Vec<BrowserTabSnapshot>,
    pub selected_tab_id: Option<String>,
    pub address_draft: String,
    pub address_cursor: usize,
    pub address_focused: bool,
    pub loading: bool,
    pub diagnostic: Option<String>,
    pub action_status: Option<String>,
    pub journal_entries: Vec<BrowserJournalEntry>,
    pub divider_dragging: bool,
    pub annotation_mode: bool,
    pub annotation_draft: Option<BrowserAnnotationDraft>,
    pub annotation_comment: String,
    pub annotation_cursor: usize,
    pub annotation_focused: bool,
    pub workflow_review: Option<BrowserWorkflowReviewProjection>,
    pub workflow_preview: Option<String>,
    pub workflow_editor: Option<BrowserWorkflowReviewEditor>,
    pub replay_secret_prompt: Option<BrowserReplaySecretPromptProjection>,
    pub replay: Option<BrowserReplayPaneProjection>,
}

impl fmt::Debug for BrowserPaneModel {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("BrowserPaneModel")
            .field("workspace_key", &self.workspace_key)
            .field("eligible", &self.eligible)
            .field("pane_open", &self.pane_open)
            .field("split_percent", &self.split_percent)
            .field("tab_count", &self.tabs.len())
            .field("selected_tab_id", &self.selected_tab_id)
            .field("address_draft_set", &!self.address_draft.is_empty())
            .field("address_cursor", &self.address_cursor)
            .field("address_focused", &self.address_focused)
            .field("loading", &self.loading)
            .field("diagnostic_set", &self.diagnostic.is_some())
            .field("action_status_set", &self.action_status.is_some())
            .field("journal_entry_count", &self.journal_entries.len())
            .field("divider_dragging", &self.divider_dragging)
            .field("annotation_mode", &self.annotation_mode)
            .field("annotation_draft_set", &self.annotation_draft.is_some())
            .field(
                "annotation_comment_set",
                &!self.annotation_comment.is_empty(),
            )
            .field("annotation_cursor", &self.annotation_cursor)
            .field("annotation_focused", &self.annotation_focused)
            .field("workflow_review_set", &self.workflow_review.is_some())
            .field("workflow_preview_set", &self.workflow_preview.is_some())
            .field("workflow_editor_set", &self.workflow_editor.is_some())
            .field("replay_secret_prompt", &self.replay_secret_prompt)
            .field("replay", &self.replay)
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BrowserHostVisibility {
    Hidden,
    Selected {
        workspace_key: BrowserWorkspaceKey,
        tab_id: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrowserHostReconcilePlan {
    pub visibility: BrowserHostVisibility,
    pub ensure_snapshot: Option<BrowserWorkspaceSnapshot>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrowserActionPlan {
    pub workspace_key: BrowserWorkspaceKey,
    pub commands: Vec<BrowserCommand>,
    pub diagnostic: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrowserSnapshotSync {
    pub workspace_key: BrowserWorkspaceKey,
    pub revision: BrowserRevision,
    pub snapshot: BrowserWorkspaceSnapshot,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BrowserPaneEventPlan {
    SyncSnapshot {
        workspace_key: BrowserWorkspaceKey,
        tab_id: String,
        interrupt_agent: bool,
        loading: Option<bool>,
    },
    OpenLogicalTab {
        workspace_key: BrowserWorkspaceKey,
        url: String,
    },
    DownloadStatus {
        workspace_key: BrowserWorkspaceKey,
        message: String,
    },
    Diagnostic {
        workspace_key: BrowserWorkspaceKey,
        message: String,
    },
    ConfirmApproval {
        workspace_key: BrowserWorkspaceKey,
        tab_id: String,
        request: BrowserApprovalRequest,
    },
    CaptureAnnotation {
        workspace_key: BrowserWorkspaceKey,
        tab_id: String,
        candidate: BrowserAnnotationCandidate,
    },
    ShowAnnotationDraft {
        workspace_key: BrowserWorkspaceKey,
        draft: BrowserAnnotationDraft,
    },
    AnnotationModeChanged {
        workspace_key: BrowserWorkspaceKey,
        enabled: bool,
    },
    ClearAnnotation {
        workspace_key: BrowserWorkspaceKey,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BrowserSettingsAction {
    ClearActiveProjectProfile,
    ResetActiveConversation,
    RevealActiveDownloads,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrowserSettingsPlan {
    pub route_key: BrowserWorkspaceKey,
    pub command: BrowserCommand,
    pub reset_workspaces: Vec<BrowserWorkspaceKey>,
    pub preserve_downloads: bool,
    pub preserve_resources: bool,
}

impl BrowserPaneModel {
    pub fn new(
        workspace_key: BrowserWorkspaceKey,
        context: &BrowserPaneContext,
        snapshot: &BrowserWorkspaceSnapshot,
        transient: BrowserPaneTransient,
    ) -> Self {
        let selected_tab_id = selected_browser_tab_id(snapshot).map(ToOwned::to_owned);
        let selected_url = selected_tab_id
            .as_deref()
            .and_then(|selected| snapshot.tabs.iter().find(|tab| tab.id == selected))
            .map(|tab| tab.url.clone())
            .unwrap_or_default();
        let has_address_draft = transient.address_draft.is_some();
        let address_draft = transient.address_draft.unwrap_or(selected_url);
        let address_cursor = if has_address_draft {
            transient.address_cursor.min(address_draft.chars().count())
        } else {
            address_draft.chars().count()
        };
        let annotation_comment = transient.annotation_comment;
        let annotation_cursor = transient
            .annotation_cursor
            .min(annotation_comment.chars().count());
        let annotation_input_available = transient.replay_secret_prompt.is_none();
        Self {
            workspace_key,
            eligible: browser_pane_eligible(context),
            pane_open: snapshot.pane_open,
            split_percent: snapshot.split_percent.clamp(25, 75),
            tabs: snapshot.tabs.clone(),
            selected_tab_id,
            address_draft,
            address_cursor,
            address_focused: transient.address_focused,
            loading: transient.loading,
            diagnostic: transient.diagnostic,
            action_status: transient.action_status,
            journal_entries: snapshot
                .journal_entries
                .iter()
                .skip(snapshot.journal_entries.len().saturating_sub(3))
                .cloned()
                .collect(),
            divider_dragging: transient.divider_dragging,
            annotation_mode: transient.annotation_mode,
            annotation_draft: transient.annotation_draft,
            annotation_comment,
            annotation_cursor,
            annotation_focused: transient.annotation_focused && annotation_input_available,
            workflow_review: transient.workflow_review,
            workflow_preview: transient.workflow_preview,
            workflow_editor: transient.workflow_editor,
            replay_secret_prompt: transient.replay_secret_prompt,
            replay: transient.replay,
        }
    }

    pub fn annotation_editor_visible(&self) -> bool {
        self.annotation_draft.is_some() && self.replay_secret_prompt.is_none()
    }

    pub fn page_surface_visible(&self) -> bool {
        !matches!(
            self.workflow_review.as_ref().map(|review| &review.state),
            Some(BrowserWorkflowReviewUiState::Review { .. })
        ) && self.replay_secret_prompt.is_none()
    }
}

pub fn browser_replay_repair_candidate_from_annotation(
    candidate: &BrowserAnnotationCandidate,
    expected_revision: BrowserRevision,
) -> Result<BrowserReplayRepairCandidate, BrowserError> {
    candidate.validate()?;
    if candidate.kind != BrowserAnnotationKind::Element {
        return Err(BrowserError::InvalidAnnotation {
            field: "kind".to_string(),
            message: "repair replacement must be a semantic element".to_string(),
        });
    }
    if candidate.revision != expected_revision {
        return Err(BrowserError::StaleReference {
            expected: expected_revision,
            actual: candidate.revision,
        });
    }
    Ok(BrowserReplayRepairCandidate::new(BrowserElementRef {
        revision: candidate.revision,
        locator: candidate.locator.clone(),
        backend_node_id: None,
    }))
}

pub fn selected_browser_tab_id(snapshot: &BrowserWorkspaceSnapshot) -> Option<&str> {
    snapshot
        .selected_tab_id
        .as_deref()
        .filter(|selected| snapshot.tabs.iter().any(|tab| tab.id == *selected))
        .or_else(|| snapshot.tabs.first().map(|tab| tab.id.as_str()))
}

pub fn browser_host_visibility(
    context: &BrowserPaneContext,
    workspace_key: &BrowserWorkspaceKey,
    snapshot: &BrowserWorkspaceSnapshot,
    divider_dragging: bool,
) -> BrowserHostVisibility {
    if !browser_pane_eligible(context) || !snapshot.pane_open || divider_dragging {
        return BrowserHostVisibility::Hidden;
    }
    selected_browser_tab_id(snapshot).map_or(BrowserHostVisibility::Hidden, |tab_id| {
        BrowserHostVisibility::Selected {
            workspace_key: workspace_key.clone(),
            tab_id: tab_id.to_string(),
        }
    })
}

pub fn browser_host_reconcile_plan(
    context: &BrowserPaneContext,
    workspace_key: &BrowserWorkspaceKey,
    persisted_snapshot: &BrowserWorkspaceSnapshot,
    divider_dragging: bool,
    live_host_snapshot: Option<&BrowserWorkspaceSnapshot>,
) -> BrowserHostReconcilePlan {
    let visibility =
        browser_host_visibility(context, workspace_key, persisted_snapshot, divider_dragging);
    let ensure_snapshot = match (&visibility, live_host_snapshot) {
        (BrowserHostVisibility::Selected { .. }, None) => Some(persisted_snapshot.clone()),
        _ => None,
    };
    BrowserHostReconcilePlan {
        visibility,
        ensure_snapshot,
    }
}

pub fn browser_action_plan(
    active_workspace: Option<&BrowserWorkspaceKey>,
    snapshot: Option<&BrowserWorkspaceSnapshot>,
    address_draft: &str,
    action: BrowserPaneAction,
) -> Result<BrowserActionPlan, BrowserError> {
    let workspace_key =
        active_workspace
            .cloned()
            .ok_or_else(|| BrowserError::InvalidWorkspaceKey {
                field: "activeWorkspace".to_string(),
            })?;
    let diagnostic = None;
    let commands = match action {
        BrowserPaneAction::Open => {
            let snapshot = snapshot.cloned().unwrap_or_default();
            vec![
                BrowserCommand::Ensure { snapshot },
                BrowserCommand::SetPaneOpen { open: true },
            ]
        }
        BrowserPaneAction::Collapse => vec![BrowserCommand::SetPaneOpen { open: false }],
        BrowserPaneAction::CreateTab => vec![BrowserCommand::CreateTab { url: None }],
        BrowserPaneAction::SelectTab(tab_id) => {
            let snapshot = snapshot.ok_or_else(|| BrowserError::InvalidInvocation {
                field: "tabId".to_string(),
            })?;
            if !snapshot.tabs.iter().any(|tab| tab.id == tab_id) {
                return Err(BrowserError::InvalidInvocation {
                    field: "tabId".to_string(),
                });
            }
            if snapshot.selected_tab_id.as_deref() == Some(tab_id.as_str()) {
                Vec::new()
            } else {
                vec![BrowserCommand::SelectTab { tab_id }]
            }
        }
        BrowserPaneAction::CloseTab(tab_id) => vec![BrowserCommand::CloseTab { tab_id }],
        BrowserPaneAction::Back => vec![BrowserCommand::Back {
            tab_id: selected_tab(snapshot)?.to_string(),
        }],
        BrowserPaneAction::Forward => vec![BrowserCommand::Forward {
            tab_id: selected_tab(snapshot)?.to_string(),
        }],
        BrowserPaneAction::Reload => vec![BrowserCommand::Reload {
            tab_id: selected_tab(snapshot)?.to_string(),
        }],
        BrowserPaneAction::SubmitAddress => vec![BrowserCommand::Navigate {
            tab_id: selected_tab(snapshot)?.to_string(),
            url: normalize_browser_address(address_draft)?,
        }],
        BrowserPaneAction::SetViewport(preset) => vec![BrowserCommand::UpdateViewport {
            tab_id: selected_tab(snapshot)?.to_string(),
            viewport: preset.viewport(),
        }],
        BrowserPaneAction::OpenDevTools => vec![BrowserCommand::OpenDevTools {
            tab_id: selected_tab(snapshot)?.to_string(),
        }],
        BrowserPaneAction::OpenDownloads => vec![BrowserCommand::DownloadDirectory],
        BrowserPaneAction::Stop => vec![BrowserCommand::Stop {
            tab_id: snapshot
                .and_then(selected_browser_tab_id)
                .map(ToOwned::to_owned),
        }],
        BrowserPaneAction::ResetWorkspace => vec![BrowserCommand::ResetWorkspace],
        BrowserPaneAction::ClearProjectProfile => vec![BrowserCommand::ClearProjectProfile],
        BrowserPaneAction::ToggleAnnotation => vec![BrowserCommand::SetAnnotationMode {
            tab_id: selected_tab(snapshot)?.to_string(),
            enabled: true,
        }],
        BrowserPaneAction::StartRecording
        | BrowserPaneAction::StopRecording { .. }
        | BrowserPaneAction::PreviewRecordingReview { .. }
        | BrowserPaneAction::SaveRecordingReview { .. }
        | BrowserPaneAction::DiscardRecordingReview { .. }
        | BrowserPaneAction::MutateRecordingReview { .. }
        | BrowserPaneAction::FocusRecordingReviewField { .. }
        | BrowserPaneAction::CancelRecordingReviewEdit => Vec::new(),
        BrowserPaneAction::FocusReplaySecret { .. }
        | BrowserPaneAction::SubmitReplaySecrets { .. }
        | BrowserPaneAction::CancelReplaySecrets { .. }
        | BrowserPaneAction::CancelReplay { .. }
        | BrowserPaneAction::BeginReplayRepairSelection { .. }
        | BrowserPaneAction::ApplyReplayRepair { .. } => Vec::new(),
        BrowserPaneAction::SaveAnnotation | BrowserPaneAction::CancelAnnotation => Vec::new(),
        BrowserPaneAction::DividerBegin { .. }
        | BrowserPaneAction::DividerUpdate { .. }
        | BrowserPaneAction::DividerEnd
        | BrowserPaneAction::FocusAddress
        | BrowserPaneAction::FocusAnnotation
        | BrowserPaneAction::EditAddress(_) => Vec::new(),
    };

    Ok(BrowserActionPlan {
        workspace_key,
        commands,
        diagnostic,
    })
}

pub fn browser_annotation_preview_plan(
    active_workspace: Option<&BrowserWorkspaceKey>,
    action_workspace: &BrowserWorkspaceKey,
    snapshot: Option<&BrowserWorkspaceSnapshot>,
    pending_annotations: &[BrowserAnnotation],
    annotation_id: &str,
) -> Result<BrowserActionPlan, BrowserError> {
    let missing = || BrowserError::MissingAnnotation {
        id: annotation_id.to_string(),
    };
    let active_workspace = active_workspace.ok_or_else(missing)?;
    if active_workspace != action_workspace {
        return Err(missing());
    }
    let annotation = pending_annotations
        .iter()
        .find(|annotation| annotation.id == annotation_id)
        .ok_or_else(missing)?;
    let saved_url = validate_browser_url(&annotation.url)?;
    let snapshot = snapshot.cloned().unwrap_or_default();
    let mut commands = vec![BrowserCommand::Ensure {
        snapshot: snapshot.clone(),
    }];
    if !snapshot.pane_open {
        commands.push(BrowserCommand::SetPaneOpen { open: true });
    }
    if let Some(tab) = snapshot.tabs.iter().find(|tab| tab.id == annotation.tab_id) {
        if snapshot.selected_tab_id.as_deref() != Some(annotation.tab_id.as_str()) {
            commands.push(BrowserCommand::SelectTab {
                tab_id: annotation.tab_id.clone(),
            });
        }
        if !browser_annotation_urls_equivalent(&tab.url, &saved_url) {
            commands.push(BrowserCommand::Navigate {
                tab_id: annotation.tab_id.clone(),
                url: saved_url,
            });
        }
    } else {
        commands.push(BrowserCommand::CreateTab {
            url: Some(saved_url),
        });
    }

    Ok(BrowserActionPlan {
        workspace_key: active_workspace.clone(),
        commands,
        diagnostic: None,
    })
}

pub fn browser_pane_open_fallback(action: &BrowserPaneAction) -> Option<bool> {
    match action {
        BrowserPaneAction::Open => Some(true),
        BrowserPaneAction::Collapse => Some(false),
        _ => None,
    }
}

pub fn browser_response_sync(
    open_workspaces: &[BrowserWorkspaceKey],
    route: &BrowserWorkspaceKey,
    response: &BrowserResponse,
) -> Option<BrowserSnapshotSync> {
    if !open_workspaces.iter().any(|open| open == route) {
        return None;
    }
    match response {
        BrowserResponse::Workspace { mutation } => Some(BrowserSnapshotSync {
            workspace_key: route.clone(),
            revision: mutation.revision,
            snapshot: mutation.snapshot.clone(),
        }),
        BrowserResponse::WorkspaceState { snapshot } => Some(BrowserSnapshotSync {
            workspace_key: route.clone(),
            revision: snapshot.revision,
            snapshot: snapshot.clone(),
        }),
        BrowserResponse::Annotations { mutation, .. }
        | BrowserResponse::Annotation { mutation, .. } => Some(BrowserSnapshotSync {
            workspace_key: route.clone(),
            revision: mutation.revision,
            snapshot: mutation.snapshot.clone(),
        }),
        BrowserResponse::AnnotationMutation { result } => Some(BrowserSnapshotSync {
            workspace_key: route.clone(),
            revision: result.mutation.revision,
            snapshot: result.mutation.snapshot.clone(),
        }),
        BrowserResponse::Status { .. }
        | BrowserResponse::Tabs { .. }
        | BrowserResponse::DownloadDirectory { .. }
        | BrowserResponse::Snapshot { .. }
        | BrowserResponse::Screenshot { .. }
        | BrowserResponse::Wait { .. }
        | BrowserResponse::Action { .. }
        | BrowserResponse::Console { .. }
        | BrowserResponse::Network { .. }
        | BrowserResponse::Performance { .. }
        | BrowserResponse::Upload { .. }
        | BrowserResponse::Downloads { .. }
        | BrowserResponse::Cdp { .. }
        | BrowserResponse::AnnotationDraft { .. }
        | BrowserResponse::Recording { .. }
        | BrowserResponse::Acknowledged => None,
    }
}

pub fn browser_event_plan(
    open_workspaces: &[BrowserWorkspaceKey],
    event: &BrowserHostEvent,
) -> Option<BrowserPaneEventPlan> {
    let workspace_key = match event {
        BrowserHostEvent::UrlChanged { workspace_key, .. }
        | BrowserHostEvent::TitleChanged { workspace_key, .. }
        | BrowserHostEvent::PageLoad { workspace_key, .. }
        | BrowserHostEvent::UserInput { workspace_key, .. }
        | BrowserHostEvent::DomMutation { workspace_key, .. }
        | BrowserHostEvent::AnnotationCandidate { workspace_key, .. }
        | BrowserHostEvent::AnnotationCanceled { workspace_key, .. }
        | BrowserHostEvent::AnnotationDraftReady { workspace_key, .. }
        | BrowserHostEvent::AnnotationModeChanged { workspace_key, .. }
        | BrowserHostEvent::AutomationStateChanged { workspace_key, .. }
        | BrowserHostEvent::ApprovalRequested { workspace_key, .. }
        | BrowserHostEvent::NewWindow { workspace_key, .. }
        | BrowserHostEvent::Download { workspace_key, .. }
        | BrowserHostEvent::Diagnostic { workspace_key, .. } => workspace_key,
    };
    if !open_workspaces.iter().any(|open| open == workspace_key) {
        return None;
    }

    match event {
        BrowserHostEvent::UrlChanged { tab_id, .. }
        | BrowserHostEvent::TitleChanged { tab_id, .. } => {
            Some(BrowserPaneEventPlan::SyncSnapshot {
                workspace_key: workspace_key.clone(),
                tab_id: tab_id.clone(),
                interrupt_agent: false,
                loading: None,
            })
        }
        BrowserHostEvent::PageLoad { tab_id, state, .. } => {
            Some(BrowserPaneEventPlan::SyncSnapshot {
                workspace_key: workspace_key.clone(),
                tab_id: tab_id.clone(),
                interrupt_agent: false,
                loading: Some(matches!(state, BrowserPageLoadState::Started)),
            })
        }
        BrowserHostEvent::UserInput { tab_id, .. } => Some(BrowserPaneEventPlan::SyncSnapshot {
            workspace_key: workspace_key.clone(),
            tab_id: tab_id.clone(),
            interrupt_agent: true,
            loading: None,
        }),
        BrowserHostEvent::DomMutation { tab_id, .. }
        | BrowserHostEvent::AutomationStateChanged { tab_id, .. } => {
            Some(BrowserPaneEventPlan::SyncSnapshot {
                workspace_key: workspace_key.clone(),
                tab_id: tab_id.clone(),
                interrupt_agent: false,
                loading: None,
            })
        }
        BrowserHostEvent::AnnotationCandidate {
            tab_id, candidate, ..
        } => Some(BrowserPaneEventPlan::CaptureAnnotation {
            workspace_key: workspace_key.clone(),
            tab_id: tab_id.clone(),
            candidate: candidate.clone(),
        }),
        BrowserHostEvent::AnnotationCanceled { .. } => {
            Some(BrowserPaneEventPlan::ClearAnnotation {
                workspace_key: workspace_key.clone(),
            })
        }
        BrowserHostEvent::AnnotationDraftReady { draft, .. } => {
            Some(BrowserPaneEventPlan::ShowAnnotationDraft {
                workspace_key: workspace_key.clone(),
                draft: draft.clone(),
            })
        }
        BrowserHostEvent::AnnotationModeChanged { enabled, .. } => {
            Some(BrowserPaneEventPlan::AnnotationModeChanged {
                workspace_key: workspace_key.clone(),
                enabled: *enabled,
            })
        }
        BrowserHostEvent::ApprovalRequested {
            tab_id, request, ..
        } => Some(BrowserPaneEventPlan::ConfirmApproval {
            workspace_key: workspace_key.clone(),
            tab_id: tab_id.clone(),
            request: request.clone(),
        }),
        BrowserHostEvent::NewWindow { url, .. } => Some(BrowserPaneEventPlan::OpenLogicalTab {
            workspace_key: workspace_key.clone(),
            url: url.clone(),
        }),
        BrowserHostEvent::Download { state, path, .. } => {
            let file = path
                .file_name()
                .and_then(|value| value.to_str())
                .unwrap_or("download");
            let message = match state {
                BrowserDownloadState::Started => format!("Downloading {file}"),
                BrowserDownloadState::Completed { successful: true } => {
                    format!("Downloaded {file}")
                }
                BrowserDownloadState::Completed { successful: false } => {
                    format!("Download failed: {file}")
                }
            };
            Some(BrowserPaneEventPlan::DownloadStatus {
                workspace_key: workspace_key.clone(),
                message,
            })
        }
        BrowserHostEvent::Diagnostic { message, .. } => Some(BrowserPaneEventPlan::Diagnostic {
            workspace_key: workspace_key.clone(),
            message: message.clone(),
        }),
    }
}

pub fn browser_settings_plan(
    action: BrowserSettingsAction,
    active_workspace: Option<&BrowserWorkspaceKey>,
    open_workspaces: &[BrowserWorkspaceKey],
) -> Result<BrowserSettingsPlan, BrowserError> {
    let route_key = active_workspace
        .cloned()
        .ok_or_else(|| BrowserError::InvalidWorkspaceKey {
            field: "activeWorkspace".to_string(),
        })?;
    let (command, reset_workspaces) = match action {
        BrowserSettingsAction::ClearActiveProjectProfile => (
            BrowserCommand::ClearProjectProfile,
            open_workspaces
                .iter()
                .filter(|key| key.project_id == route_key.project_id)
                .cloned()
                .collect(),
        ),
        BrowserSettingsAction::ResetActiveConversation => {
            (BrowserCommand::ResetWorkspace, vec![route_key.clone()])
        }
        BrowserSettingsAction::RevealActiveDownloads => {
            (BrowserCommand::DownloadDirectory, Vec::new())
        }
    };
    Ok(BrowserSettingsPlan {
        route_key,
        command,
        reset_workspaces,
        preserve_downloads: true,
        preserve_resources: true,
    })
}

fn selected_tab(snapshot: Option<&BrowserWorkspaceSnapshot>) -> Result<&str, BrowserError> {
    snapshot
        .and_then(selected_browser_tab_id)
        .ok_or_else(|| BrowserError::CrashedView {
            message: "browser workspace has no selected tab".to_string(),
        })
}

pub fn browser_pane_eligible(context: &BrowserPaneContext) -> bool {
    context.browser_enabled
        && context.platform_supported
        && !context.editor_open
        && !context.modal_open
        && matches!(
            context.active_surface,
            Some(BrowserPaneSurface::Claude | BrowserPaneSurface::Codex)
        )
}

pub fn calculate_browser_split(
    total_width: f32,
    split_percent: u8,
    terminal_min_width: f32,
    pane_min_width: f32,
    divider_width: f32,
) -> BrowserSplitLayout {
    let total_width = total_width.max(0.0);
    let divider_width = divider_width.max(0.0).min(total_width);
    let available_width = total_width - divider_width;
    let split_percent = split_percent.clamp(25, 75);
    let desired_pane_width = available_width * f32::from(split_percent) / 100.0;
    let terminal_min_width = terminal_min_width.max(0.0);
    let pane_min_width = pane_min_width.max(0.0);
    let pane_width = if available_width >= terminal_min_width + pane_min_width {
        desired_pane_width.clamp(pane_min_width, available_width - terminal_min_width)
    } else {
        desired_pane_width.clamp(0.0, available_width)
    };
    let terminal_width = available_width - pane_width;

    BrowserSplitLayout {
        total_width,
        terminal_width,
        divider_width,
        pane_width,
        split_percent,
    }
}

pub fn browser_content_bounds(
    pane_bounds: BrowserBounds,
    toolbar_height: i32,
) -> Option<BrowserBounds> {
    let toolbar_height = toolbar_height.max(0);
    let height = pane_bounds.height.checked_sub(toolbar_height)?;
    if pane_bounds.width <= 0 || height <= 0 {
        return None;
    }
    Some(BrowserBounds {
        x: pane_bounds.x,
        y: pane_bounds.y.saturating_add(toolbar_height),
        width: pane_bounds.width,
        height,
    })
}

pub fn normalize_browser_address(input: &str) -> Result<String, BrowserError> {
    let address = input.trim();
    let failure = |message: &str| BrowserError::NavigationFailure {
        url: address.to_string(),
        message: message.to_string(),
    };
    if address.is_empty() {
        return Err(failure("address cannot be blank"));
    }
    if address.eq_ignore_ascii_case("about:blank") {
        return Ok("about:blank".to_string());
    }
    if address.contains(char::is_whitespace) || address.contains('\\') {
        return Err(failure("address must contain a host, not free text"));
    }
    if address.contains("://") {
        return validate_browser_url(address);
    }
    if address.contains('@') {
        return Err(failure("address user information is not supported"));
    }

    let authority = address.split(['/', '?', '#']).next().unwrap_or_default();
    let (host, explicit_ipv6) = split_host(authority).ok_or_else(|| failure("invalid host"))?;
    let is_loopback = host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<Ipv4Addr>()
            .is_ok_and(|address| address.is_loopback())
        || host
            .parse::<Ipv6Addr>()
            .is_ok_and(|address| address.is_loopback());
    let is_local = host.to_ascii_lowercase().ends_with(".local");
    let host_like = explicit_ipv6
        || host.parse::<Ipv4Addr>().is_ok()
        || host.split('.').all(|label| is_valid_hostname_label(label));
    if !host_like {
        return Err(failure("address must contain a valid host"));
    }

    let scheme = if is_loopback || is_local {
        "http"
    } else {
        "https"
    };
    let normalized_address = if explicit_ipv6 && !authority.starts_with('[') {
        format!("[{authority}]{}", &address[authority.len()..])
    } else {
        address.to_string()
    };
    validate_browser_url(&format!("{scheme}://{normalized_address}"))
}

fn split_host(authority: &str) -> Option<(&str, bool)> {
    if authority.starts_with('[') {
        let close = authority.find(']')?;
        let host = &authority[1..close];
        let suffix = &authority[close + 1..];
        if suffix.is_empty()
            || suffix
                .strip_prefix(':')
                .is_some_and(|port| !port.is_empty() && port.chars().all(|c| c.is_ascii_digit()))
        {
            return host.parse::<Ipv6Addr>().ok().map(|_| (host, true));
        }
        return None;
    }

    if authority.parse::<Ipv6Addr>().is_ok() {
        return Some((authority, true));
    }
    let (host, port) = authority.rsplit_once(':').unwrap_or((authority, ""));
    if authority.contains(':') && (port.is_empty() || !port.chars().all(|c| c.is_ascii_digit())) {
        return None;
    }
    if host.is_empty() || (authority.contains(':') && port.is_empty()) {
        return None;
    }
    Some((host, false))
}

fn is_valid_hostname_label(label: &str) -> bool {
    !label.is_empty()
        && label.len() <= 63
        && !label.starts_with('-')
        && !label.ends_with('-')
        && label
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || character == '-')
}

pub struct BrowserPaneActions {
    pub on_action:
        Arc<dyn Fn(BrowserPaneAction) -> Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)>>,
    pub on_address_key: Box<dyn Fn(&KeyDownEvent, &mut Window, &mut App)>,
    pub on_replay_secret_key: Box<dyn Fn(&KeyDownEvent, &mut Window, &mut App)>,
    pub on_annotation_key: Box<dyn Fn(&KeyDownEvent, &mut Window, &mut App)>,
    pub on_workflow_key: Box<dyn Fn(&KeyDownEvent, &mut Window, &mut App)>,
    pub on_page_bounds: Arc<dyn Fn(Bounds<Pixels>, &mut Window, &mut App)>,
}

struct BrowserReplaySecretPromptRenderInput<'a> {
    name: &'a str,
    is_set: bool,
}

pub fn render_browser_pane(
    model: BrowserPaneModel,
    address_focus: FocusHandle,
    replay_secret_focus: FocusHandle,
    annotation_focus: FocusHandle,
    workflow_focus: FocusHandle,
    actions: BrowserPaneActions,
) -> impl IntoElement {
    let action = actions.on_action.clone();
    let show_page_surface = model.page_surface_visible();
    let selected_viewport = model
        .selected_tab_id
        .as_deref()
        .and_then(|selected| model.tabs.iter().find(|tab| tab.id == selected))
        .map(|tab| tab.viewport.clone());
    let tab_strip = model.tabs.iter().map(|tab| {
        let selected = model.selected_tab_id.as_deref() == Some(tab.id.as_str());
        let select = action(BrowserPaneAction::SelectTab(tab.id.clone()));
        let close = action(BrowserPaneAction::CloseTab(tab.id.clone()));
        div()
            .flex()
            .items_center()
            .min_w(px(0.0))
            .max_w(px(180.0))
            .border_r_1()
            .border_color(rgb(theme::BORDER_PRIMARY))
            .bg(rgb(if selected {
                theme::TAB_ACTIVE_BG
            } else {
                theme::TOPBAR_BG
            }))
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.0))
                    .px(px(6.0))
                    .py(px(3.0))
                    .text_xs()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_color(rgb(if selected {
                        theme::TEXT_PRIMARY
                    } else {
                        theme::TEXT_MUTED
                    }))
                    .on_mouse_down(MouseButton::Left, select)
                    .child(SharedString::from(browser_tab_label(tab))),
            )
            .child(
                div()
                    .px(px(4.0))
                    .py(px(3.0))
                    .text_xs()
                    .text_color(rgb(theme::TEXT_DIM))
                    .hover(|style| style.bg(rgb(theme::DANGER_BG_SUBTLE)))
                    .on_mouse_down(MouseButton::Left, close)
                    .child("x"),
            )
            .into_any_element()
    });
    let status = model
        .diagnostic
        .clone()
        .or_else(|| model.action_status.clone())
        .or_else(|| model.loading.then(|| "Loading...".to_string()));
    let journal_rows = model.journal_entries.iter().map(|entry| {
        div()
            .h(px(18.0))
            .flex_none()
            .flex()
            .items_center()
            .px(px(6.0))
            .overflow_hidden()
            .whitespace_nowrap()
            .bg(rgb(theme::TOPBAR_BG))
            .text_xs()
            .text_color(rgb(theme::TEXT_DIM))
            .child(SharedString::from(format!(
                "{:?} - {} - {}",
                entry.actor, entry.result, entry.intent
            )))
            .into_any_element()
    });
    let address_text = if model.address_draft.is_empty() {
        "Enter an address".to_string()
    } else if model.address_focused {
        let cursor_byte = model
            .address_draft
            .char_indices()
            .nth(model.address_cursor)
            .map(|(index, _)| index)
            .unwrap_or(model.address_draft.len());
        format!(
            "{}|{}",
            &model.address_draft[..cursor_byte],
            &model.address_draft[cursor_byte..]
        )
    } else {
        model.address_draft.clone()
    };
    let page_bounds = actions.on_page_bounds.clone();
    let annotation_editor = model
        .annotation_draft
        .as_ref()
        .filter(|_| model.annotation_editor_visible())
        .map(|draft| {
            let cursor_byte = model
                .annotation_comment
                .char_indices()
                .nth(model.annotation_cursor)
                .map(|(index, _)| index)
                .unwrap_or(model.annotation_comment.len());
            let comment = if model.annotation_comment.is_empty() {
                "Add a required comment".to_string()
            } else if model.annotation_focused {
                format!(
                    "{}|{}",
                    &model.annotation_comment[..cursor_byte],
                    &model.annotation_comment[cursor_byte..]
                )
            } else {
                model.annotation_comment.clone()
            };
            let bounds = draft.candidate.bounds;
            div()
                .flex_none()
                .flex()
                .flex_col()
                .gap(px(4.0))
                .p(px(6.0))
                .border_b_1()
                .border_color(rgb(theme::BORDER_PRIMARY))
                .bg(rgb(theme::PANEL_HEADER_BG))
                .child(div().text_xs().text_color(rgb(theme::TEXT_MUTED)).child(
                    SharedString::from(format!(
                        "{:?} at {},{} {}x{} - screenshot {}",
                        draft.candidate.kind,
                        bounds.x,
                        bounds.y,
                        bounds.width,
                        bounds.height,
                        draft.screenshot_resource.0
                    )),
                ))
                .child(
                    div()
                        .h(px(28.0))
                        .flex()
                        .items_center()
                        .px(px(6.0))
                        .border_1()
                        .border_color(rgb(if model.annotation_focused {
                            theme::PRIMARY
                        } else {
                            theme::BORDER_PRIMARY
                        }))
                        .bg(rgb(theme::APP_BG))
                        .text_xs()
                        .text_color(rgb(if model.annotation_comment.is_empty() {
                            theme::TEXT_DIM
                        } else {
                            theme::TEXT_PRIMARY
                        }))
                        .track_focus(&annotation_focus)
                        .on_mouse_down(
                            MouseButton::Left,
                            action(BrowserPaneAction::FocusAnnotation),
                        )
                        .on_key_down(actions.on_annotation_key)
                        .child(SharedString::from(comment)),
                )
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(4.0))
                        .child(browser_button(
                            "Save",
                            false,
                            false,
                            action(BrowserPaneAction::SaveAnnotation),
                        ))
                        .child(browser_button(
                            "Cancel",
                            false,
                            true,
                            action(BrowserPaneAction::CancelAnnotation),
                        )),
                )
                .into_any_element()
        });
    let replay_panel = model.replay.as_ref().map(|projection| {
        let instance_id = projection.replay.instance_id;
        let current_step = if projection.replay.total_steps == 0 {
            0
        } else {
            projection
                .replay
                .current_step_index
                .saturating_add(1)
                .min(projection.replay.total_steps)
        };
        let unresolved = (!projection.replay.unresolved_secret_inputs.is_empty()).then(|| {
            div()
                .text_xs()
                .text_color(rgb(theme::WARNING_TEXT))
                .child(SharedString::from(format!(
                    "Secrets needed: {}",
                    projection.replay.unresolved_secret_inputs.join(", ")
                )))
        });
        let repair_summary = projection.repair.as_ref().map(|repair| {
            div()
                .text_xs()
                .text_color(rgb(theme::TEXT_MUTED))
                .child(SharedString::from(format!(
                    "Repair step {} ({:?}) - {:?} - snapshot and screenshot ready",
                    repair.step_index.saturating_add(1),
                    repair.locator_slot,
                    repair.phase
                )))
        });
        let repair_controls = projection.repair.as_ref().map(|repair| {
            let repair_id = repair.repair_id;
            let mut controls = vec![browser_button(
                if projection.selecting_replacement {
                    "Selecting replacement..."
                } else {
                    "Select replacement"
                },
                projection.selecting_replacement,
                false,
                action(BrowserPaneAction::BeginReplayRepairSelection {
                    instance_id,
                    repair_id,
                }),
            )
            .into_any_element()];
            if projection.repair_apply_ready
                && matches!(
                    repair.phase,
                    BrowserReplayRepairPhase::Previewed | BrowserReplayRepairPhase::Applied
                )
            {
                if repair.phase == BrowserReplayRepairPhase::Previewed {
                    controls.push(
                        browser_button(
                            "Save repair",
                            false,
                            false,
                            action(BrowserPaneAction::ApplyReplayRepair {
                                instance_id,
                                repair_id,
                                resume: false,
                            }),
                        )
                        .into_any_element(),
                    );
                }
                controls.push(
                    browser_button(
                        "Save and retry",
                        false,
                        false,
                        action(BrowserPaneAction::ApplyReplayRepair {
                            instance_id,
                            repair_id,
                            resume: true,
                        }),
                    )
                    .into_any_element(),
                );
            }
            div().flex().items_center().gap(px(4.0)).children(controls)
        });
        div()
            .flex_none()
            .flex()
            .flex_col()
            .gap(px(3.0))
            .p(px(6.0))
            .border_b_1()
            .border_color(rgb(theme::BORDER_PRIMARY))
            .bg(rgb(theme::PANEL_HEADER_BG))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.0))
                            .text_xs()
                            .text_color(rgb(theme::TEXT_PRIMARY))
                            .child(SharedString::from(format!(
                                "Replay {} - {:?} - step {current_step}/{}",
                                projection.replay.recipe_id,
                                projection.replay.status,
                                projection.replay.total_steps
                            ))),
                    )
                    .child(browser_button(
                        "Cancel replay",
                        false,
                        true,
                        action(BrowserPaneAction::CancelReplay { instance_id }),
                    )),
            )
            .children(unresolved)
            .children(repair_summary)
            .children(repair_controls)
            .into_any_element()
    });
    let replay_secret_prompt_panel = model.replay_secret_prompt.as_ref().map(|prompt| {
        let workspace_key = prompt.workspace_key.clone();
        let instance_id = prompt.instance_id;
        let input_rows = prompt
            .input_names
            .iter()
            .zip(prompt.is_set.iter().copied())
            .map(|(name, is_set)| {
                let input = BrowserReplaySecretPromptRenderInput {
                    name: name.as_str(),
                    is_set,
                };
                let focused = prompt.focused_input.as_deref() == Some(input.name);
                div()
                    .w_full()
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .p(px(5.0))
                    .border_1()
                    .border_color(rgb(if focused {
                        theme::PRIMARY
                    } else {
                        theme::BORDER_PRIMARY
                    }))
                    .bg(rgb(theme::APP_BG))
                    .on_mouse_down(
                        MouseButton::Left,
                        action(BrowserPaneAction::FocusReplaySecret {
                            workspace_key: workspace_key.clone(),
                            instance_id,
                            input_name: input.name.to_string(),
                        }),
                    )
                    .child(
                        div()
                            .w(px(140.0))
                            .text_xs()
                            .text_color(rgb(theme::TEXT_MUTED))
                            .child(SharedString::from(input.name.to_string())),
                    )
                    .child(
                        div()
                            .flex_1()
                            .text_sm()
                            .text_color(rgb(theme::TEXT_PRIMARY))
                            .child(SharedString::from(
                                browser_replay_secret_mask(input.is_set).to_string(),
                            )),
                    )
                    .into_any_element()
            });
        div()
            .flex_1()
            .min_h(px(0.0))
            .flex()
            .flex_col()
            .gap(px(8.0))
            .p(px(10.0))
            .bg(rgb(theme::PANEL_BG))
            .track_focus(&replay_secret_focus)
            .on_key_down(actions.on_replay_secret_key)
            .child(
                div()
                    .text_sm()
                    .text_color(rgb(theme::TEXT_PRIMARY))
                    .child("Enter replay secrets"),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(rgb(theme::TEXT_MUTED))
                    .child("Values stay in volatile native memory and are never shown."),
            )
            .children(input_rows)
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(4.0))
                    .child(browser_button(
                        "Submit",
                        false,
                        false,
                        action(BrowserPaneAction::SubmitReplaySecrets {
                            workspace_key: workspace_key.clone(),
                            instance_id,
                        }),
                    ))
                    .child(browser_button(
                        "Cancel",
                        false,
                        true,
                        action(BrowserPaneAction::CancelReplaySecrets {
                            workspace_key,
                            instance_id,
                        }),
                    )),
            )
            .into_any_element()
    });
    let workflow_controls = match model.workflow_review.as_ref().map(|review| &review.state) {
        Some(BrowserWorkflowReviewUiState::Recording { instance_id }) => vec![browser_button(
            "Stop Recording",
            true,
            true,
            action(BrowserPaneAction::StopRecording {
                instance_id: *instance_id,
            }),
        )
        .into_any_element()],
        Some(BrowserWorkflowReviewUiState::Review { instance_id }) => vec![browser_button(
            "Review",
            true,
            false,
            action(BrowserPaneAction::PreviewRecordingReview {
                instance_id: *instance_id,
            }),
        )
        .into_any_element()],
        Some(BrowserWorkflowReviewUiState::Inactive) => vec![browser_button(
            "Record",
            false,
            false,
            action(BrowserPaneAction::StartRecording),
        )
        .into_any_element()],
        None => Vec::new(),
    };
    let workflow_review = model
        .workflow_review
        .as_ref()
        .filter(|_| model.replay_secret_prompt.is_none());
    let workflow_review_panel = workflow_review.and_then(|review| {
        let BrowserWorkflowReviewUiState::Review { instance_id } = &review.state else {
            return None;
        };
        let instance_id = *instance_id;
        let metadata = review.metadata.as_ref()?;
        let workflow_control_group = || {
            div()
                .w_full()
                .flex()
                .flex_wrap()
                .items_center()
                .gap(px(4.0))
        };
        let metadata_rows = [
            (
                "Name",
                metadata.name.clone(),
                BrowserWorkflowReviewEditorField::Name,
            ),
            (
                "ID",
                metadata.id.clone(),
                BrowserWorkflowReviewEditorField::Id,
            ),
            (
                "Description",
                metadata.description.clone(),
                BrowserWorkflowReviewEditorField::Description,
            ),
            (
                "Start URL",
                metadata.start_url.clone(),
                BrowserWorkflowReviewEditorField::StartUrl,
            ),
        ]
        .into_iter()
        .map(|(label, value, field)| {
            workflow_control_group()
                .text_xs()
                .text_color(rgb(theme::TEXT_MUTED))
                .child(SharedString::from(format!("{label}: {value}")))
                .child(browser_button(
                    "edit",
                    false,
                    false,
                    action(BrowserPaneAction::FocusRecordingReviewField { instance_id, field }),
                ))
                .into_any_element()
        });
        let step_rows = review.steps.iter().map(|step| {
            let mut controls = Vec::new();
            if step.can_move_up {
                controls.push(
                    browser_button(
                        "up",
                        false,
                        false,
                        action(BrowserPaneAction::MutateRecordingReview {
                            instance_id,
                            mutation: BrowserWorkflowReviewMutation::MoveStep {
                                step_id: step.id.clone(),
                                new_index: step.index - 1,
                            },
                        }),
                    )
                    .into_any_element(),
                );
            }
            if step.can_move_down {
                controls.push(
                    browser_button(
                        "down",
                        false,
                        false,
                        action(BrowserPaneAction::MutateRecordingReview {
                            instance_id,
                            mutation: BrowserWorkflowReviewMutation::MoveStep {
                                step_id: step.id.clone(),
                                new_index: step.index + 1,
                            },
                        }),
                    )
                    .into_any_element(),
                );
            }
            if let Some(kind) = step.convertible_kind {
                controls.push(
                    browser_button(
                        match kind {
                            BrowserRecipeInputKind::Text => "to text input",
                            BrowserRecipeInputKind::Url => "to URL input",
                            BrowserRecipeInputKind::File => "to file input",
                            BrowserRecipeInputKind::Secret => "to secret input",
                        },
                        false,
                        false,
                        action(BrowserPaneAction::MutateRecordingReview {
                            instance_id,
                            mutation: BrowserWorkflowReviewMutation::ConvertActionValueToInput {
                                step_id: step.id.clone(),
                                input_name: format!("{}_input", step.id.replace('-', "_")),
                                kind,
                            },
                        }),
                    )
                    .into_any_element(),
                );
            }
            for (label, wait) in [
                (
                    "wait 1s",
                    BrowserRecipeWait::Duration { duration_ms: 1_000 },
                ),
                ("wait load", BrowserRecipeWait::Load { timeout_ms: 2_000 }),
                (
                    "wait idle",
                    BrowserRecipeWait::NetworkIdle { timeout_ms: 2_000 },
                ),
            ] {
                controls.push(
                    browser_button(
                        label,
                        false,
                        false,
                        action(BrowserPaneAction::MutateRecordingReview {
                            instance_id,
                            mutation: BrowserWorkflowReviewMutation::SetStepWait {
                                step_id: step.id.clone(),
                                wait: Some(wait),
                            },
                        }),
                    )
                    .into_any_element(),
                );
            }
            if step.has_wait {
                controls.push(
                    browser_button(
                        "remove wait",
                        false,
                        true,
                        action(BrowserPaneAction::MutateRecordingReview {
                            instance_id,
                            mutation: BrowserWorkflowReviewMutation::SetStepWait {
                                step_id: step.id.clone(),
                                wait: None,
                            },
                        }),
                    )
                    .into_any_element(),
                );
            }
            controls.push(
                browser_button(
                    "delete",
                    false,
                    true,
                    action(BrowserPaneAction::MutateRecordingReview {
                        instance_id,
                        mutation: BrowserWorkflowReviewMutation::DeleteStep {
                            step_id: step.id.clone(),
                        },
                    }),
                )
                .into_any_element(),
            );
            let mut assertion_buttons = [
                ("+ URL", BrowserWorkflowReviewAssertionKind::Url),
                ("+ title", BrowserWorkflowReviewAssertionKind::Title),
                ("+ text", BrowserWorkflowReviewAssertionKind::Text),
            ]
            .into_iter()
            .map(|(label, kind)| {
                browser_button(
                    label,
                    false,
                    false,
                    action(BrowserPaneAction::FocusRecordingReviewField {
                        instance_id,
                        field: BrowserWorkflowReviewEditorField::Assertion {
                            step_id: step.id.clone(),
                            kind,
                        },
                    }),
                )
                .into_any_element()
            })
            .collect::<Vec<_>>();
            if step.has_assertion_locator {
                assertion_buttons.push(
                    browser_button(
                        "+ element",
                        false,
                        false,
                        action(BrowserPaneAction::MutateRecordingReview {
                            instance_id,
                            mutation: BrowserWorkflowReviewMutation::AddStepAssertionDraft {
                                step_id: step.id.clone(),
                                kind: BrowserWorkflowReviewAssertionKind::Element,
                                expected: None,
                            },
                        }),
                    )
                    .into_any_element(),
                );
                assertion_buttons.push(
                    browser_button(
                        "+ value",
                        false,
                        false,
                        action(BrowserPaneAction::FocusRecordingReviewField {
                            instance_id,
                            field: BrowserWorkflowReviewEditorField::Assertion {
                                step_id: step.id.clone(),
                                kind: BrowserWorkflowReviewAssertionKind::Value,
                            },
                        }),
                    )
                    .into_any_element(),
                );
            }
            let remove_assertions = (0..step.assertion_count).map(|assertion_index| {
                browser_button(
                    format!("remove assertion {}", assertion_index + 1),
                    false,
                    true,
                    action(BrowserPaneAction::MutateRecordingReview {
                        instance_id,
                        mutation: BrowserWorkflowReviewMutation::RemoveStepAssertion {
                            step_id: step.id.clone(),
                            assertion_index,
                        },
                    }),
                )
                .into_any_element()
            });
            div()
                .flex()
                .flex_col()
                .gap(px(3.0))
                .p(px(4.0))
                .border_1()
                .border_color(rgb(theme::BORDER_PRIMARY))
                .child(
                    div()
                        .w_full()
                        .flex()
                        .flex_col()
                        .gap(px(3.0))
                        .text_xs()
                        .text_color(rgb(theme::TEXT_PRIMARY))
                        .child(SharedString::from(format!(
                            "{}  {:?}  {}",
                            step.id, step.actor, step.summary
                        )))
                        .child(workflow_control_group().children(controls)),
                )
                .child(
                    workflow_control_group()
                        .children(assertion_buttons)
                        .children(remove_assertions),
                )
                .into_any_element()
        });
        let input_rows = review.inputs.iter().map(|input| {
            let mut controls = vec![
                browser_button(
                    "rename",
                    false,
                    false,
                    action(BrowserPaneAction::FocusRecordingReviewField {
                        instance_id,
                        field: BrowserWorkflowReviewEditorField::InputName {
                            input_name: input.name.clone(),
                        },
                    }),
                )
                .into_any_element(),
                browser_button(
                    "remove",
                    false,
                    true,
                    action(BrowserPaneAction::MutateRecordingReview {
                        instance_id,
                        mutation: BrowserWorkflowReviewMutation::RemoveInput {
                            input_name: input.name.clone(),
                        },
                    }),
                )
                .into_any_element(),
            ];
            if matches!(
                input.kind,
                BrowserRecipeInputKind::Text | BrowserRecipeInputKind::Url
            ) {
                controls.insert(
                    1,
                    browser_button(
                        "edit default",
                        false,
                        false,
                        action(BrowserPaneAction::FocusRecordingReviewField {
                            instance_id,
                            field: BrowserWorkflowReviewEditorField::InputDefault {
                                input_name: input.name.clone(),
                            },
                        }),
                    )
                    .into_any_element(),
                );
            }
            div()
                .w_full()
                .flex()
                .flex_col()
                .gap(px(3.0))
                .text_xs()
                .text_color(rgb(theme::TEXT_MUTED))
                .child(SharedString::from(format!(
                    "Input {}  {:?}  {}",
                    input.name,
                    input.kind,
                    if input.unset { "unset" } else { "default set" }
                )))
                .child(workflow_control_group().children(controls))
                .into_any_element()
        });
        let editor = model.workflow_editor.as_ref().and_then(|editor| {
            if editor.instance_id != instance_id {
                return None;
            }
            let cursor_byte = editor
                .draft
                .char_indices()
                .nth(editor.cursor)
                .map(|(index, _)| index)
                .unwrap_or(editor.draft.len());
            let draft = format!(
                "{}|{}",
                &editor.draft[..cursor_byte],
                &editor.draft[cursor_byte..]
            );
            Some(
                workflow_control_group()
                    .p(px(4.0))
                    .border_1()
                    .border_color(rgb(theme::PRIMARY))
                    .track_focus(&workflow_focus)
                    .on_key_down(actions.on_workflow_key)
                    .child(SharedString::from(format!(
                        "Edit {:?}: {draft}",
                        editor.field
                    )))
                    .child(browser_button(
                        "cancel",
                        false,
                        true,
                        action(BrowserPaneAction::CancelRecordingReviewEdit),
                    )),
            )
        });
        Some(
            div()
                .flex_1()
                .min_h(px(0.0))
                .flex()
                .flex_col()
                .gap(px(6.0))
                .p(px(8.0))
                .id("browser-workflow-review-scroll")
                .overflow_y_scroll()
                .bg(rgb(theme::PANEL_BG))
                .child(
                    workflow_control_group()
                        .child("Recorded workflow review")
                        .child(browser_button(
                            "Preview",
                            false,
                            false,
                            action(BrowserPaneAction::PreviewRecordingReview { instance_id }),
                        ))
                        .child(browser_button(
                            "Save",
                            false,
                            false,
                            action(BrowserPaneAction::SaveRecordingReview { instance_id }),
                        ))
                        .child(browser_button(
                            "Discard",
                            false,
                            true,
                            action(BrowserPaneAction::DiscardRecordingReview { instance_id }),
                        )),
                )
                .children(editor)
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(3.0))
                        .children(metadata_rows)
                        .child(
                            div()
                                .w_full()
                                .flex()
                                .flex_col()
                                .gap(px(3.0))
                                .child(SharedString::from(format!(
                                    "Viewport {}x{} @ {}%",
                                    metadata.viewport.width,
                                    metadata.viewport.height,
                                    metadata.viewport.scale_percent
                                )))
                                .child(
                                    workflow_control_group().children(
                                        [
                                            BrowserViewportPreset::Desktop,
                                            BrowserViewportPreset::Tablet,
                                            BrowserViewportPreset::Mobile,
                                        ]
                                        .into_iter()
                                        .map(|preset| {
                                            browser_button(
                                                match preset {
                                                    BrowserViewportPreset::Desktop => "desktop",
                                                    BrowserViewportPreset::Tablet => "tablet",
                                                    BrowserViewportPreset::Mobile => "mobile",
                                                },
                                                metadata.viewport
                                                    == BrowserRecipeViewport::from(
                                                        preset.viewport(),
                                                    ),
                                                false,
                                                action(BrowserPaneAction::MutateRecordingReview {
                                                    instance_id,
                                                    mutation:
                                                        BrowserWorkflowReviewMutation::SetMetadata {
                                                            id: metadata.id.clone(),
                                                            name: metadata.name.clone(),
                                                            description: metadata
                                                                .description
                                                                .clone(),
                                                            start_url: metadata.start_url.clone(),
                                                            viewport: preset.viewport().into(),
                                                        },
                                                }),
                                            )
                                            .into_any_element()
                                        }),
                                    ),
                                ),
                        ),
                )
                .child(div().flex().flex_col().gap(px(3.0)).children(step_rows))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(3.0))
                        .child(
                            workflow_control_group().child("Inputs").children(
                                [
                                    ("add text input", BrowserRecipeInputKind::Text, "text"),
                                    ("add URL input", BrowserRecipeInputKind::Url, "url"),
                                    ("add file input", BrowserRecipeInputKind::File, "file"),
                                    ("add secret input", BrowserRecipeInputKind::Secret, "secret"),
                                ]
                                .into_iter()
                                .filter_map(
                                    |(label, kind, prefix)| {
                                        let input_name = browser_workflow_next_input_name(
                                            &review.inputs,
                                            prefix,
                                        )?;
                                        Some(
                                            browser_button(
                                                label,
                                                false,
                                                false,
                                                action(BrowserPaneAction::MutateRecordingReview {
                                                    instance_id,
                                                    mutation:
                                                        BrowserWorkflowReviewMutation::AddInput {
                                                            input: BrowserRecipeInput {
                                                                name: input_name,
                                                                kind,
                                                                default_value: None,
                                                            },
                                                        },
                                                }),
                                            )
                                            .into_any_element(),
                                        )
                                    },
                                ),
                            ),
                        )
                        .children(input_rows),
                )
                .children(model.workflow_preview.as_ref().map(|preview| {
                    div()
                        .flex_1()
                        .min_h(px(0.0))
                        .id("browser-workflow-preview-scroll")
                        .overflow_y_scroll()
                        .p(px(6.0))
                        .border_1()
                        .border_color(rgb(theme::BORDER_PRIMARY))
                        .bg(rgb(theme::APP_BG))
                        .text_xs()
                        .text_color(rgb(theme::TEXT_MUTED))
                        .child(SharedString::from(preview.clone()))
                }))
                .into_any_element(),
        )
    });
    let page_surface = show_page_surface.then(|| {
        div()
            .flex_1()
            .min_h(px(0.0))
            .relative()
            .bg(rgb(theme::TERMINAL_BG))
            .child(
                canvas(
                    move |bounds, window, cx| {
                        (page_bounds)(bounds, window, cx);
                    },
                    |_, _, _, _| {},
                )
                .size_full(),
            )
            .into_any_element()
    });

    div()
        .h_full()
        .flex()
        .flex_col()
        .overflow_hidden()
        .bg(rgb(theme::PANEL_BG))
        .border_l_1()
        .border_color(rgb(theme::BORDER_PRIMARY))
        .child(
            div()
                .h(px(26.0))
                .flex_none()
                .flex()
                .items_center()
                .overflow_hidden()
                .bg(rgb(theme::TOPBAR_BG))
                .border_b_1()
                .border_color(rgb(theme::BORDER_PRIMARY))
                .children(tab_strip)
                .child(browser_button(
                    "+",
                    false,
                    false,
                    action(BrowserPaneAction::CreateTab),
                ))
                .child(browser_button(
                    "collapse",
                    false,
                    false,
                    action(BrowserPaneAction::Collapse),
                )),
        )
        .child(
            div()
                .h(px(30.0))
                .flex_none()
                .flex()
                .items_center()
                .gap(px(3.0))
                .px(px(4.0))
                .bg(rgb(theme::PANEL_HEADER_BG))
                .border_b_1()
                .border_color(rgb(theme::BORDER_PRIMARY))
                .child(browser_button(
                    "back",
                    false,
                    false,
                    action(BrowserPaneAction::Back),
                ))
                .child(browser_button(
                    "forward",
                    false,
                    false,
                    action(BrowserPaneAction::Forward),
                ))
                .child(browser_button(
                    "reload",
                    false,
                    false,
                    action(BrowserPaneAction::Reload),
                ))
                .child(
                    div()
                        .flex_1()
                        .min_w(px(60.0))
                        .h(px(22.0))
                        .flex()
                        .items_center()
                        .px(px(6.0))
                        .border_1()
                        .border_color(rgb(if model.address_focused {
                            theme::PRIMARY
                        } else {
                            theme::BORDER_PRIMARY
                        }))
                        .bg(rgb(theme::APP_BG))
                        .text_xs()
                        .text_color(rgb(if model.address_draft.is_empty() {
                            theme::TEXT_DIM
                        } else {
                            theme::TEXT_PRIMARY
                        }))
                        .track_focus(&address_focus)
                        .on_mouse_down(MouseButton::Left, action(BrowserPaneAction::FocusAddress))
                        .on_key_down(actions.on_address_key)
                        .child(SharedString::from(address_text)),
                )
                .child(browser_button(
                    "go",
                    false,
                    false,
                    action(BrowserPaneAction::SubmitAddress),
                )),
        )
        .child(
            div()
                .h(px(28.0))
                .flex_none()
                .flex()
                .items_center()
                .gap(px(3.0))
                .px(px(4.0))
                .overflow_hidden()
                .bg(rgb(theme::PANEL_HEADER_BG))
                .border_b_1()
                .border_color(rgb(theme::BORDER_PRIMARY))
                .children(
                    [
                        BrowserViewportPreset::Desktop,
                        BrowserViewportPreset::Tablet,
                        BrowserViewportPreset::Mobile,
                    ]
                    .into_iter()
                    .map(|preset| {
                        browser_button(
                            match preset {
                                BrowserViewportPreset::Desktop => "desktop",
                                BrowserViewportPreset::Tablet => "tablet",
                                BrowserViewportPreset::Mobile => "mobile",
                            },
                            selected_viewport.as_ref() == Some(&preset.viewport()),
                            false,
                            action(BrowserPaneAction::SetViewport(preset)),
                        )
                        .into_any_element()
                    }),
                )
                .child(browser_button(
                    "annotate",
                    model.annotation_mode,
                    false,
                    action(BrowserPaneAction::ToggleAnnotation),
                ))
                .children(workflow_controls)
                .child(browser_button(
                    "devtools",
                    false,
                    false,
                    action(BrowserPaneAction::OpenDevTools),
                ))
                .child(browser_button(
                    "downloads",
                    false,
                    false,
                    action(BrowserPaneAction::OpenDownloads),
                ))
                .child(browser_button(
                    "Stop",
                    false,
                    true,
                    action(BrowserPaneAction::Stop),
                )),
        )
        .child(
            div()
                .h(px(22.0))
                .flex_none()
                .flex()
                .items_center()
                .px(px(6.0))
                .bg(rgb(theme::TOPBAR_BG))
                .border_b_1()
                .border_color(rgb(theme::BORDER_PRIMARY))
                .text_xs()
                .text_color(rgb(if model.diagnostic.is_some() {
                    theme::DANGER_TEXT
                } else if model.loading {
                    theme::WARNING_TEXT
                } else {
                    theme::TEXT_DIM
                }))
                .child(SharedString::from(status.unwrap_or_else(|| {
                    selected_viewport
                        .map(viewport_label)
                        .unwrap_or_else(|| "Browser ready".to_string())
                }))),
        )
        .children(replay_panel)
        .child(div().flex_none().flex().flex_col().children(journal_rows))
        .children(annotation_editor)
        .children(replay_secret_prompt_panel)
        .children(workflow_review_panel)
        .children(page_surface)
}

fn browser_button(
    label: impl Into<SharedString>,
    active: bool,
    danger: bool,
    on_click: Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)>,
) -> impl IntoElement {
    div()
        .flex_none()
        .px(px(5.0))
        .py(px(3.0))
        .rounded(px(2.0))
        .bg(rgb(if active {
            theme::PRIMARY_MUTED
        } else {
            theme::TOPBAR_BG
        }))
        .hover(|style| style.bg(rgb(theme::BUTTON_HOVER_BG)))
        .text_xs()
        .text_color(rgb(if danger {
            theme::DANGER_TEXT
        } else if active {
            theme::TEXT_PRIMARY
        } else {
            theme::TEXT_MUTED
        }))
        .on_mouse_down(MouseButton::Left, on_click)
        .child(label.into())
}

fn browser_tab_label(tab: &BrowserTabSnapshot) -> String {
    if !tab.title.trim().is_empty() {
        return tab.title.trim().to_string();
    }
    if tab.url.eq_ignore_ascii_case("about:blank") {
        return "New tab".to_string();
    }
    tab.url
        .split_once("://")
        .map(|(_, rest)| rest.split(['/', '?', '#']).next().unwrap_or(rest))
        .filter(|host| !host.is_empty())
        .unwrap_or(tab.url.as_str())
        .to_string()
}

fn viewport_label(viewport: BrowserViewport) -> String {
    format!("Viewport {}x{}", viewport.width, viewport.height)
}
