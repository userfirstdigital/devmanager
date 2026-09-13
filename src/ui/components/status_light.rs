//! Noninteractive semantic status indicator.

use super::interaction::{
    redacted_bounded_text, status_tokens, AccessibilityMetadata, AccessibleRole, ComponentError,
    MAX_ACCESSIBLE_DESCRIPTION_SCALARS, MAX_ACCESSIBLE_NAME_SCALARS,
};
use crate::ui::tokens::{Color, StatusMeaning, ThemeTokens};
use gpui::{div, px, rgb, IntoElement, ParentElement, Styled};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StatusPresentation {
    pub meaning: StatusMeaning,
    pub indicator: Color,
    pub surface: Color,
    pub foreground: Color,
}

/// Caller-owned external/port presentation input.
///
/// This does not invent port health: the caller supplies already-bounded label
/// and description text. The helper only maps them onto
/// [`StatusMeaning::External`] blue semantics.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExternalPortStatus {
    label: String,
    description: String,
}

impl ExternalPortStatus {
    pub fn new(
        label: impl Into<String>,
        description: impl Into<String>,
    ) -> Result<Self, ComponentError> {
        let label = redacted_bounded_text(
            "external port label",
            label,
            MAX_ACCESSIBLE_NAME_SCALARS,
            MAX_ACCESSIBLE_NAME_SCALARS * 4,
        )?;
        let description = redacted_bounded_text(
            "external port description",
            description,
            MAX_ACCESSIBLE_DESCRIPTION_SCALARS,
            MAX_ACCESSIBLE_DESCRIPTION_SCALARS * 4,
        )?;
        Ok(Self { label, description })
    }

    pub fn label(&self) -> &str {
        &self.label
    }

    pub fn description(&self) -> &str {
        &self.description
    }
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

    /// First-class external/port blue presentation. Consumes caller-supplied
    /// bounded state and never probes ports or fabricates health facts.
    pub fn external_port(status: ExternalPortStatus) -> Result<Self, ComponentError> {
        Self::new(StatusMeaning::External, status.label, status.description)
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

    /// Render the noninteractive semantic status surface used by production
    /// status rows and by the native preview gallery.
    pub fn element(&self, tokens: ThemeTokens) -> impl IntoElement {
        let presentation = self.presentation(tokens);
        div()
            .flex()
            .items_center()
            .gap(px(tokens.density.controls.icon_gap))
            .px(px(tokens.density.controls.control_padding))
            .py(px(tokens.density.spacing.xs))
            .rounded_md()
            .border_1()
            .border_color(rgb(presentation.indicator.to_u32()))
            .bg(rgb(presentation.surface.to_u32()))
            .text_color(rgb(presentation.foreground.to_u32()))
            .child(
                div()
                    .size(px(tokens.density.icons.xs))
                    .rounded_full()
                    .bg(rgb(presentation.indicator.to_u32())),
            )
            .child(self.label.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::tokens::{contrast_ratio, theme, Density, Scale, ThemeMode};

    #[test]
    fn external_port_maps_to_external_blue_and_stays_noninteractive() {
        let status = ExternalPortStatus::new("Port 8080", "Caller-supplied listening port state.")
            .expect("bounded external port status");
        let light = StatusLight::external_port(status).expect("external port light");
        assert_eq!(light.meaning(), StatusMeaning::External);
        assert!(!light.is_interactive());
        assert_eq!(light.accessibility().role(), AccessibleRole::Status);
        assert_eq!(light.label(), "Port 8080");

        for mode in [ThemeMode::Dark, ThemeMode::Light, ThemeMode::HighContrast] {
            let tokens = theme(mode, Density::Comfortable, Scale::Scale100);
            let presentation = light.presentation(tokens);
            assert_eq!(presentation.meaning, StatusMeaning::External);
            assert_eq!(presentation.indicator, tokens.status.external);
            assert_eq!(presentation.surface, tokens.status.external_surface);
            assert_eq!(presentation.foreground, tokens.status.external_foreground);
            assert!(
                contrast_ratio(presentation.foreground, presentation.surface) >= 4.5,
                "external surface text must stay AA for {mode:?}"
            );
            assert!(
                contrast_ratio(presentation.indicator, presentation.surface) >= 3.0,
                "external indicator must stay distinguishable for {mode:?}"
            );
        }
    }

    #[test]
    fn external_port_rejects_unbounded_caller_text() {
        let oversized = "x".repeat(MAX_ACCESSIBLE_NAME_SCALARS + 1);
        assert!(ExternalPortStatus::new(oversized, "ok").is_err());
    }
}
