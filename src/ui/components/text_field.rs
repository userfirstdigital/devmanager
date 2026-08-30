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
    Undo,
    Redo,
}

const MAX_TEXT_FIELD_HISTORY: usize = 64;

#[derive(Clone)]
struct TextFieldSnapshot {
    value: String,
    cursor: usize,
    selection_anchor: Option<usize>,
}

pub struct TextField {
    value: String,
    cursor: usize,
    /// Exclusive selection start in Unicode scalars when present. The live
    /// cursor is the other end; equality clears the range.
    selection_anchor: Option<usize>,
    limits: TextFieldLimits,
    label: String,
    description: String,
    error: Option<String>,
    read_only: bool,
    sensitive: bool,
    interaction: InteractionStateModel,
    accessibility: AccessibilityMetadata,
    undo_history: Vec<TextFieldSnapshot>,
    redo_history: Vec<TextFieldSnapshot>,
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
            selection_anchor: None,
            limits,
            label,
            description: String::new(),
            error: None,
            read_only: false,
            sensitive: false,
            interaction: InteractionStateModel::default(),
            undo_history: Vec::new(),
            redo_history: Vec::new(),
        })
    }

    pub fn value(&self) -> &str {
        &self.value
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    pub fn selection_range(&self) -> Option<Range<usize>> {
        let len = self.value.chars().count();
        let cursor = self.cursor.min(len);
        let anchor = self.selection_anchor?.min(len);
        if anchor == cursor {
            None
        } else if anchor < cursor {
            Some(anchor..cursor)
        } else {
            Some(cursor..anchor)
        }
    }

    pub fn is_all_selected(&self) -> bool {
        let len = self.value.chars().count();
        matches!(
            self.selection_range(),
            Some(range) if range.start == 0 && range.end == len && len > 0
        )
    }

    pub fn select_all(&mut self) {
        if self.value.is_empty() {
            self.selection_anchor = None;
            return;
        }
        self.selection_anchor = Some(0);
        self.cursor = self.value.chars().count();
    }

    pub fn set_cursor(&mut self, cursor: usize, extend: bool) {
        let len = self.value.chars().count();
        let next = cursor.min(len);
        if extend {
            if self.selection_anchor.is_none() {
                self.selection_anchor = Some(self.cursor.min(len));
            }
        } else {
            self.selection_anchor = None;
        }
        self.cursor = next;
        if self.selection_anchor == Some(self.cursor) {
            self.selection_anchor = None;
        }
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

    /// Secret edit buffers must not be copied into accessibility metadata.
    pub fn set_sensitive(&mut self, sensitive: bool) {
        self.sensitive = sensitive;
        self.refresh_accessibility_value();
    }

    fn refresh_accessibility_value(&mut self) {
        self.accessibility.set_value(if self.sensitive {
            None
        } else {
            Some(self.value.clone())
        });
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
        self.selection_anchor = None;
        self.undo_history.clear();
        self.redo_history.clear();
        self.refresh_accessibility_value();
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
                    self.replace_range(
                        self.selection_range().unwrap_or(self.cursor..self.cursor),
                        &character.to_string(),
                        focus_epoch,
                    )
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
                let target = if let Some(range) = self.selection_range() {
                    range.start
                } else {
                    self.cursor.saturating_sub(1)
                };
                self.set_cursor(target, false);
                Ok(true)
            }
            TextFieldKey::Right => {
                let len = self.value.chars().count();
                let target = if let Some(range) = self.selection_range() {
                    range.end
                } else {
                    (self.cursor + 1).min(len)
                };
                self.set_cursor(target, false);
                Ok(true)
            }
            TextFieldKey::Home => {
                self.set_cursor(0, false);
                Ok(true)
            }
            TextFieldKey::End => {
                self.set_cursor(self.value.chars().count(), false);
                Ok(true)
            }
            TextFieldKey::Undo => Ok(self.undo()),
            TextFieldKey::Redo => Ok(self.redo()),
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
        if text.is_empty() {
            return Ok(false);
        }
        self.replace_range(
            self.selection_range().unwrap_or(self.cursor..self.cursor),
            text,
            focus_epoch,
        )
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
        if changed {
            self.push_undo_snapshot();
        }
        self.value = next;
        self.cursor = range.start + text.chars().count();
        self.selection_anchor = None;
        self.refresh_accessibility_value();
        Ok(changed)
    }

    fn clear_selection_keeping_value(&mut self) {
        self.selection_anchor = None;
    }

    fn clear_selection_contents(&mut self) -> bool {
        let Some(range) = self.selection_range() else {
            return false;
        };
        let start = byte_index_at_scalar(&self.value, range.start);
        let end = byte_index_at_scalar(&self.value, range.end);
        self.push_undo_snapshot();
        self.value.replace_range(start..end, "");
        self.cursor = range.start;
        self.selection_anchor = None;
        self.refresh_accessibility_value();
        true
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
        self.push_undo_snapshot();
        self.value.replace_range(start..end, "");
        self.cursor -= 1;
        self.refresh_accessibility_value();
        true
    }

    fn delete_at_cursor(&mut self) -> bool {
        if self.cursor >= self.value.chars().count() {
            return false;
        }
        let start = byte_index_at_scalar(&self.value, self.cursor);
        let end = byte_index_at_scalar(&self.value, self.cursor + 1);
        self.push_undo_snapshot();
        self.value.replace_range(start..end, "");
        self.refresh_accessibility_value();
        true
    }

    fn snapshot(&self) -> TextFieldSnapshot {
        TextFieldSnapshot {
            value: self.value.clone(),
            cursor: self.cursor,
            selection_anchor: self.selection_anchor,
        }
    }

    fn push_undo_snapshot(&mut self) {
        if self.undo_history.len() == MAX_TEXT_FIELD_HISTORY {
            self.undo_history.remove(0);
        }
        let snapshot = self.snapshot();
        self.undo_history.push(snapshot);
        self.redo_history.clear();
    }

    fn restore_snapshot(&mut self, snapshot: TextFieldSnapshot) {
        self.value = snapshot.value;
        self.cursor = snapshot.cursor.min(self.value.chars().count());
        self.selection_anchor = snapshot
            .selection_anchor
            .map(|anchor| anchor.min(self.value.chars().count()))
            .filter(|anchor| *anchor != self.cursor);
        self.refresh_accessibility_value();
    }

    fn undo(&mut self) -> bool {
        if self.read_only {
            return false;
        }
        let Some(previous) = self.undo_history.pop() else {
            return false;
        };
        if self.redo_history.len() == MAX_TEXT_FIELD_HISTORY {
            self.redo_history.remove(0);
        }
        let current = self.snapshot();
        self.redo_history.push(current);
        self.restore_snapshot(previous);
        true
    }

    fn redo(&mut self) -> bool {
        if self.read_only {
            return false;
        }
        let Some(next) = self.redo_history.pop() else {
            return false;
        };
        if self.undo_history.len() == MAX_TEXT_FIELD_HISTORY {
            self.undo_history.remove(0);
        }
        let current = self.snapshot();
        self.undo_history.push(current);
        self.restore_snapshot(next);
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
    fn sensitive_field_edits_do_not_publish_buffer_to_accessibility() {
        let mut field = focused_field("secret");
        field.set_sensitive(true);
        assert_eq!(field.accessibility().value(), None);
        field.set_value("replacement").unwrap();
        let epoch = field.focus_epoch();
        field.handle_key(TextFieldKey::Backspace, epoch).unwrap();
        assert_eq!(field.value(), "replacemen");
        assert_eq!(field.accessibility().value(), None);
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

    #[test]
    fn partial_selection_is_replaced_by_typing() {
        let mut field = focused_field("abcdef");
        field.set_cursor(1, false);
        field.set_cursor(4, true);
        assert_eq!(field.selection_range(), Some(1..4));
        let epoch = field.focus_epoch();
        assert!(field
            .handle_key(TextFieldKey::Character('x'), epoch)
            .expect("replace"));
        assert_eq!(field.value(), "axef");
        assert_eq!(field.cursor(), 2);
        assert!(field.selection_range().is_none());
    }

    #[test]
    fn rejected_replacement_preserves_text_cursor_and_selection() {
        let mut field = focused_field("abcde");
        field.limits = super::TextFieldLimits::new(5, 5).unwrap();
        field.set_cursor(1, false);
        field.set_cursor(3, true);
        let epoch = field.focus_epoch();
        assert!(field.paste("12345", epoch).is_err());
        assert_eq!(field.value(), "abcde");
        assert_eq!(field.cursor(), 3);
        assert_eq!(field.selection_range(), Some(1..3));
        assert!(field
            .handle_key(TextFieldKey::Character('界'), epoch)
            .is_err());
        assert_eq!(field.value(), "abcde");
        assert_eq!(field.selection_range(), Some(1..3));
        assert!(field.paste("12", epoch).unwrap());
        assert_eq!(field.value(), "a12de");
        assert_eq!(field.cursor(), 3);
        assert_eq!(field.selection_range(), None);
    }

    #[test]
    fn undo_and_redo_restore_replaced_selection_and_cursor() {
        let mut field = focused_field("original");
        field.select_all();
        let epoch = field.focus_epoch();
        assert!(field
            .replace_range(0..8, "replacement", epoch)
            .expect("replace"));
        assert_eq!(field.value(), "replacement");

        assert!(field.handle_key(TextFieldKey::Undo, epoch).expect("undo"));
        assert_eq!(field.value(), "original");
        assert!(field.is_all_selected());

        assert!(field.handle_key(TextFieldKey::Redo, epoch).expect("redo"));
        assert_eq!(field.value(), "replacement");
        assert_eq!(field.cursor(), "replacement".chars().count());
        assert!(field.selection_range().is_none());
    }
}
