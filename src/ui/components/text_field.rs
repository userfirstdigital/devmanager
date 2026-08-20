//! Bounded text-field model with explicit read-only and disabled behavior.

use super::interaction::{
    redacted_bounded_text, AccessibilityMetadata, AccessibleRole, ComponentError, FocusEpoch,
    InteractionStateModel, MAX_ACCESSIBLE_DESCRIPTION_SCALARS, MAX_ACCESSIBLE_NAME_SCALARS,
};
use std::fmt::{Display, Formatter};
use std::ops::Range;

pub const MAX_TEXT_FIELD_SCALARS: usize = 4_096;
pub const MAX_TEXT_FIELD_BYTES: usize = 16_384;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TextFieldLimits {
    pub max_scalars: usize,
    pub max_bytes: usize,
}

impl TextFieldLimits {
    pub const fn new(max_scalars: usize, max_bytes: usize) -> Result<Self, ComponentError> {
        let limits = Self {
            max_scalars,
            max_bytes,
        };
        limits.validate()
    }

    pub const fn validate(self) -> Result<Self, ComponentError> {
        if self.max_scalars == 0 {
            return Err(ComponentError::InvalidLimit("maximum scalar count"));
        }
        if self.max_scalars > MAX_TEXT_FIELD_SCALARS {
            return Err(ComponentError::LimitTooLarge {
                field: "maximum scalar count",
                max: MAX_TEXT_FIELD_SCALARS,
                actual: self.max_scalars,
            });
        }
        if self.max_bytes == 0 {
            return Err(ComponentError::InvalidLimit("maximum byte count"));
        }
        if self.max_bytes > MAX_TEXT_FIELD_BYTES {
            return Err(ComponentError::LimitTooLarge {
                field: "maximum byte count",
                max: MAX_TEXT_FIELD_BYTES,
                actual: self.max_bytes,
            });
        }
        Ok(self)
    }
}

impl Default for TextFieldLimits {
    fn default() -> Self {
        Self {
            max_scalars: MAX_TEXT_FIELD_SCALARS,
            max_bytes: MAX_TEXT_FIELD_BYTES,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TextFieldError {
    Component(ComponentError),
    InvalidScalarRange {
        start: usize,
        end: usize,
        len: usize,
    },
    ScalarLimitExceeded {
        max: usize,
        actual: usize,
    },
    ByteLimitExceeded {
        max: usize,
        actual: usize,
    },
}

impl From<ComponentError> for TextFieldError {
    fn from(error: ComponentError) -> Self {
        Self::Component(error)
    }
}

impl Display for TextFieldError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Component(error) => Display::fmt(error, formatter),
            Self::InvalidScalarRange { start, end, len } => {
                write!(
                    formatter,
                    "invalid text range {start}..{end} for {len} scalars"
                )
            }
            Self::ScalarLimitExceeded { max, actual } => {
                write!(
                    formatter,
                    "text exceeds {max} Unicode scalar values ({actual})"
                )
            }
            Self::ByteLimitExceeded { max, actual } => {
                write!(formatter, "text exceeds {max} UTF-8 bytes ({actual})")
            }
        }
    }
}

impl std::error::Error for TextFieldError {}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TextFieldKey {
    Character(char),
    Backspace,
    Delete,
    Left,
    Right,
    Home,
    End,
    Enter,
    Escape,
    Tab,
}

pub struct TextField {
    value: String,
    cursor: usize,
    all_selected: bool,
    limits: TextFieldLimits,
    label: String,
    description: String,
    error: Option<String>,
    read_only: bool,
    interaction: InteractionStateModel,
    accessibility: AccessibilityMetadata,
}

impl TextField {
    pub fn new(label: impl Into<String>) -> Result<Self, TextFieldError> {
        Self::with_limits(label, TextFieldLimits::default())
    }

