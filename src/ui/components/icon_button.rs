//! Accessible icon-only control boundary.

use super::button::{Button, ButtonVariant};
use super::interaction::{
    AccessibilityMetadata, ActionEvent, ActionId, ComponentError, ControlPresentation, KeyboardKey,
};
use crate::ui::tokens::ThemeTokens;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TooltipContract {
    pub label: String,
    pub delay_ms: u16,
}

impl TooltipContract {
    pub fn new(label: impl Into<String>, delay_ms: u16) -> Result<Self, ComponentError> {
        if delay_ms == 0 {
            return Err(ComponentError::InvalidLimit("tooltip delay"));
        }
        Ok(Self {
            label: super::interaction::bounded_text(
                "tooltip label",
                label,
                super::interaction::MAX_ACCESSIBLE_DESCRIPTION_SCALARS,
                super::interaction::MAX_ACCESSIBLE_DESCRIPTION_SCALARS * 4,
            )?,
            delay_ms,
        })
    }
}

pub struct IconButton {
    icon: String,
    button: Button,
    tooltip: TooltipContract,
}

impl IconButton {
    pub fn new<F>(
        icon: impl Into<String>,
        accessible_label: impl Into<String>,
        tooltip: TooltipContract,
        action_id: ActionId,
        callback: F,
    ) -> Result<Self, ComponentError>
    where
        F: Fn(ActionEvent) + Send + Sync + 'static,
    {
        let icon = super::interaction::bounded_text("icon name", icon, 96, 384)?;
        if tooltip.label.trim().is_empty() {
            return Err(ComponentError::MissingTooltip);
        }
        let mut button = Button::new(accessible_label, action_id, callback)?;
        button.set_accessibility_description(tooltip.label.clone())?;
        Ok(Self {
            icon,
            button,
            tooltip,
        })
    }

    pub fn new_variant<F>(
        icon: impl Into<String>,
        accessible_label: impl Into<String>,
        tooltip: TooltipContract,
        variant: ButtonVariant,
        action_id: ActionId,
        callback: F,
    ) -> Result<Self, ComponentError>
    where
        F: Fn(ActionEvent) + Send + Sync + 'static,
    {
        let icon = super::interaction::bounded_text("icon name", icon, 96, 384)?;
        if tooltip.label.trim().is_empty() {
            return Err(ComponentError::MissingTooltip);
        }
        let mut button = Button::new_variant(accessible_label, variant, action_id, callback)?;
        button.set_accessibility_description(tooltip.label.clone())?;
        Ok(Self {
            icon,
            button,
            tooltip,
        })
    }

    pub fn icon(&self) -> &str {
        &self.icon
    }

    pub fn tooltip(&self) -> &TooltipContract {
        &self.tooltip
    }

    pub fn interaction_state(&self) -> super::interaction::InteractionState {
        self.button.interaction_state()
    }

    pub fn accessibility(&self) -> &AccessibilityMetadata {
        self.button.accessibility()
    }

    pub fn disabled_reason(&self) -> Option<&str> {
        self.button.disabled_reason()
    }

    pub fn disable(&mut self, reason: impl Into<String>) -> Result<(), ComponentError> {
        self.button.disable(reason)
    }

    pub fn enable(&mut self) -> Result<(), ComponentError> {
        self.button.enable()
    }

    pub fn set_loading(&mut self, loading: bool) -> Result<(), ComponentError> {
        self.button.set_loading(loading)
    }

    pub fn focus(&mut self) -> bool {
        self.button.focus()
    }

    pub fn blur(&mut self) {
        self.button.blur();
    }

    pub fn set_focus_epoch(&mut self, focus_epoch: u64) {
        self.button.set_focus_epoch(focus_epoch);
    }

    pub fn pointer_down(&mut self, pointer_id: u64, focus_epoch: u64) -> bool {
        self.button.pointer_down(pointer_id, focus_epoch)
    }

    pub fn pointer_up(&mut self, pointer_id: u64, focus_epoch: u64) -> bool {
        self.button.pointer_up(pointer_id, focus_epoch)
    }

    pub fn key_activate(&self, key: KeyboardKey, focus_epoch: u64) -> bool {
        self.button.key_activate(key, focus_epoch)
    }

    pub fn presentation(&self, tokens: ThemeTokens) -> ControlPresentation {
        self.button.presentation(tokens)
    }
}
