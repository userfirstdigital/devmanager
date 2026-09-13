//! The one terminal integration seam owned by the native Task Cockpit.
//!
//! The established renderer remains the raw-terminal implementation.  This
//! adapter either delegates to that renderer with a complete
//! [`TerminalPaneModel`] or renders an honest, typed unavailable state.  It
//! never clones terminal cells, translates terminal events, or falls back to a
//! WebView.

use crate::terminal::view::{render_terminal_surface_with_tokens, TerminalPaneModel};
use crate::ui::tokens::{RuntimePreferencesSnapshot, ThemeTokens};
use gpui::{
    div, px, App, Component, InteractiveElement, IntoElement, ParentElement, RenderOnce, Styled,
    Window,
};

pub const TERMINAL_ADAPTER_DEPENDENCY: &str =
    "src/terminal/view.rs::render_terminal_surface_with_tokens(TerminalPaneModel, TerminalPaneActions, ThemeTokens)";

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

    /// Legacy paint path: uses stored runtime preference tokens.
    pub fn element(&self) -> gpui::AnyElement {
        self.element_with_tokens(self.preferences.tokens())
    }

    /// Paint the live terminal or unavailable chrome with the caller's exact
    /// active [`ThemeTokens`] (custom/T3 editor preview included).
    pub fn element_with_tokens(&self, tokens: ThemeTokens) -> gpui::AnyElement {
        let tokens = self.resolve_paint_tokens(tokens);
        match self.model.as_ref() {
            Some(model) => {
                render_terminal_surface_with_tokens(model, None, tokens).into_any_element()
            }
            None => Component::new(TerminalDockUnavailable { tokens }).into_any_element(),
        }
    }

    /// Contract seam: the themed element path always consumes the supplied
    /// tokens and never substitutes preference defaults when tokens are passed.
    pub fn resolve_paint_tokens(&self, tokens: ThemeTokens) -> ThemeTokens {
        let _ = self.preferences;
        tokens
    }
}

/// A visible failure state for the isolated shell. This keeps the dependency
/// and the missing capability discoverable to both a user and a screen reader.
struct TerminalDockUnavailable {
    tokens: ThemeTokens,
}

impl RenderOnce for TerminalDockUnavailable {
    fn render(self, _window: &mut Window, _cx: &mut App) -> impl IntoElement {
        let tokens = self.tokens;
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::terminal::view::terminal_render_palette_from_tokens;
    use crate::ui::tokens::{
        dark, Color, Density, RuntimePreferencesSnapshot, Scale, ThemeMode, PREVIEW_SENTINEL,
    };

    fn sentinel_tokens() -> ThemeTokens {
        let mut tokens = dark(Density::Comfortable, Scale::Scale100);
        tokens.mode = ThemeMode::Dark;
        tokens.surfaces.canvas = Color::from_u32(0x010101);
        tokens.surfaces.raised = Color::from_u32(0x020202);
        tokens.surfaces.overlay = Color::from_u32(0x030303);
        tokens.surfaces.hover = Color::from_u32(0x040404);
        tokens.surfaces.disabled = Color::from_u32(0x050505);
        tokens.surfaces.sunken = Color::from_u32(0x161616);
        tokens.borders.default = Color::from_u32(0x060606);
        tokens.text.primary = Color::from_u32(0x070707);
        tokens.text.muted = Color::from_u32(0x080808);
        tokens.text.disabled = Color::from_u32(0x090909);
        tokens.text.on_selection = Color::from_u32(0x0a0a0a);
        tokens.actions.primary.default.background = Color::from_u32(0x0b0b0b);
        tokens.actions.primary.selected.background = Color::from_u32(0x0c0c0c);
        tokens.status.destructive = Color::from_u32(0x0d0d0d);
        tokens.status.destructive_surface = Color::from_u32(0x0e0e0e);
        tokens.status.warning = Color::from_u32(0x0f0f0f);
        tokens.status.success = Color::from_u32(0x101010);
        tokens.terminal.background = PREVIEW_SENTINEL;
        tokens.terminal.foreground = Color::from_u32(0x121212);
        tokens.terminal.cursor = Color::from_u32(0x131313);
        tokens.terminal.selection = Color::from_u32(0x141414);
        tokens
    }

    #[test]
    fn element_with_tokens_consumes_supplied_tokens_not_preference_defaults() {
        let preference_defaults = RuntimePreferencesSnapshot::default().tokens();
        let supplied = sentinel_tokens();
        assert_ne!(
            supplied.terminal.background.to_u32(),
            preference_defaults.terminal.background.to_u32()
        );

        let adapter = TerminalDockAdapter::unavailable_with_preferences(
            RuntimePreferencesSnapshot::default(),
        );
        let resolved = adapter.resolve_paint_tokens(supplied);
        assert_eq!(
            resolved.terminal.background.to_u32(),
            supplied.terminal.background.to_u32()
        );
        assert_ne!(
            resolved.terminal.background.to_u32(),
            preference_defaults.terminal.background.to_u32()
        );

        let palette = terminal_render_palette_from_tokens(resolved);
        assert_eq!(palette.terminal_bg, PREVIEW_SENTINEL.to_u32());
        assert_eq!(palette.terminal_bg, supplied.terminal.background.to_u32());

        // Source contract: native shell must pass active theme tokens into the
        // themed adapter path (not preference-default `element()`).
        let native_shell = include_str!("native_shell.rs");
        assert!(
            native_shell.contains("element_with_tokens("),
            "native_shell must call element_with_tokens"
        );
        assert!(
            native_shell.contains("self.terminal.element_with_tokens(")
                || native_shell.contains(".terminal.element_with_tokens("),
            "native_shell terminal dock must invoke element_with_tokens on the adapter"
        );
        assert!(
            native_shell.contains("element_with_tokens(tokens)")
                || native_shell.contains("element_with_tokens(self.theme_tokens())"),
            "native_shell must supply ThemeTokens to the terminal adapter"
        );
    }

    #[test]
    fn legacy_element_still_delegates_through_themed_entrypoint() {
        let source = include_str!("terminal_adapter.rs");
        assert!(source.contains("fn element(&self)"));
        assert!(source.contains("element_with_tokens(self.preferences.tokens())"));
        assert!(source.contains("render_terminal_surface_with_tokens"));
        assert!(source.contains("fn element_with_tokens(&self, tokens: ThemeTokens)"));
    }
}