    pub fn with_limits(
        label: impl Into<String>,
        limits: TextFieldLimits,
    ) -> Result<Self, TextFieldError> {
        let limits = limits.validate()?;
        let label = redacted_bounded_text(
            "text field label",
            label,
            MAX_ACCESSIBLE_NAME_SCALARS,
            MAX_ACCESSIBLE_NAME_SCALARS * 4,
        )?;
        Ok(Self {
            accessibility: AccessibilityMetadata::new(AccessibleRole::TextField, label.clone())?,
            value: String::new(),
            cursor: 0,
            all_selected: false,
            limits,
            label,
            description: String::new(),
            error: None,
            read_only: false,
            interaction: InteractionStateModel::default(),
        })
    }

    pub fn value(&self) -> &str {
        &self.value
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    pub fn is_all_selected(&self) -> bool {
        self.all_selected && !self.value.is_empty()
    }

    pub fn select_all(&mut self) {
        if self.value.is_empty() {
            self.all_selected = false;
            return;
        }
        self.cursor = self.value.chars().count();
        self.all_selected = true;
    }

    pub fn label(&self) -> &str {
        &self.label
    }

    pub fn limits(&self) -> TextFieldLimits {
        self.limits
    }

    pub fn is_disabled(&self) -> bool {
        self.interaction.state().is_disabled()
    }

    pub fn is_read_only(&self) -> bool {
        self.read_only
    }

    pub fn is_focused(&self) -> bool {
        self.interaction.state().focused()
    }

    pub fn focus_epoch(&self) -> FocusEpoch {
        self.interaction.focus_epoch()
    }

    pub fn accessibility(&self) -> &AccessibilityMetadata {
        &self.accessibility
    }

    pub fn set_description(
        &mut self,
        description: impl Into<String>,
    ) -> Result<(), TextFieldError> {
        self.description = redacted_bounded_text(
            "text field description",
            description,
            MAX_ACCESSIBLE_DESCRIPTION_SCALARS,
            MAX_ACCESSIBLE_DESCRIPTION_SCALARS * 4,
        )?;
        self.accessibility
            .set_description(self.description.clone())?;
        Ok(())
    }

    pub fn set_error(&mut self, error: Option<impl Into<String>>) -> Result<(), TextFieldError> {
        self.error = match error {
            Some(error) => Some(redacted_bounded_text(
                "text field error",
                error,
                MAX_ACCESSIBLE_DESCRIPTION_SCALARS,
                MAX_ACCESSIBLE_DESCRIPTION_SCALARS * 4,
            )?),
            None => None,
        };
        self.accessibility.set_error(self.error.clone())?;
        if self.error.is_none() {
            self.accessibility
                .set_description(self.description.clone())?;
        }
        Ok(())
    }

    pub fn set_value(&mut self, value: impl Into<String>) -> Result<(), TextFieldError> {
        let value = value.into();
        self.validate_value(&value)?;
        self.value = value;
        self.cursor = self.value.chars().count();
        self.all_selected = false;
        self.accessibility.set_value(Some(self.value.clone()));
        Ok(())
    }

    pub fn set_read_only(&mut self, read_only: bool) {
        self.read_only = read_only;
        self.accessibility.set_read_only(read_only);
    }

    pub fn set_focus_epoch(&mut self, focus_epoch: FocusEpoch) -> bool {
        let accepted = self.interaction.set_focus_epoch(focus_epoch);
        self.accessibility
            .set_focused(self.interaction.state().focused());
        accepted
    }

    pub fn set_disabled(&mut self, disabled: bool) {
        self.interaction.set_disabled(disabled);
        self.accessibility
            .set_disabled(self.interaction.state().is_disabled());
        if disabled {
            self.accessibility.set_focused(false);
        }
    }

    pub fn focus(&mut self) -> bool {
        let focused = self.interaction.focus();
        self.accessibility.set_focused(focused);
        focused
    }

    pub fn blur(&mut self) {
        self.interaction.blur();
        self.accessibility.set_focused(false);
    }

    pub fn handle_key(
        &mut self,
        key: TextFieldKey,
        focus_epoch: FocusEpoch,
    ) -> Result<bool, TextFieldError> {
        let state = self.interaction.state();
        if self.interaction.focus_epoch() != focus_epoch || state.is_disabled() || !state.focused()
        {
            return Ok(false);
        }
        match key {
            TextFieldKey::Character(character) => {
                if self.read_only {
                    Ok(false)
                } else {
                    self.replace_selection_if_needed();
                    self.insert_text(&character.to_string())
                }
            }
            TextFieldKey::Backspace => {
                if self.read_only {
                    Ok(false)
                } else if self.clear_selection_contents() {
                    Ok(true)
                } else {
                    Ok(self.delete_before_cursor())
                }
            }
            TextFieldKey::Delete => {
                if self.read_only {
                    Ok(false)
                } else if self.clear_selection_contents() {
                    Ok(true)
                } else {
                    Ok(self.delete_at_cursor())
                }
            }
            TextFieldKey::Left => {
                self.clear_selection_keeping_value();
                self.cursor = self.cursor.saturating_sub(1);
                Ok(false)
            }
            TextFieldKey::Right => {
                self.clear_selection_keeping_value();
                self.cursor = (self.cursor + 1).min(self.value.chars().count());
                Ok(false)
            }
            TextFieldKey::Home => {
                self.clear_selection_keeping_value();
                self.cursor = 0;
                Ok(false)
            }
            TextFieldKey::End => {
                self.clear_selection_keeping_value();
                self.cursor = self.value.chars().count();
                Ok(false)
            }
            TextFieldKey::Enter | TextFieldKey::Escape | TextFieldKey::Tab => Ok(false),
        }
    }

    pub fn paste(&mut self, text: &str, focus_epoch: FocusEpoch) -> Result<bool, TextFieldError> {
        let state = self.interaction.state();
        if self.interaction.focus_epoch() != focus_epoch
            || !state.focused()
            || state.is_disabled()
            || self.read_only
        {
            return Ok(false);
        }
        self.replace_selection_if_needed();
        self.insert_text(text)
    }

    /// Replace a scalar-indexed range as one platform text-input operation.
    ///
    /// Platform IME, paste, and accessibility input arrive as range
    /// replacements rather than individual key presses. Keeping that edit in
    /// the field model preserves the same limits, focus fence, cursor, and
    /// accessibility value used by ordinary typing.
    pub fn replace_range(
        &mut self,
        range: Range<usize>,
        text: &str,
        focus_epoch: FocusEpoch,
    ) -> Result<bool, TextFieldError> {
        let state = self.interaction.state();
        if self.interaction.focus_epoch() != focus_epoch
            || !state.focused()
            || state.is_disabled()
            || self.read_only
        {
            return Ok(false);
        }
        let scalar_len = self.value.chars().count();
        if range.start > range.end || range.end > scalar_len {
            return Err(TextFieldError::InvalidScalarRange {
                start: range.start,
                end: range.end,
                len: scalar_len,
            });
        }
        let start = byte_index_at_scalar(&self.value, range.start);
        let end = byte_index_at_scalar(&self.value, range.end);
        let mut next = self.value.clone();
        next.replace_range(start..end, text);
        self.validate_value(&next)?;
        let changed = next != self.value;
        self.value = next;
        self.cursor = range.start + text.chars().count();
        self.all_selected = false;
        self.accessibility.set_value(Some(self.value.clone()));
        Ok(changed)
    }

    fn replace_selection_if_needed(&mut self) {
        let _ = self.clear_selection_contents();
    }

    fn clear_selection_keeping_value(&mut self) {
        self.all_selected = false;
    }

    fn clear_selection_contents(&mut self) -> bool {
        if !self.all_selected {
            return false;
        }
        self.value.clear();
        self.cursor = 0;
        self.all_selected = false;
        self.accessibility.set_value(Some(self.value.clone()));
        true
    }

    fn insert_text(&mut self, text: &str) -> Result<bool, TextFieldError> {
        if self.interaction.state().is_disabled() || self.read_only {
            return Ok(false);
        }
        if text.is_empty() {
            return Ok(false);
        }
        self.preflight_insert(text)?;
        let byte_index = byte_index_at_scalar(&self.value, self.cursor);
        let mut next = self.value.clone();
        next.insert_str(byte_index, text);
        self.value = next;
        self.cursor += text.chars().count();
        self.accessibility.set_value(Some(self.value.clone()));
        Ok(!text.is_empty())
    }

    fn validate_value(&self, value: &str) -> Result<(), TextFieldError> {
        let scalar_count = value.chars().count();
        if scalar_count > self.limits.max_scalars {
            return Err(TextFieldError::ScalarLimitExceeded {
                max: self.limits.max_scalars,
                actual: scalar_count,
            });
        }
        if value.len() > self.limits.max_bytes {
            return Err(TextFieldError::ByteLimitExceeded {
                max: self.limits.max_bytes,
                actual: value.len(),
            });
        }
        Ok(())
    }

    fn preflight_insert(&self, text: &str) -> Result<(), TextFieldError> {
        let scalar_count = self
            .value
            .chars()
            .count()
            .checked_add(text.chars().count())
            .unwrap_or(usize::MAX);
        if scalar_count > self.limits.max_scalars {
            return Err(TextFieldError::ScalarLimitExceeded {
                max: self.limits.max_scalars,
                actual: scalar_count,
            });
        }
        let byte_count = self
            .value
            .len()
            .checked_add(text.len())
            .unwrap_or(usize::MAX);
        if byte_count > self.limits.max_bytes {
            return Err(TextFieldError::ByteLimitExceeded {
                max: self.limits.max_bytes,
                actual: byte_count,
            });
        }
        Ok(())
    }

    fn delete_before_cursor(&mut self) -> bool {
        if self.cursor == 0 {
            return false;
        }
        let start = byte_index_at_scalar(&self.value, self.cursor - 1);
        let end = byte_index_at_scalar(&self.value, self.cursor);
        self.value.replace_range(start..end, "");
        self.cursor -= 1;
        self.accessibility.set_value(Some(self.value.clone()));
        true
    }

    fn delete_at_cursor(&mut self) -> bool {
        if self.cursor >= self.value.chars().count() {
            return false;
        }
        let start = byte_index_at_scalar(&self.value, self.cursor);
        let end = byte_index_at_scalar(&self.value, self.cursor + 1);
        self.value.replace_range(start..end, "");
        self.accessibility.set_value(Some(self.value.clone()));
        true
    }
}

fn byte_index_at_scalar(value: &str, scalar_index: usize) -> usize {
    value
        .char_indices()
        .nth(scalar_index)
        .map(|(index, _)| index)
        .unwrap_or(value.len())
}

#[cfg(test)]
mod tests {
    use super::{TextField, TextFieldKey};

