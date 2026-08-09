//! Bounded empty state with explicitly typed recovery actions.

use super::button::Button;
use super::interaction::{
    AccessibilityMetadata, AccessibleRole, ActionEvent, ActionRequest, ComponentError, KeyboardKey,
    MAX_RECOVERY_ACTIONS,
};

pub struct RecoveryAction {
    button: Button,
}

impl RecoveryAction {
    pub fn new(
        label: impl Into<String>,
        action_request: ActionRequest,
    ) -> Result<Self, ComponentError> {
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

    pub fn set_focus_epoch(&mut self, focus_epoch: u64) {
        self.button.set_focus_epoch(focus_epoch);
    }

    pub fn accessibility(&self) -> &super::interaction::AccessibilityMetadata {
        self.button.accessibility()
    }

    pub fn key_activate(&self, key: KeyboardKey, focus_epoch: u64) -> Option<ActionEvent> {
        self.button.key_activate(key, focus_epoch)
    }

    pub fn activate(&self, focus_epoch: u64) -> Option<ActionEvent> {
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
        let title = super::interaction::bounded_text("empty state title", title, 256, 1024)?;
        let description =
            super::interaction::bounded_text("empty state description", description, 512, 2048)?;
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

    pub fn activate_recovery(&self, index: usize, focus_epoch: u64) -> Option<ActionEvent> {
        self.recovery_actions
            .get(index)
            .map(|action| action.activate(focus_epoch))
            .flatten()
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
