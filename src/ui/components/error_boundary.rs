//! Safe error presentation with explicitly typed recovery actions.

use super::empty_state::RecoveryAction;
use super::interaction::{
    redacted_bounded_text, AccessibilityMetadata, AccessibleRole, ActionEvent, ComponentError,
    KeyboardKey, MAX_ACCESSIBLE_DESCRIPTION_SCALARS, MAX_ACCESSIBLE_NAME_SCALARS,
    MAX_RECOVERY_ACTIONS,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SafeErrorCode {
    HostUnavailable,
    InvalidProjection,
    RendererFailure,
    Unknown,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SafeErrorProjection {
    code: SafeErrorCode,
    title: String,
    message: String,
}

impl SafeErrorProjection {
    pub fn new(
        code: SafeErrorCode,
        title: impl Into<String>,
        message: impl Into<String>,
    ) -> Result<Self, ComponentError> {
        let title = redacted_bounded_text(
            "error title",
            title,
            MAX_ACCESSIBLE_NAME_SCALARS,
            MAX_ACCESSIBLE_NAME_SCALARS * 4,
        )?;
        let message = redacted_bounded_text(
            "error message",
            message,
            MAX_ACCESSIBLE_DESCRIPTION_SCALARS,
            MAX_ACCESSIBLE_DESCRIPTION_SCALARS * 4,
        )?;
        Ok(Self {
            code,
            title,
            message,
        })
    }

    pub fn code(&self) -> SafeErrorCode {
        self.code
    }

    pub fn title(&self) -> &str {
        &self.title
    }

    pub fn message(&self) -> &str {
        &self.message
    }
}

pub struct ErrorBoundary {
    projection: SafeErrorProjection,
    recovery_actions: Vec<RecoveryAction>,
    accessibility: AccessibilityMetadata,
}

impl ErrorBoundary {
    pub fn new(projection: SafeErrorProjection) -> Result<Self, ComponentError> {
        let mut accessibility =
            AccessibilityMetadata::new(AccessibleRole::Alert, projection.title.clone())?;
        accessibility.set_description(projection.message.clone())?;
        accessibility.invalid = true;
        Ok(Self {
            projection,
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
        self.projection.title()
    }

    pub fn message(&self) -> &str {
        self.projection.message()
    }

    pub fn projection(&self) -> &SafeErrorProjection {
        &self.projection
    }

    pub fn recovery_actions(&self) -> &[RecoveryAction] {
        &self.recovery_actions
    }

    pub fn set_focus_epoch(&mut self, focus_epoch: u64) {
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
        focus_epoch: u64,
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
        focus_epoch: u64,
    ) -> Option<ActionEvent> {
        self.recovery_actions
            .get_mut(index)
            .and_then(|action| action.pointer_up(pointer_id, focus_epoch))
    }

    pub fn key_activate_recovery(
        &self,
        index: usize,
        key: KeyboardKey,
        focus_epoch: u64,
    ) -> Option<ActionEvent> {
        self.recovery_actions
            .get(index)
            .and_then(|action| action.key_activate(key, focus_epoch))
    }

    pub fn activate_recovery(&self, index: usize, focus_epoch: u64) -> Option<ActionEvent> {
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
            format!("{}: {}", self.title(), self.message())
        } else {
            format!("{}: {} ({actions})", self.title(), self.message())
        }
    }
}
