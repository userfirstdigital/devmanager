//! Safe error presentation with explicitly typed recovery actions.

use super::empty_state::RecoveryAction;
use super::interaction::{
    AccessibilityMetadata, AccessibleRole, ComponentError, MAX_RECOVERY_ACTIONS,
};

pub struct ErrorBoundary {
    title: String,
    message: String,
    recovery_actions: Vec<RecoveryAction>,
    accessibility: AccessibilityMetadata,
}

impl ErrorBoundary {
    pub fn new(
        title: impl Into<String>,
        message: impl Into<String>,
    ) -> Result<Self, ComponentError> {
        let title = super::interaction::bounded_text("error title", title, 256, 1024)?;
        let message = super::interaction::bounded_text("error message", message, 512, 2048)?;
        let mut accessibility = AccessibilityMetadata::new(AccessibleRole::Alert, title.clone())?;
        accessibility.set_description(message.clone())?;
        accessibility.invalid = true;
        Ok(Self {
            title,
            message,
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

    pub fn message(&self) -> &str {
        &self.message
    }

    pub fn recovery_actions(&self) -> &[RecoveryAction] {
        &self.recovery_actions
    }

    pub fn activate_recovery(&self, index: usize, focus_epoch: u64) -> bool {
        self.recovery_actions
            .get(index)
            .map(|action| action.activate(focus_epoch))
            .unwrap_or(false)
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
            format!("{}: {}", self.title, self.message)
        } else {
            format!("{}: {} ({actions})", self.title, self.message)
        }
    }
}
