//! Shared interaction, action, accessibility, and bounded-text contracts.
//!
//! Components use this module as their policy boundary.  GPUI event plumbing
//! may change, but an actionable control still has one catalog request,
//! typed action event, focus epoch, and pointer-press owner.

pub use crate::client::action::ActionRequest;
use crate::ui::tokens::{ActionStateTokens, Color, ThemeTokens};
use std::fmt::{Display, Formatter};
use std::sync::atomic::{AtomicU64, Ordering};

pub const MAX_ACCESSIBLE_NAME_SCALARS: usize = 256;
pub const MAX_ACCESSIBLE_DESCRIPTION_SCALARS: usize = 512;
pub const MAX_RECOVERY_ACTIONS: usize = 3;

static NEXT_FOCUS_SOURCE_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FocusEpoch {
    source_id: u64,
    sequence: u64,
}

impl FocusEpoch {
    const INITIAL: Self = Self {
        source_id: 0,
        sequence: 0,
    };

    const fn initial() -> Self {
        Self::INITIAL
    }

    const fn is_initial(self) -> bool {
        self.source_id == 0 && self.sequence == 0
    }
}

#[derive(Debug)]
pub struct FocusEpochSource {
    source_id: u64,
    sequence: u64,
}

pub type FocusCoordinator = FocusEpochSource;

impl FocusEpochSource {
    pub fn new() -> Self {
        let source_id = NEXT_FOCUS_SOURCE_ID
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |next| {
                next.checked_add(1)
            })
            .expect("focus epoch source identity exhausted");
        Self {
            source_id,
            sequence: 0,
        }
    }

    pub fn current(&self) -> FocusEpoch {
        FocusEpoch {
            source_id: self.source_id,
            sequence: self.sequence,
        }
    }

    pub fn advance(&mut self) -> FocusEpoch {
        self.sequence = self
            .sequence
            .checked_add(1)
            .expect("focus epoch source exhausted");
        self.current()
    }
}

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
        current: FocusEpoch,
        attempted: FocusEpoch,
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
            Self::StaleFocusEpoch { .. } => {
                write!(formatter, "focus epoch is stale or belongs to another host")
            }
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
    let value = value.into();
    let value = redact_sensitive_text(&value);
    bounded_text(field, value, max_scalars, max_bytes)
}

pub(crate) fn redact_sensitive_text(value: &str) -> String {
    let value = crate::diagnostics::runner::redact_secrets(value);
    redact_ui_credential_lines(&value)
}

fn redact_ui_credential_lines(value: &str) -> String {
    let mut redacted = String::with_capacity(value.len());
    for line in value.split_inclusive('\n') {
        let (body, trailing) = line
            .strip_suffix('\n')
            .map(|body| (body, "\n"))
            .unwrap_or((line, ""));
        if contains_ui_credential_marker(body) {
            redacted.push_str("[redacted]");
            redacted.push_str(trailing);
        } else {
            redacted.push_str(line);
        }
    }
    redacted
}

fn contains_ui_credential_marker(value: &str) -> bool {
    for (start, _) in value.char_indices() {
        if start > 0 {
            let previous = value[..start].chars().next_back().unwrap_or_default();
            if previous.is_ascii_alphanumeric() || previous == '_' {
                continue;
            }
        }
        for key in [
            "apikey",
            "accesskeyid",
            "secretaccesskey",
            "awsaccesskeyid",
            "awssecretaccesskey",
            "credential",
            "authorization",
        ] {
            if let Some(end) = normalized_key_end(value, start, key) {
                let remainder = &value[end..];
                if key == "authorization" {
                    if has_authorization_value(remainder) {
                        return true;
                    }
                } else if has_assignment_value(remainder) {
                    return true;
                }
            }
        }
    }
    false
}

