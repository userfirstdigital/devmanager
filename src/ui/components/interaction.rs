//! Shared interaction, action, accessibility, and bounded-text contracts.
//!
//! Components use this module as their policy boundary.  GPUI event plumbing
//! may change, but an actionable control still has one catalog request,
//! typed action event, focus epoch, and pointer-press owner.

pub use crate::client::action::ActionRequest;
use crate::ui::tokens::{ActionStateTokens, Color, ThemeTokens};
use std::fmt::{Display, Formatter};

pub const MAX_ACCESSIBLE_NAME_SCALARS: usize = 256;
pub const MAX_ACCESSIBLE_DESCRIPTION_SCALARS: usize = 512;
pub const MAX_RECOVERY_ACTIONS: usize = 3;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ComponentError {
    Empty {
        field: &'static str,
    },
    TooManyScalars {
        field: &'static str,
        max: usize,
        actual: usize,
    },
    TooManyBytes {
        field: &'static str,
        max: usize,
        actual: usize,
    },
    InvalidCombination(&'static str),
    MissingTooltip,
    TooManyRecoveryActions {
        max: usize,
        actual: usize,
    },
    InvalidLimit(&'static str),
    LimitTooLarge {
        field: &'static str,
        max: usize,
        actual: usize,
    },
    StaleFocusEpoch {
        current: u64,
        attempted: u64,
    },
}

impl Display for ComponentError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Empty { field } => write!(formatter, "{field} must not be blank"),
            Self::TooManyScalars { field, max, actual } => {
                write!(
                    formatter,
                    "{field} exceeds {max} Unicode scalar values ({actual})"
                )
            }
            Self::TooManyBytes { field, max, actual } => {
                write!(formatter, "{field} exceeds {max} UTF-8 bytes ({actual})")
            }
            Self::InvalidCombination(reason) => {
                write!(formatter, "invalid interaction state: {reason}")
            }
            Self::MissingTooltip => write!(formatter, "icon-only controls require a tooltip"),
            Self::TooManyRecoveryActions { max, actual } => {
                write!(
                    formatter,
                    "at most {max} recovery actions are allowed ({actual})"
                )
            }
            Self::InvalidLimit(field) => write!(formatter, "{field} must be greater than zero"),
            Self::LimitTooLarge { field, max, actual } => {
                write!(formatter, "{field} exceeds safe maximum {max} ({actual})")
            }
            Self::StaleFocusEpoch { current, attempted } => write!(
                formatter,
                "focus epoch {attempted} is stale; current host epoch is {current}"
            ),
        }
    }
}

impl std::error::Error for ComponentError {}

pub fn bounded_text(
    field: &'static str,
    value: impl Into<String>,
    max_scalars: usize,
    max_bytes: usize,
) -> Result<String, ComponentError> {
    let value = value.into();
    if value.trim().is_empty() {
        return Err(ComponentError::Empty { field });
    }
    let actual_scalars = value.chars().count();
    if actual_scalars > max_scalars {
        return Err(ComponentError::TooManyScalars {
            field,
            max: max_scalars,
            actual: actual_scalars,
        });
    }
    if value.len() > max_bytes {
        return Err(ComponentError::TooManyBytes {
            field,
            max: max_bytes,
            actual: value.len(),
        });
    }
    Ok(value)
}

pub(crate) fn redacted_bounded_text(
    field: &'static str,
    value: impl Into<String>,
    max_scalars: usize,
    max_bytes: usize,
) -> Result<String, ComponentError> {
    let value = crate::diagnostics::runner::redact_secrets(&value.into());
    let value = redact_ui_credential_lines(&value);
    bounded_text(field, value, max_scalars, max_bytes)
}

fn redact_ui_credential_lines(value: &str) -> String {
    value
        .split_inclusive('\n')
        .map(|line| {
            let (body, trailing) = line
                .strip_suffix('\n')
                .map(|body| (body, "\n"))
                .unwrap_or((line, ""));
            let lower = body.to_ascii_lowercase();
            if contains_ui_credential_marker(&lower) {
                format!("[redacted]{trailing}")
            } else {
                line.to_string()
            }
        })
        .collect()
}

