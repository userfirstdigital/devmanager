//! Bounded, noninteractive semantic badge.

use super::interaction::{AccessibilityMetadata, ComponentError};
use super::status_light::{StatusLight, StatusPresentation};
use crate::ui::tokens::{StatusMeaning, ThemeTokens};

pub struct Badge {
    status: StatusLight,
}

impl Badge {
    pub fn new(
        label: impl Into<String>,
        description: Option<impl Into<String>>,
        meaning: StatusMeaning,
    ) -> Result<Self, ComponentError> {
        let description = description
            .map(Into::into)
            .unwrap_or_else(|| "Semantic status".to_string());
        Ok(Self {
            status: StatusLight::new(meaning, label, description)?,
        })
    }

    pub fn accessibility(&self) -> &AccessibilityMetadata {
        self.status.accessibility()
    }

    pub fn label(&self) -> &str {
        self.status.label()
    }

    pub const fn is_interactive(&self) -> bool {
        false
    }

    pub fn presentation(&self, tokens: ThemeTokens) -> StatusPresentation {
        self.status.presentation(tokens)
    }
}
