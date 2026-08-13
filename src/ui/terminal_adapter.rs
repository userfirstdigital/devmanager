//! The one terminal integration seam owned by the native Task Cockpit.
//!
//! The established renderer remains the raw-terminal implementation.  This
//! adapter either delegates to that renderer with a complete
//! [`TerminalPaneModel`] or renders an honest, typed unavailable state.  It
//! never clones terminal cells, translates terminal events, or falls back to a
//! WebView.

use crate::terminal::view::{render_terminal_surface, TerminalPaneModel};
use crate::ui::tokens::RuntimePreferencesSnapshot;
use gpui::{
    div, px, App, Component, InteractiveElement, IntoElement, ParentElement, RenderOnce, Styled,
    Window,
};

pub const TERMINAL_ADAPTER_DEPENDENCY: &str =
    "src/terminal/view.rs::render_terminal_surface(TerminalPaneModel, TerminalPaneActions)";

const TERMINAL_UNAVAILABLE_MESSAGE: &str = "No task-bound native terminal view has been admitted yet. The dock will render the established terminal surface as soon as the exact task/resource generation is available.";

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TerminalDockState {
    live: bool,
    message: String,
}

impl TerminalDockState {
    pub fn unavailable() -> Self {
        Self {
            live: false,
            message: TERMINAL_UNAVAILABLE_MESSAGE.to_string(),
        }
    }

    fn live() -> Self {
        Self {
            live: true,
            message: String::new(),
        }
    }

    pub fn is_live(&self) -> bool {
        self.live
    }

    pub fn message(&self) -> &str {
        &self.message
    }
}

#[derive(Debug, Default)]
pub struct TerminalDockAdapter {
    model: Option<TerminalPaneModel>,
    preferences: RuntimePreferencesSnapshot,
}

impl TerminalDockAdapter {
    pub fn unavailable() -> Self {
        Self {
            model: None,
            preferences: RuntimePreferencesSnapshot::default(),
        }
    }

    pub fn unavailable_with_preferences(preferences: RuntimePreferencesSnapshot) -> Self {
        Self {
            model: None,
            preferences,
        }
    }

    /// Create a live adapter only when the caller already owns a complete
    /// model from the established renderer. No model transformation occurs.
    pub fn live(model: TerminalPaneModel) -> Self {
        Self {
            model: Some(model),
            preferences: RuntimePreferencesSnapshot::default(),
        }
    }

    pub fn set_preferences(&mut self, preferences: RuntimePreferencesSnapshot) {
        self.preferences = preferences;
    }

    /// Bind or clear the established pane model without synthesizing identity.
    /// `None` keeps the typed unavailable state; `Some` requires a complete
    /// caller-owned [`TerminalPaneModel`].
    pub fn rebind(&mut self, model: Option<TerminalPaneModel>) {
        self.model = model;
    }

    pub fn state(&self) -> TerminalDockState {
        if self.model.is_some() {
            TerminalDockState::live()
        } else {
            TerminalDockState::unavailable()
        }
    }

    pub fn element(&self) -> gpui::AnyElement {
        match self.model.as_ref() {
            Some(model) => render_terminal_surface(model, None).into_any_element(),
            None => Component::new(TerminalDockUnavailable {
                preferences: self.preferences,
            })
            .into_any_element(),
        }
    }
}

/// A visible failure state for the isolated shell. This keeps the dependency
/// and the missing capability discoverable to both a user and a screen reader.
struct TerminalDockUnavailable {
    preferences: RuntimePreferencesSnapshot,
}

impl RenderOnce for TerminalDockUnavailable {
    fn render(self, _window: &mut Window, _cx: &mut App) -> impl IntoElement {
        let tokens = self.preferences.tokens();
        // The dock sits on the terminal surface, so this state reads from the
        // terminal palette. Panel colors here would render dark text on a dark
        // background and look like a rendering failure.
        div()
            .id("terminal-dock-unavailable")
            .w_full()
            .flex_col()
            .gap(px(tokens.density.spacing.xs))
            .text_color(tokens.terminal.foreground.to_gpui())
            .whitespace_normal()
            .child(
                div()
                    .flex()
                    .gap(px(tokens.density.spacing.sm))
                    .child(div().text_color(tokens.terminal.green.to_gpui()).child("$"))
                    .child(
                        div()
                            .text_color(tokens.terminal.foreground.to_gpui())
                            .child("Terminal dock unavailable"),
                    ),
            )
            .child("Waiting for the task terminal stream")
            .child("No terminal is synthesized and no extra PTY is started.")
    }
}
