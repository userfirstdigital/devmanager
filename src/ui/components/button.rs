//! DevManager-owned actionable button model.

use super::interaction::{
    control_presentation, AccessibilityMetadata, AccessibleRole, ActionEvent, ActionRequest,
    ActivationSource, ComponentError, ControlPresentation, FocusEpoch, InteractionState,
    InteractionStateModel, InteractionTransition, KeyboardKey,
};
use crate::ui::tokens::ThemeTokens;
use gpui::{div, px, rgb, IntoElement, ParentElement, Styled};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ButtonVariant {
    Primary,
    Secondary,
    Quiet,
    Destructive,
}

pub struct Button {
    label: String,
    action_request: ActionRequest,
    variant: ButtonVariant,
    interaction: InteractionStateModel,
    accessibility: AccessibilityMetadata,
    disabled_reason: Option<String>,
}

impl Button {
    pub fn new(
        label: impl Into<String>,
        action_request: ActionRequest,
    ) -> Result<Self, ComponentError> {
        Self::new_variant(label, ButtonVariant::Primary, action_request)
    }

    pub fn new_variant(
        label: impl Into<String>,
        variant: ButtonVariant,
        action_request: ActionRequest,
    ) -> Result<Self, ComponentError> {
        let label = super::interaction::redacted_bounded_text(
            "button label",
            label,
            super::interaction::MAX_ACCESSIBLE_NAME_SCALARS,
            super::interaction::MAX_ACCESSIBLE_NAME_SCALARS * 4,
        )?;
        let mut interaction = InteractionStateModel::default();
        if variant == ButtonVariant::Destructive {
            interaction.set_destructive(true)?;
        }
        Ok(Self {
            accessibility: AccessibilityMetadata::new(AccessibleRole::Button, label.clone())?,
            label,
            action_request,
            variant,
            interaction,
            disabled_reason: None,
        })
    }

    pub fn label(&self) -> &str {
        &self.label
    }

    pub fn action_request(&self) -> &ActionRequest {
        &self.action_request
    }

    pub fn variant(&self) -> ButtonVariant {
        self.variant
    }

    pub fn interaction_state(&self) -> InteractionState {
        self.interaction.state()
    }

    pub fn accessibility(&self) -> &AccessibilityMetadata {
        &self.accessibility
    }

    pub(crate) fn set_accessibility_description(
        &mut self,
        description: impl Into<String>,
    ) -> Result<(), ComponentError> {
        self.accessibility.set_description(description)
    }

    pub fn disabled_reason(&self) -> Option<&str> {
        self.disabled_reason.as_deref()
    }

    pub fn set_focus_epoch(&mut self, focus_epoch: FocusEpoch) {
        self.interaction.set_focus_epoch(focus_epoch);
        self.sync_accessibility();
    }

    pub fn focus(&mut self) -> bool {
        let focused = self.interaction.focus();
        self.sync_accessibility();
        focused
    }

    pub fn blur(&mut self) {
        self.interaction.blur();
        self.sync_accessibility();
    }

    pub fn set_hovered(&mut self, hovered: bool) -> Result<(), ComponentError> {
        self.interaction.transition(if hovered {
            InteractionTransition::Hover
        } else {
            InteractionTransition::Unhover
        })?;
        self.sync_accessibility();
        Ok(())
    }

    pub fn disable(&mut self, reason: impl Into<String>) -> Result<(), ComponentError> {
        let reason = super::interaction::redacted_bounded_text(
            "disabled reason",
            reason,
            super::interaction::MAX_ACCESSIBLE_DESCRIPTION_SCALARS,
            super::interaction::MAX_ACCESSIBLE_DESCRIPTION_SCALARS * 4,
        )?;
        self.interaction.set_disabled(true);
        self.accessibility.set_description(reason.clone())?;
        self.disabled_reason = Some(reason);
        self.sync_accessibility();
        Ok(())
    }

    pub fn enable(&mut self) -> Result<(), ComponentError> {
        self.interaction.set_disabled(false);
        self.disabled_reason = None;
        self.accessibility.clear_description();
        self.sync_accessibility();
        Ok(())
    }

    pub fn set_loading(&mut self, loading: bool) -> Result<(), ComponentError> {
        self.interaction.set_loading(loading)?;
        self.sync_accessibility();
        Ok(())
    }

    pub(crate) fn set_pressed_for_preview(&mut self) {
        let _ = self.interaction.transition(InteractionTransition::Press);
    }

    pub(crate) fn set_destructive_for_preview(&mut self) {
        let _ = self.interaction.set_destructive(true);
    }

    pub fn pointer_down(&mut self, pointer_id: u64, focus_epoch: FocusEpoch) -> bool {
        self.interaction.pointer_down(pointer_id, focus_epoch)
    }

    pub fn pointer_up(&mut self, pointer_id: u64, focus_epoch: FocusEpoch) -> Option<ActionEvent> {
        let activated = self.interaction.pointer_up(pointer_id, focus_epoch);
        activated.then(|| {
            ActionEvent::new(
                self.action_request.clone(),
                ActivationSource::Pointer { pointer_id },
                focus_epoch,
            )
        })
    }

    pub fn key_activate(&self, key: KeyboardKey, focus_epoch: FocusEpoch) -> Option<ActionEvent> {
        if !self.interaction.key_activate(key.clone(), focus_epoch) {
            return None;
        }
        Some(ActionEvent::new(
            self.action_request.clone(),
            ActivationSource::Keyboard { key },
            focus_epoch,
        ))
    }

    pub fn presentation(&self, tokens: ThemeTokens) -> ControlPresentation {
        match self.variant {
            ButtonVariant::Primary | ButtonVariant::Destructive => control_presentation(
                tokens,
                self.interaction.state(),
                self.variant == ButtonVariant::Destructive,
            ),
            ButtonVariant::Secondary | ButtonVariant::Quiet => {
                super::interaction::neutral_control_presentation(tokens, self.interaction.state())
            }
        }
    }

    pub fn disabled(&self) -> bool {
        self.interaction.state().is_disabled()
    }

    /// Render the production button surface from the same interaction and
    /// token presentation used by the shell.  Preview/gallery callers use
    /// this element directly so visual evidence cannot drift into a hand-
    /// styled `div` duplicate.
    pub fn element(&self, tokens: ThemeTokens) -> impl IntoElement {
        let presentation = self.presentation(tokens);
        let mut element = div()
            .flex()
            .items_center()
            .justify_center()
            .gap(px(tokens.density.spacing.xs))
            .px(px(tokens.density.controls.control_padding))
            .py(px(tokens.density.spacing.xs))
            .rounded_md()
            .border_1()
            .border_color(rgb(presentation.border.to_u32()))
            .bg(rgb(presentation.background.to_u32()))
            .text_color(rgb(presentation.foreground.to_u32()))
            .child(self.label.clone());
        if let Some(focus_ring) = presentation.focus_ring {
            element = element.border_color(rgb(focus_ring.color.to_u32()));
        }
        if !presentation.disabled {
            element = element.cursor_pointer();
        }
        element
    }

    pub fn loading(&self) -> bool {
        self.interaction.state().is_loading()
    }

    fn sync_accessibility(&mut self) {
        let state = self.interaction.state();
        self.accessibility.set_disabled(state.is_disabled());
        self.accessibility.set_busy(state.is_loading());
        self.accessibility.set_focused(state.focused());
    }
}
