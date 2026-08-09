//! Noninteractive semantic status indicator.

use super::interaction::{
    redacted_bounded_text, status_tokens, AccessibilityMetadata, AccessibleRole, ComponentError,
    MAX_ACCESSIBLE_DESCRIPTION_SCALARS, MAX_ACCESSIBLE_NAME_SCALARS,
};
use crate::ui::tokens::{Color, StatusMeaning, ThemeTokens};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StatusPresentation {
    pub meaning: StatusMeaning,
    pub indicator: Color,
    pub surface: Color,
    pub foreground: Color,
}

pub struct StatusLight {
    meaning: StatusMeaning,
    label: String,
    description: String,
    accessibility: AccessibilityMetadata,
}

impl StatusLight {
    pub fn new(
        meaning: StatusMeaning,
        label: impl Into<String>,
        description: impl Into<String>,
    ) -> Result<Self, ComponentError> {
        let label = redacted_bounded_text(
            "status label",
            label,
            MAX_ACCESSIBLE_NAME_SCALARS,
            MAX_ACCESSIBLE_NAME_SCALARS * 4,
        )?;
        let description = redacted_bounded_text(
            "status description",
            description,
            MAX_ACCESSIBLE_DESCRIPTION_SCALARS,
            MAX_ACCESSIBLE_DESCRIPTION_SCALARS * 4,
        )?;
        let mut accessibility = AccessibilityMetadata::new(AccessibleRole::Status, label.clone())?;
        accessibility.set_description(description.clone())?;
        Ok(Self {
            meaning,
            label,
            description,
            accessibility,
        })
    }

    pub fn meaning(&self) -> StatusMeaning {
        self.meaning
    }

    pub fn label(&self) -> &str {
        &self.label
    }

    pub fn description(&self) -> &str {
        &self.description
    }

    pub fn accessibility(&self) -> &AccessibilityMetadata {
        &self.accessibility
    }

    pub const fn is_interactive(&self) -> bool {
        false
    }

    pub fn presentation(&self, tokens: ThemeTokens) -> StatusPresentation {
        let semantic = status_tokens(tokens, self.meaning);
        StatusPresentation {
            meaning: self.meaning,
            indicator: tokens.status.color(self.meaning),
            surface: semantic.background,
            foreground: semantic.foreground,
        }
    }
}