fn normalized_key_end(value: &str, start: usize, key: &str) -> Option<usize> {
    let mut cursor = start;
    for expected in key.chars() {
        loop {
            let character = value.get(cursor..)?.chars().next()?;
            cursor += character.len_utf8();
            if matches!(character, '_' | '-' | ' ' | '\t') {
                continue;
            }
            if !character.eq_ignore_ascii_case(&expected) {
                return None;
            }
            break;
        }
    }
    Some(cursor)
}

fn has_assignment_value(remainder: &str) -> bool {
    let remainder = remainder.trim_start();
    let remainder = remainder
        .strip_prefix('"')
        .or_else(|| remainder.strip_prefix('\''))
        .map(str::trim_start)
        .unwrap_or(remainder);
    let Some(remainder) = remainder
        .strip_prefix(':')
        .or_else(|| remainder.strip_prefix('='))
    else {
        return false;
    };
    !remainder.trim().is_empty()
}

fn has_authorization_value(remainder: &str) -> bool {
    let remainder = remainder.trim_start();
    let remainder = remainder
        .strip_prefix('"')
        .or_else(|| remainder.strip_prefix('\''))
        .map(str::trim_start)
        .unwrap_or(remainder);
    let Some(remainder) = remainder
        .strip_prefix(':')
        .or_else(|| remainder.strip_prefix('='))
    else {
        return false;
    };
    let remainder = remainder.trim_start();
    ["basic", "bearer"].iter().any(|scheme| {
        remainder
            .get(..scheme.len())
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case(scheme))
            && !remainder[scheme.len()..].trim().is_empty()
    })
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
    focus_epoch: FocusEpoch,
}

impl ActionEvent {
    pub fn focus_epoch(&self) -> FocusEpoch {
        self.focus_epoch
    }

    pub(crate) fn new(
        request: ActionRequest,
        source: ActivationSource,
        focus_epoch: FocusEpoch,
    ) -> Self {
        Self {
            request,
            source,
            focus_epoch,
        }
    }
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
            name: redacted_bounded_text(
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
        self.description = redacted_bounded_text(
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
    focus_epoch: FocusEpoch,
}

#[derive(Debug, Eq, PartialEq)]
pub struct InteractionStateModel {
    state: InteractionState,
    focus_epoch: FocusEpoch,
    press_owner: Option<PressOwner>,
}

impl Default for InteractionStateModel {
    fn default() -> Self {
        Self {
            state: InteractionState::default(),
            focus_epoch: FocusEpoch::initial(),
            press_owner: None,
        }
    }
}

impl InteractionStateModel {
    pub fn state(&self) -> InteractionState {
        self.state
    }

    pub fn focus_epoch(&self) -> FocusEpoch {
        self.focus_epoch
    }

    pub fn set_focus_epoch(&mut self, focus_epoch: FocusEpoch) -> bool {
        self.try_set_focus_epoch(focus_epoch).is_ok()
    }

    pub fn try_set_focus_epoch(&mut self, focus_epoch: FocusEpoch) -> Result<(), ComponentError> {
        if (!self.focus_epoch.is_initial() && focus_epoch.source_id != self.focus_epoch.source_id)
            || (focus_epoch.source_id == self.focus_epoch.source_id
                && focus_epoch.sequence < self.focus_epoch.sequence)
        {
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

    pub fn pointer_down(&mut self, pointer_id: u64, focus_epoch: FocusEpoch) -> bool {
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

    pub fn pointer_up(&mut self, pointer_id: u64, focus_epoch: FocusEpoch) -> bool {
        let owner = self.press_owner.take();
        let same_owner = owner
            .map(|owner| owner.pointer_id == pointer_id && owner.focus_epoch == focus_epoch)
            .unwrap_or(false);
        let current_epoch = focus_epoch == self.focus_epoch;
        let actionable = self.state.can_activate();
        let _ = self.transition(InteractionTransition::Release);
        same_owner && current_epoch && actionable
    }

    pub fn key_activate(&self, key: KeyboardKey, focus_epoch: FocusEpoch) -> bool {
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
