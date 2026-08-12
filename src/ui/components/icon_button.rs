//! Accessible icon-only control boundary.

use super::button::{Button, ButtonVariant};
use super::interaction::{
    AccessibilityMetadata, ActionEvent, ActionRequest, ComponentError, ControlPresentation,
    FocusEpoch, KeyboardKey,
};
use crate::ui::tokens::ThemeTokens;
use gpui::{div, px, rgb, IntoElement, ParentElement, Styled};

/// Stable icon vocabulary used by native controls.  Keeping the identifier
/// typed prevents a fixture, remote client, or caller from smuggling an
/// arbitrary asset path into the renderer.
#[derive(Clone, Copy, Eq, PartialEq, Hash)]
pub struct IconId(u8);

#[allow(non_upper_case_globals)]
impl IconId {
    pub const Activity: Self = Self(0);
    pub const Bot: Self = Self(1);
    pub const ChevronDown: Self = Self(2);
    pub const ChevronLeft: Self = Self(3);
    pub const ChevronRight: Self = Self(4);
    pub const ChevronUp: Self = Self(5);
    pub const FileText: Self = Self(6);
    pub const Folder: Self = Self(7);
    pub const GitBranch: Self = Self(8);
    pub const Globe: Self = Self(9);
    pub const MoreHorizontal: Self = Self(10);
    pub const Play: Self = Self(11);
    pub const Plus: Self = Self(12);
    pub const Refresh: Self = Self(13);
    pub const Server: Self = Self(14);
    pub const Settings: Self = Self(15);
    pub const Sparkles: Self = Self(16);
    pub const Square: Self = Self(17);
    pub const Terminal: Self = Self(18);
    pub const X: Self = Self(19);
    pub const Warning: Self = Self(20);
    pub const OpenInNew: Self = Self(21);

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Activity => "activity",
            Self::Bot => "bot",
            Self::ChevronDown => "chevron-down",
            Self::ChevronLeft => "chevron-left",
            Self::ChevronRight => "chevron-right",
            Self::ChevronUp => "chevron-up",
            Self::FileText => "file-text",
            Self::Folder => "folder",
            Self::GitBranch => "git-branch",
            Self::Globe => "globe",
            Self::MoreHorizontal => "more-horizontal",
            Self::Play => "play",
            Self::Plus => "plus",
            Self::Refresh => "refresh-cw",
            Self::Server => "server",
            Self::Settings => "settings",
            Self::Sparkles => "sparkles",
            Self::Square => "square",
            Self::Terminal => "terminal",
            Self::X => "x",
            Self::Warning => "warning",
            Self::OpenInNew => "open-in-new",
            _ => "unknown",
        }
    }

    pub(crate) const fn asset_path(self) -> &'static str {
        match self {
            Self::Activity => crate::icons::ACTIVITY,
            Self::Bot => crate::icons::BOT,
            Self::ChevronDown => crate::icons::CHEVRON_DOWN,
            Self::ChevronLeft => crate::icons::CHEVRON_LEFT,
            Self::ChevronRight => crate::icons::CHEVRON_RIGHT,
            Self::ChevronUp => crate::icons::CHEVRON_UP,
            Self::FileText => crate::icons::FILE_TEXT,
            Self::Folder => crate::icons::FOLDER,
            Self::GitBranch => crate::icons::GIT_BRANCH,
            Self::Globe => crate::icons::GLOBE,
            Self::MoreHorizontal => crate::icons::MORE_HORIZONTAL,
            Self::Play => crate::icons::PLAY,
            Self::Plus => crate::icons::PLUS,
            Self::Refresh => crate::icons::REFRESH_CW,
            Self::Server => crate::icons::SERVER,
            Self::Settings => crate::icons::SETTINGS,
            Self::Sparkles => crate::icons::SPARKLES,
            Self::Square => crate::icons::SQUARE,
            Self::Terminal => crate::icons::TERMINAL,
            Self::X => crate::icons::X,
            Self::Warning => "icons/warning.svg",
            Self::OpenInNew => "icons/open-in-new.svg",
            _ => "icons/unknown.svg",
        }
    }
}

impl std::fmt::Debug for IconId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("IconId(<approved>)")
    }
}

impl std::fmt::Display for IconId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("IconId(<approved>)")
    }
}