    fn focused_field(value: &str) -> TextField {
        let mut field = TextField::new("Name").expect("text field");
        field.set_value(value).expect("set value");
        field.focus();
        field
    }

    #[test]
    fn typing_replaces_a_selected_prefilled_name() {
        let mut field = focused_field("command");
        field.select_all();
        assert!(field.is_all_selected());
        let epoch = field.focus_epoch();
        assert!(field
            .handle_key(TextFieldKey::Character('x'), epoch)
            .expect("type"));
        assert_eq!(field.value(), "x");
        assert!(!field.is_all_selected());
    }

    #[test]
    fn backspace_clears_a_selected_prefilled_name() {
        let mut field = focused_field("command");
        field.select_all();
        let epoch = field.focus_epoch();
        assert!(field
            .handle_key(TextFieldKey::Backspace, epoch)
            .expect("backspace"));
        assert_eq!(field.value(), "");
        assert!(!field.is_all_selected());
    }

    #[test]
    fn platform_text_replacement_keeps_the_inserted_cursor_position() {
        let mut field = focused_field("alpha omega");
        let epoch = field.focus_epoch();
        assert!(field
            .replace_range(6..11, "beta", epoch)
            .expect("replace platform text range"));
        assert_eq!(field.value(), "alpha beta");
        assert_eq!(field.cursor(), 10);
        assert!(!field.is_all_selected());
    }
}
