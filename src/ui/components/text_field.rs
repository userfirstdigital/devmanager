//! Bounded text-field model with explicit read-only and disabled behavior.

use super::interaction::{
    AccessibilityMetadata, AccessibleRole, ComponentError, InteractionStateModel,
};
use std::fmt::{Display, Formatter};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TextFieldLimits {
    pub max_scalars: usize,
    pub max_bytes: usize,
}

impl TextFieldLimits {
    pub const fn new(max_scalars: usize, max_bytes: usize) -> Result<Self, ComponentError> {
        if max_scalars == 0 {
            return Err(ComponentError::InvalidLimit("maximum scalar count"));
        }
        if max_bytes == 0 {
            return Err(ComponentError::InvalidLimit("maximum byte count"));
        }
        Ok(Self {
            max_scalars,
            max_bytes,
        })
    }
}

impl Default for TextFieldLimits {
    fn default() -> Self {
        Self {
            max_scalars: 4096,
            max_bytes: 16384,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TextFieldError {
    Component(ComponentError),
    ScalarLimitExceeded { max: usize, actual: usize },
    ByteLimitExceeded { max: usize, actual: usize },
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
        let label = super::interaction::bounded_text("text field label", label, 256, 1024)?;
        Ok(Self {
            accessibility: AccessibilityMetadata::new(AccessibleRole::TextField, label.clone())?,
            value: String::new(),
            cursor: 0,
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

    pub fn label(&self) -> &str {
        &self.label
    }

    pub fn limits(&self) -> TextFieldLimits {
        self.limits
    }

    pub fn is_disabled(&self) -> bool {
        self.interaction.state().disabled
    }

    pub fn is_read_only(&self) -> bool {
        self.read_only
    }

    pub fn is_focused(&self) -> bool {
        self.interaction.state().focused
    }

    pub fn accessibility(&self) -> &AccessibilityMetadata {
        &self.accessibility
    }

    pub fn set_description(
        &mut self,
        description: impl Into<String>,
    ) -> Result<(), TextFieldError> {
        self.description =
            super::interaction::bounded_text("text field description", description, 512, 2048)?;
        self.accessibility
            .set_description(self.description.clone())?;
        Ok(())
    }

    pub fn set_error(&mut self, error: Option<impl Into<String>>) -> Result<(), TextFieldError> {
        self.error = match error {
            Some(error) => Some(super::interaction::bounded_text(
                "text field error",
                error,
                512,
                2048,
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
        self.accessibility.value = Some(self.value.clone());
        Ok(())
    }

    pub fn set_read_only(&mut self, read_only: bool) {
        self.read_only = read_only;
        self.accessibility.read_only = read_only;
    }

    pub fn set_disabled(&mut self, disabled: bool) {
        self.interaction.set_disabled(disabled);
        self.accessibility.disabled = self.interaction.state().disabled;
        if disabled {
            self.accessibility.focused = false;
        }
    }

    pub fn focus(&mut self) -> bool {
        let focused = self.interaction.focus();
        self.accessibility.focused = focused;
        focused
    }

    pub fn blur(&mut self) {
        self.interaction.blur();
        self.accessibility.focused = false;
    }

    pub fn handle_key(&mut self, key: TextFieldKey) -> Result<bool, TextFieldError> {
        if self.interaction.state().disabled || !self.interaction.state().focused {
            return Ok(false);
        }
        match key {
            TextFieldKey::Character(character) => {
                if self.read_only {
                    Ok(false)
                } else {
                    self.insert_text(&character.to_string())
                }
            }
            TextFieldKey::Backspace => {
                if self.read_only {
                    Ok(false)
                } else {
                    Ok(self.delete_before_cursor())
                }
            }
            TextFieldKey::Delete => {
                if self.read_only {
                    Ok(false)
                } else {
                    Ok(self.delete_at_cursor())
                }
            }
            TextFieldKey::Left => {
                self.cursor = self.cursor.saturating_sub(1);
                Ok(false)
            }
            TextFieldKey::Right => {
                self.cursor = (self.cursor + 1).min(self.value.chars().count());
                Ok(false)
            }
            TextFieldKey::Home => {
                self.cursor = 0;
                Ok(false)
            }
            TextFieldKey::End => {
                self.cursor = self.value.chars().count();
                Ok(false)
            }
            TextFieldKey::Enter | TextFieldKey::Escape | TextFieldKey::Tab => Ok(false),
        }
    }

    pub fn paste(&mut self, text: &str) -> Result<bool, TextFieldError> {
        self.insert_text(text)
    }

    fn insert_text(&mut self, text: &str) -> Result<bool, TextFieldError> {
        if self.interaction.state().disabled || self.read_only {
            return Ok(false);
        }
        let byte_index = byte_index_at_scalar(&self.value, self.cursor);
        let mut next = self.value.clone();
        next.insert_str(byte_index, text);
        self.validate_value(&next)?;
        self.value = next;
        self.cursor += text.chars().count();
        self.accessibility.value = Some(self.value.clone());
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

    fn delete_before_cursor(&mut self) -> bool {
        if self.cursor == 0 {
            return false;
        }
        let start = byte_index_at_scalar(&self.value, self.cursor - 1);
        let end = byte_index_at_scalar(&self.value, self.cursor);
        self.value.replace_range(start..end, "");
        self.cursor -= 1;
        self.accessibility.value = Some(self.value.clone());
        true
    }

    fn delete_at_cursor(&mut self) -> bool {
        if self.cursor >= self.value.chars().count() {
            return false;
        }
        let start = byte_index_at_scalar(&self.value, self.cursor);
        let end = byte_index_at_scalar(&self.value, self.cursor + 1);
        self.value.replace_range(start..end, "");
        self.accessibility.value = Some(self.value.clone());
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