impl AsRef<str> for IconId {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl TryFrom<&str> for IconId {
    type Error = ComponentError;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        match value {
            "activity" => Ok(Self::Activity),
            "bot" => Ok(Self::Bot),
            "chevron-down" => Ok(Self::ChevronDown),
            "chevron-left" => Ok(Self::ChevronLeft),
            "chevron-right" => Ok(Self::ChevronRight),
            "chevron-up" => Ok(Self::ChevronUp),
            "file-text" => Ok(Self::FileText),
            "folder" => Ok(Self::Folder),
            "git-branch" => Ok(Self::GitBranch),
            "globe" => Ok(Self::Globe),
            "more-horizontal" => Ok(Self::MoreHorizontal),
            "play" => Ok(Self::Play),
            "plus" => Ok(Self::Plus),
            "refresh-cw" => Ok(Self::Refresh),
            "server" => Ok(Self::Server),
            "settings" => Ok(Self::Settings),
            "sparkles" => Ok(Self::Sparkles),
            "square" => Ok(Self::Square),
            "terminal" => Ok(Self::Terminal),
            "x" => Ok(Self::X),
            "warning" => Ok(Self::Warning),
            "open-in-new" => Ok(Self::OpenInNew),
            _ => Err(ComponentError::InvalidIconId),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TooltipContract {
    pub label: String,
    pub delay_ms: u16,
}

impl TooltipContract {
    pub fn new(label: impl Into<String>, delay_ms: u16) -> Result<Self, ComponentError> {
        normalize_tooltip(Self {
            label: label.into(),
            delay_ms,
        })
    }
}

fn normalize_tooltip(tooltip: TooltipContract) -> Result<TooltipContract, ComponentError> {
    if tooltip.delay_ms == 0 {
        return Err(ComponentError::InvalidLimit("tooltip delay"));
    }
    Ok(TooltipContract {
        label: super::interaction::redacted_bounded_text(
            "tooltip label",
            tooltip.label,
            super::interaction::MAX_ACCESSIBLE_DESCRIPTION_SCALARS,
            super::interaction::MAX_ACCESSIBLE_DESCRIPTION_SCALARS * 4,
        )?,
        delay_ms: tooltip.delay_ms,
    })
}

pub struct IconButton {
    icon: IconId,
    button: Button,
    tooltip: TooltipContract,
}

impl IconButton {
    pub fn new(
        icon: impl AsRef<str>,
        accessible_label: impl Into<String>,
        tooltip: TooltipContract,
        action_request: ActionRequest,
    ) -> Result<Self, ComponentError> {
        let icon = IconId::try_from(icon.as_ref())?;
        let tooltip = normalize_tooltip(tooltip)?;
        if tooltip.label.trim().is_empty() {
            return Err(ComponentError::MissingTooltip);
        }
        let mut button = Button::new(accessible_label, action_request)?;
        button.set_accessibility_description(tooltip.label.clone())?;
        Ok(Self {
            icon,
            button,
            tooltip,
        })
    }

    pub fn new_variant(
        icon: impl AsRef<str>,
        accessible_label: impl Into<String>,
        tooltip: TooltipContract,
        variant: ButtonVariant,
        action_request: ActionRequest,
    ) -> Result<Self, ComponentError> {
        let icon = IconId::try_from(icon.as_ref())?;
        let tooltip = normalize_tooltip(tooltip)?;
        if tooltip.label.trim().is_empty() {
            return Err(ComponentError::MissingTooltip);
        }
        let mut button = Button::new_variant(accessible_label, variant, action_request)?;
        button.set_accessibility_description(tooltip.label.clone())?;
        Ok(Self {
            icon,
            button,
            tooltip,
        })
    }

    pub fn icon(&self) -> IconId {
        self.icon
    }

    pub fn icon_id(&self) -> IconId {
        self.icon
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

    pub(crate) fn set_pressed_for_preview(&mut self) {
        self.button.set_pressed_for_preview();
    }

    pub(crate) fn set_hovered_for_preview(&mut self, hovered: bool) -> Result<(), ComponentError> {
        self.button.set_hovered(hovered)
    }

    pub fn focus(&mut self) -> bool {
        self.button.focus()
    }

    pub fn blur(&mut self) {
        self.button.blur();
    }

    pub fn set_focus_epoch(&mut self, focus_epoch: FocusEpoch) {
        self.button.set_focus_epoch(focus_epoch);
    }

    pub fn pointer_down(&mut self, pointer_id: u64, focus_epoch: FocusEpoch) -> bool {
        self.button.pointer_down(pointer_id, focus_epoch)
    }

    pub fn pointer_up(&mut self, pointer_id: u64, focus_epoch: FocusEpoch) -> Option<ActionEvent> {
        self.button.pointer_up(pointer_id, focus_epoch)
    }

    pub fn key_activate(&self, key: KeyboardKey, focus_epoch: FocusEpoch) -> Option<ActionEvent> {
        self.button.key_activate(key, focus_epoch)
    }

    pub fn presentation(&self, tokens: ThemeTokens) -> ControlPresentation {
        self.button.presentation(tokens)
    }

    /// Render an icon-only control through the shared button presentation and
    /// typed icon asset mapping.  The accessible label remains metadata, not
    /// an attacker-controlled asset path.
    pub fn element(&self, tokens: ThemeTokens) -> impl IntoElement {
        let presentation = self.presentation(tokens);
        let mut element = div()
            .flex()
            .items_center()
            .justify_center()
            .gap(px(tokens.density.controls.icon_gap))
            .p(px(tokens.density.controls.control_padding))
            .rounded_md()
            .border_1()
            .border_color(rgb(presentation.border.to_u32()))
            .bg(rgb(presentation.background.to_u32()))
            .child(crate::icons::app_icon(
                self.icon.asset_path(),
                tokens.density.icons.md,
                presentation.foreground.to_u32(),
            ));
        if let Some(focus_ring) = presentation.focus_ring {
            element = element.border_color(rgb(focus_ring.color.to_u32()));
        }
        if !presentation.disabled {
            element = element.cursor_pointer();
        }
        element
    }
}
