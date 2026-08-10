//! Accessible icon-only control boundary.

use super::button::{Button, ButtonVariant};
use super::interaction::{
    AccessibilityMetadata, ActionEvent, ActionRequest, ComponentError, ControlPresentation,
    FocusEpoch, KeyboardKey,
};
use crate::ui::tokens::ThemeTokens;

/// Stable icon vocabulary used by native controls.  Keeping the identifier
/// typed prevents a fixture, remote client, or caller from smuggling an
/// arbitrary asset path into the renderer.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum IconId {
    Activity,
    Bot,
    ChevronDown,
    ChevronLeft,
    ChevronRight,
    ChevronUp,
    FileText,
    Folder,
    GitBranch,
    Globe,
    MoreHorizontal,
    Play,
    Plus,
    Refresh,
    Server,
    Settings,
    Sparkles,
    Square,
    Terminal,
    X,
    Warning,
    OpenInNew,
}

impl IconId {
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
        }
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
}
