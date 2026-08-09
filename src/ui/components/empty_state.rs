//! Bounded empty state with explicitly typed recovery actions.

use super::button::Button;
use super::interaction::{
    redacted_bounded_text, AccessibilityMetadata, AccessibleRole, ActionEvent, ActionRequest,
    ComponentError, FocusEpoch, KeyboardKey, MAX_ACCESSIBLE_DESCRIPTION_SCALARS,
    MAX_ACCESSIBLE_NAME_SCALARS, MAX_RECOVERY_ACTIONS,
};

pub struct RecoveryAction {
    button: Button,
}

impl RecoveryAction {
    pub fn new(
        label: impl Into<String>,
        action_request: ActionRequest,
    ) -> Result<Self, ComponentError> {
        let label = redacted_bounded_text(
            "recovery action label",
            label,
            MAX_ACCESSIBLE_NAME_SCALARS,
            MAX_ACCESSIBLE_NAME_SCALARS * 4,
        )?;
        Ok(Self {
            button: Button::new_variant(
                label,
                super::button::ButtonVariant::Secondary,
                action_request,
            )?,
        })
    }

    pub fn label(&self) -> &str {
        self.button.label()
    }

    pub fn action_request(&self) -> &ActionRequest {
        self.button.action_request()
    }

    pub fn set_focus_epoch(&mut self, focus_epoch: FocusEpoch) {
        self.button.set_focus_epoch(focus_epoch);
    }

    pub fn focus(&mut self) -> bool {
        self.button.focus()
    }

    pub fn blur(&mut self) {
        self.button.blur();
    }

    pub fn accessibility(&self) -> &super::interaction::AccessibilityMetadata {
        self.button.accessibility()
    }

    pub fn key_activate(&self, key: KeyboardKey, focus_epoch: FocusEpoch) -> Option<ActionEvent> {
        self.button.key_activate(key, focus_epoch)
    }

    pub fn pointer_down(&mut self, pointer_id: u64, focus_epoch: FocusEpoch) -> bool {
        self.button.pointer_down(pointer_id, focus_epoch)
    }

    pub fn pointer_up(&mut self, pointer_id: u64, focus_epoch: FocusEpoch) -> Option<ActionEvent> {
        self.button.pointer_up(pointer_id, focus_epoch)
    }

    pub fn activate(&self, focus_epoch: FocusEpoch) -> Option<ActionEvent> {
        self.button.key_activate(KeyboardKey::Enter, focus_epoch)
    }
}

pub struct EmptyState {
    title: String,
    description: String,
    recovery_actions: Vec<RecoveryAction>,
    accessibility: AccessibilityMetadata,
}

impl EmptyState {
    pub fn new(
        title: impl Into<String>,
        description: impl Into<String>,
    ) -> Result<Self, ComponentError> {
        let title = redacted_bounded_text(
            "empty state title",
            title,
            MAX_ACCESSIBLE_NAME_SCALARS,
            MAX_ACCESSIBLE_NAME_SCALARS * 4,
        )?;
        let description = redacted_bounded_text(
            "empty state description",
            description,
            MAX_ACCESSIBLE_DESCRIPTION_SCALARS,
            MAX_ACCESSIBLE_DESCRIPTION_SCALARS * 4,
        )?;
        let mut accessibility = AccessibilityMetadata::new(AccessibleRole::Region, title.clone())?;
        accessibility.set_description(description.clone())?;
        Ok(Self {
            title,
            description,
            recovery_actions: Vec::new(),
            accessibility,
        })
    }

    pub fn with_recovery_action(mut self, action: RecoveryAction) -> Result<Self, ComponentError> {
        if self.recovery_actions.len() >= MAX_RECOVERY_ACTIONS {
            return Err(ComponentError::TooManyRecoveryActions {
                max: MAX_RECOVERY_ACTIONS,
                actual: self.recovery_actions.len() + 1,
            });
        }
        self.recovery_actions.push(action);
        Ok(self)
    }

    pub fn with_recovery_actions(
        mut self,
        actions: impl IntoIterator<Item = RecoveryAction>,
    ) -> Result<Self, ComponentError> {
        for action in actions {
            self = self.with_recovery_action(action)?;
        }
        Ok(self)
    }

    pub fn title(&self) -> &str {
        &self.title
    }

    pub fn description(&self) -> &str {
        &self.description
    }

    pub fn recovery_actions(&self) -> &[RecoveryAction] {
        &self.recovery_actions
    }

    pub fn set_focus_epoch(&mut self, focus_epoch: FocusEpoch) {
        for action in &mut self.recovery_actions {
            action.set_focus_epoch(focus_epoch);
        }
    }

    pub fn focus_recovery(&mut self, index: usize) -> bool {
        self.recovery_actions
            .get_mut(index)
            .map(RecoveryAction::focus)
            .unwrap_or(false)
    }

    pub fn blur_recovery(&mut self, index: usize) {
        if let Some(action) = self.recovery_actions.get_mut(index) {
            action.blur();
        }
    }

    pub fn pointer_down_recovery(
        &mut self,
        index: usize,
        pointer_id: u64,
        focus_epoch: FocusEpoch,
    ) -> bool {
        self.recovery_actions
            .get_mut(index)
            .map(|action| action.pointer_down(pointer_id, focus_epoch))
            .unwrap_or(false)
    }

    pub fn pointer_up_recovery(
        &mut self,
        index: usize,
        pointer_id: u64,
        focus_epoch: FocusEpoch,
    ) -> Option<ActionEvent> {
        self.recovery_actions
            .get_mut(index)
            .and_then(|action| action.pointer_up(pointer_id, focus_epoch))
    }

    pub fn key_activate_recovery(
        &self,
        index: usize,
        key: KeyboardKey,
        focus_epoch: FocusEpoch,
    ) -> Option<ActionEvent> {
        self.recovery_actions
            .get(index)
            .and_then(|action| action.key_activate(key, focus_epoch))
    }

    pub fn activate_recovery(&self, index: usize, focus_epoch: FocusEpoch) -> Option<ActionEvent> {
        self.key_activate_recovery(index, KeyboardKey::Enter, focus_epoch)
    }

    pub fn accessibility(&self) -> &AccessibilityMetadata {
        &self.accessibility
    }

    pub fn rendered_payload(&self) -> String {
        let actions = self
            .recovery_actions
            .iter()
            .map(RecoveryAction::label)
            .collect::<Vec<_>>()
            .join(", ");
        if actions.is_empty() {
            format!("{}: {}", self.title, self.description)
        } else {
            format!("{}: {} ({actions})", self.title, self.description)
        }
    }
}