fn contains_ui_credential_marker(lower: &str) -> bool {
    [
        "api_key",
        "api-key",
        "apikey",
        "aws_access_key",
        "aws-access-key",
        "aws_secret_key",
        "aws-secret-key",
        "access_key_id",
        "access-key-id",
        "secret_access_key",
        "secret-access-key",
    ]
    .iter()
    .any(|marker| lower.contains(marker))
        || [
            "authorization:",
            "authorization=",
            "\"authorization\"",
            "'authorization'",
            "authorization bearer ",
            "authorization basic ",
        ]
        .iter()
        .any(|marker| lower.contains(marker))
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum KeyboardKey {
    Enter,
    Space,
    Escape,
    Tab,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ActivationSource {
    Pointer { pointer_id: u64 },
    Keyboard { key: KeyboardKey },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ActionEvent {
    pub request: ActionRequest,
    pub source: ActivationSource,
    pub focus_epoch: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AccessibleRole {
    Button,
    TextField,
    Status,
    Alert,
    Region,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AccessibilityMetadata {
    pub role: AccessibleRole,
    pub name: String,
    pub description: String,
    pub error: Option<String>,
    pub disabled: bool,
    pub busy: bool,
    pub focused: bool,
    pub invalid: bool,
    pub read_only: bool,
    pub value: Option<String>,
}

impl AccessibilityMetadata {
    pub fn new(role: AccessibleRole, name: impl Into<String>) -> Result<Self, ComponentError> {
        Ok(Self {
            role,
            name: bounded_text(
                "accessible name",
                name,
                MAX_ACCESSIBLE_NAME_SCALARS,
                MAX_ACCESSIBLE_NAME_SCALARS * 4,
            )?,
            description: String::new(),
            error: None,
            disabled: false,
            busy: false,
            focused: false,
            invalid: false,
            read_only: false,
            value: None,
        })
    }

    pub fn set_description(
        &mut self,
        description: impl Into<String>,
    ) -> Result<(), ComponentError> {
        self.description = bounded_text(
            "accessible description",
            description,
            MAX_ACCESSIBLE_DESCRIPTION_SCALARS,
            MAX_ACCESSIBLE_DESCRIPTION_SCALARS * 4,
        )?;
        Ok(())
    }

    pub fn clear_description(&mut self) {
        self.description.clear();
    }

    pub fn set_optional_description(
        &mut self,
        description: Option<impl Into<String>>,
    ) -> Result<(), ComponentError> {
        match description {
            Some(description) => self.set_description(description),
            None => {
                self.description.clear();
                Ok(())
            }
        }
    }

    pub fn set_error(&mut self, error: Option<impl Into<String>>) -> Result<(), ComponentError> {
        self.error = match error {
            Some(error) => Some(redacted_bounded_text(
                "accessible error",
                error,
                MAX_ACCESSIBLE_DESCRIPTION_SCALARS,
                MAX_ACCESSIBLE_DESCRIPTION_SCALARS * 4,
            )?),
            None => None,
        };
        self.invalid = self.error.is_some();
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VisualState {
    Default,
    Hover,
    Pressed,
    Focused,
    Disabled,
    Loading,
    Destructive,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InteractionTransition {
    Hover,
    Unhover,
    Press,
    Release,
    Focus,
    Blur,
    Disable,
    Enable,
    BeginLoading,
    EndLoading,
    Destructive(bool),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InteractionState {
    pub hovered: bool,
    pub pressed: bool,
    pub focused: bool,
    pub disabled: bool,
    pub loading: bool,
    pub destructive: bool,
}

impl Default for InteractionState {
    fn default() -> Self {
        Self {
            hovered: false,
            pressed: false,
            focused: false,
            disabled: false,
            loading: false,
            destructive: false,
        }
    }
}

impl InteractionState {
    pub const fn disabled() -> Self {
        Self {
            hovered: false,
            pressed: false,
            focused: false,
            disabled: true,
            loading: false,
            destructive: false,
        }
    }

    pub const fn loading() -> Self {
        Self {
            hovered: false,
            pressed: false,
            focused: false,
            disabled: false,
            loading: true,
            destructive: false,
        }
    }

    pub fn transition(self, transition: InteractionTransition) -> Result<Self, ComponentError> {
        let mut next = self;
        match transition {
            InteractionTransition::Hover => {
                if self.disabled || self.loading {
                    return Err(ComponentError::InvalidCombination(
                        "disabled or loading controls cannot hover",
                    ));
                }
                next.hovered = true;
            }
            InteractionTransition::Unhover => next.hovered = false,
            InteractionTransition::Press => {
                if self.disabled || self.loading {
                    return Err(ComponentError::InvalidCombination(
                        "disabled or loading controls cannot press",
                    ));
                }
                next.pressed = true;
            }
            InteractionTransition::Release => next.pressed = false,
            InteractionTransition::Focus => {
                if self.disabled {
                    return Err(ComponentError::InvalidCombination(
                        "disabled controls cannot focus",
                    ));
                }
                next.focused = true;
            }
            InteractionTransition::Blur => next.focused = false,
            InteractionTransition::Disable => {
                next = Self {
                    disabled: true,
                    destructive: self.destructive,
                    ..Self::default()
                };
            }
            InteractionTransition::Enable => next.disabled = false,
            InteractionTransition::BeginLoading => {
                if self.disabled {
                    return Err(ComponentError::InvalidCombination(
                        "disabled controls cannot enter loading",
                    ));
                }
                next.loading = true;
                next.hovered = false;
                next.pressed = false;
            }
            InteractionTransition::EndLoading => next.loading = false,
            InteractionTransition::Destructive(destructive) => next.destructive = destructive,
        }

        if next.disabled && (next.hovered || next.pressed || next.focused || next.loading) {
            return Err(ComponentError::InvalidCombination(
                "disabled controls cannot carry active interaction flags",
            ));
        }
        if next.loading && (next.hovered || next.pressed) {
            return Err(ComponentError::InvalidCombination(
                "loading controls cannot carry pointer interaction flags",
            ));
        }
        Ok(next)
    }

    pub fn fail_closed(self) -> Self {
        Self {
            disabled: true,
            destructive: self.destructive,
            ..Self::default()
        }
    }

    pub fn can_activate(self) -> bool {
        !self.disabled && !self.loading
    }

    pub fn visual_state(self) -> VisualState {
        if self.disabled {
            VisualState::Disabled
        } else if self.loading {
            VisualState::Loading
        } else if self.pressed {
            VisualState::Pressed
        } else if self.focused {
            VisualState::Focused
        } else if self.hovered {
            VisualState::Hover
        } else if self.destructive {
            VisualState::Destructive
        } else {
            VisualState::Default
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct PressOwner {
    pointer_id: u64,
    focus_epoch: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InteractionStateModel {
    state: InteractionState,
    focus_epoch: u64,
    press_owner: Option<PressOwner>,
}

impl Default for InteractionStateModel {
    fn default() -> Self {
        Self {
            state: InteractionState::default(),
            focus_epoch: 0,
            press_owner: None,
        }
    }
}

impl InteractionStateModel {
    pub fn state(&self) -> InteractionState {
        self.state
    }

    pub fn focus_epoch(&self) -> u64 {
        self.focus_epoch
    }

    pub fn set_focus_epoch(&mut self, focus_epoch: u64) -> bool {
        self.try_set_focus_epoch(focus_epoch).is_ok()
    }

    pub fn try_set_focus_epoch(&mut self, focus_epoch: u64) -> Result<(), ComponentError> {
        if focus_epoch < self.focus_epoch {
            return Err(ComponentError::StaleFocusEpoch {
                current: self.focus_epoch,
                attempted: focus_epoch,
            });
        }
        if self.focus_epoch == focus_epoch {
            return Ok(());
        }

        self.focus_epoch = focus_epoch;
        self.press_owner = None;
        let state = self.state;
        self.state = state
            .transition(InteractionTransition::Release)
            .and_then(|state| state.transition(InteractionTransition::Blur))
            .unwrap_or_else(|_| state.fail_closed());
        Ok(())
    }

    pub fn transition(&mut self, transition: InteractionTransition) -> Result<(), ComponentError> {
        match self.state.transition(transition) {
            Ok(next) => {
                self.state = next;
                if matches!(
                    transition,
                    InteractionTransition::Disable | InteractionTransition::BeginLoading
                ) {
                    self.press_owner = None;
                }
                Ok(())
            }
            Err(error) => {
                self.press_owner = None;
                self.state = self.state.fail_closed();
                Err(error)
            }
        }
    }

    pub fn set_disabled(&mut self, disabled: bool) {
        let transition = if disabled {
            InteractionTransition::Disable
        } else {
            InteractionTransition::Enable
        };
        let _ = self.transition(transition);
    }

    pub fn focus(&mut self) -> bool {
        self.transition(InteractionTransition::Focus).is_ok()
    }

    pub fn blur(&mut self) {
        let _ = self.transition(InteractionTransition::Blur);
    }

    pub fn set_loading(&mut self, loading: bool) -> Result<(), ComponentError> {
        self.transition(if loading {
            InteractionTransition::BeginLoading
        } else {
            InteractionTransition::EndLoading
        })
    }

    pub fn set_destructive(&mut self, destructive: bool) -> Result<(), ComponentError> {
        self.transition(InteractionTransition::Destructive(destructive))
    }

    pub fn pointer_down(&mut self, pointer_id: u64, focus_epoch: u64) -> bool {
        if focus_epoch != self.focus_epoch || !self.state.can_activate() {
            return false;
        }
        if self.transition(InteractionTransition::Press).is_err() {
            return false;
        }
        self.press_owner = Some(PressOwner {
            pointer_id,
            focus_epoch,
        });
        true
    }

    pub fn pointer_up(&mut self, pointer_id: u64, focus_epoch: u64) -> bool {
        let owner = self.press_owner.take();
        let same_owner = owner
            .map(|owner| owner.pointer_id == pointer_id && owner.focus_epoch == focus_epoch)
            .unwrap_or(false);
        let current_epoch = focus_epoch == self.focus_epoch;
        let actionable = self.state.can_activate();
        let _ = self.transition(InteractionTransition::Release);
        same_owner && current_epoch && actionable
    }

    pub fn key_activate(&self, key: KeyboardKey, focus_epoch: u64) -> bool {
        focus_epoch == self.focus_epoch
            && self.state.focused
            && self.state.can_activate()
            && matches!(key, KeyboardKey::Enter | KeyboardKey::Space)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FocusRing {
    pub color: Color,
    pub width: u32,
    pub offset: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ControlPresentation {
    pub foreground: Color,
    pub background: Color,
    pub border: Color,
    pub focus_ring: Option<FocusRing>,
    pub disabled: bool,
    pub busy: bool,
    pub destructive: bool,
}

pub fn control_presentation(
    tokens: ThemeTokens,
    state: InteractionState,
    destructive: bool,
) -> ControlPresentation {
    let semantic_tokens = if destructive {
        tokens.actions.destructive
    } else {
        tokens.actions.primary
    };
    let colors = match state.visual_state() {
        VisualState::Hover => semantic_tokens.hover,
        VisualState::Pressed => semantic_tokens.selected,
        VisualState::Focused => semantic_tokens.focus,
        VisualState::Disabled | VisualState::Loading => semantic_tokens.disabled,
        VisualState::Default | VisualState::Destructive => semantic_tokens.default,
    };
    let physical = tokens.density.physical();
    ControlPresentation {
        foreground: colors.foreground,
        background: colors.background,
        border: colors.border,
        focus_ring: state.focused.then_some(FocusRing {
            color: tokens.borders.focus,
            width: physical.focus_ring_width,
            offset: physical.focus_ring_width.max(1),
        }),
        disabled: state.disabled,
        busy: state.loading,
        destructive,
    }
}

pub fn neutral_control_presentation(
    tokens: ThemeTokens,
    state: InteractionState,
) -> ControlPresentation {
    let physical = tokens.density.physical();
    let (foreground, background, border) = if state.disabled || state.loading {
        (
            tokens.text.disabled,
            tokens.surfaces.disabled,
            tokens.borders.disabled,
        )
    } else if state.pressed || state.hovered {
        (
            tokens.text.primary,
            tokens.surfaces.hover,
            tokens.borders.strong,
        )
    } else {
        (
            tokens.text.primary,
            tokens.surfaces.raised,
            tokens.borders.default,
        )
    };
    ControlPresentation {
        foreground,
        background,
        border,
        focus_ring: state.focused.then_some(FocusRing {
            color: tokens.borders.focus,
            width: physical.focus_ring_width,
            offset: physical.focus_ring_width.max(1),
        }),
        disabled: state.disabled,
        busy: state.loading,
        destructive: false,
    }
}

pub fn status_tokens(
    tokens: ThemeTokens,
    meaning: crate::ui::tokens::StatusMeaning,
) -> ActionStateTokens {
    let (background, foreground) = match meaning {
        crate::ui::tokens::StatusMeaning::External => (
            tokens.status.external_surface,
            tokens.status.external_foreground,
        ),
        crate::ui::tokens::StatusMeaning::Attention => (
            tokens.status.attention_surface,
            tokens.status.attention_foreground,
        ),
        crate::ui::tokens::StatusMeaning::Success => (
            tokens.status.success_surface,
            tokens.status.success_foreground,
        ),
        crate::ui::tokens::StatusMeaning::Warning => (
            tokens.status.warning_surface,
            tokens.status.warning_foreground,
        ),
        crate::ui::tokens::StatusMeaning::Destructive => (
            tokens.status.destructive_surface,
            tokens.status.destructive_foreground,
        ),
        crate::ui::tokens::StatusMeaning::Inactive => (
            tokens.status.inactive_surface,
            tokens.status.inactive_foreground,
        ),
    };
    ActionStateTokens {
        foreground,
        background,
        border: tokens.status.color(meaning),
    }
}
