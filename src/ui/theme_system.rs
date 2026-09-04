//! T3-inspired native appearance authority.
//!
//! The native shell already consumes semantic [`ThemeTokens`]. This module
//! adds a persisted, user-extensible palette library without allowing render
//! code to grow a second set of literal colours.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs;
use std::io::{self, Write};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::{Map as JsonMap, Value};

use crate::ui::tokens::{
    mix_color, theme, ActionStateTokens, Color, Density, Scale, ThemeMode, ThemeTokens,
};

pub const THEME_FILE_VERSION: u16 = 1;
const APPEARANCE_FILE: &str = "appearance.json";
const MAX_THEME_FILE_BYTES: u64 = 256 * 1024;
const MAX_CUSTOM_THEMES: usize = 128;
const MAX_THEME_ID_BYTES: usize = 96;
const MAX_THEME_LABEL_BYTES: usize = 160;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThemeAppearance {
    Light,
    Dark,
}

impl ThemeAppearance {
    pub const fn mode(self) -> ThemeMode {
        match self {
            Self::Light => ThemeMode::Light,
            Self::Dark => ThemeMode::Dark,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AppearancePreference {
    System,
    Light,
    Dark,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThemeSelection {
    pub appearance: AppearancePreference,
    pub light_theme_id: String,
    pub dark_theme_id: String,
}

impl Default for ThemeSelection {
    fn default() -> Self {
        Self {
            appearance: AppearancePreference::Dark,
            light_theme_id: "devmanager-classic".to_string(),
            dark_theme_id: "devmanager-classic".to_string(),
        }
    }
}

/// Untouched pre-Classic default: System appearance with T3 Code on both halves.
fn is_untouched_legacy_theme_selection(selection: &ThemeSelection) -> bool {
    selection.appearance == AppearancePreference::System
        && selection.light_theme_id == "t3-code"
        && selection.dark_theme_id == "t3-code"
}

/// Migrate only the exact legacy default; never overwrite an explicit user choice.
pub fn migrate_untouched_legacy_theme_selection(selection: ThemeSelection) -> ThemeSelection {
    if is_untouched_legacy_theme_selection(&selection) {
        ThemeSelection::default()
    } else {
        selection
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedThemeSelection {
    pub appearance: ThemeAppearance,
    pub theme_id: String,
}

impl ThemeSelection {
    pub fn resolve(&self, system: ThemeAppearance) -> ResolvedThemeSelection {
        let appearance = match self.appearance {
            AppearancePreference::System => system,
            AppearancePreference::Light => ThemeAppearance::Light,
            AppearancePreference::Dark => ThemeAppearance::Dark,
        };
        let theme_id = match appearance {
            ThemeAppearance::Light => self.light_theme_id.clone(),
            ThemeAppearance::Dark => self.dark_theme_id.clone(),
        };
        ResolvedThemeSelection {
            appearance,
            theme_id,
        }
    }
}

macro_rules! theme_color_roles {
    ($($variant:ident => $name:literal),+ $(,)?) => {
        #[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
        pub enum ThemeColorRole { $($variant),+ }

        impl ThemeColorRole {
            pub const ALL: &'static [Self] = &[$(Self::$variant),+];

            pub const fn as_str(self) -> &'static str {
                match self { $(Self::$variant => $name),+ }
            }

            pub fn parse(value: &str) -> Option<Self> {
                match value { $($name => Some(Self::$variant),)+ _ => None }
            }
        }
    };
}

theme_color_roles! {
    Canvas => "canvas",
    Chrome => "chrome",
    Toolbar => "toolbar",
    ToolbarForeground => "toolbarForeground",
    ToolbarBorder => "toolbarBorder",
    ToolbarControl => "toolbarControl",
    ToolbarControlForeground => "toolbarControlForeground",
    ToolbarControlHover => "toolbarControlHover",
    Surface => "surface",
    SurfaceRaised => "surfaceRaised",
    SurfaceOverlay => "surfaceOverlay",
    Text => "text",
    TextMuted => "textMuted",
    Border => "border",
    Input => "input",
    Focus => "focus",
    Accent => "accent",
    AccentForeground => "accentForeground",
    Secondary => "secondary",
    SecondaryForeground => "secondaryForeground",
    Muted => "muted",
    MutedForeground => "mutedForeground",
    Placeholder => "placeholder",
    SecondaryLabel => "secondaryLabel",
    IconMuted => "iconMuted",
    Error => "error",
    ErrorForeground => "errorForeground",
    ErrorSurface => "errorSurface",
    Warning => "warning",
    WarningForeground => "warningForeground",
    WarningSurface => "warningSurface",
    Update => "update",
    UpdateForeground => "updateForeground",
    UpdateSurface => "updateSurface",
    AccentSurface => "accentSurface",
    AccentSurfaceForeground => "accentSurfaceForeground",
    MessageSurface => "messageSurface",
    MessageForeground => "messageForeground",
    MessageAction => "messageAction",
    MessageActionForeground => "messageActionForeground",
    MessageActionHover => "messageActionHover",
    CodeBackground => "codeBackground",
    CodeForeground => "codeForeground",
    Sidebar => "sidebar",
    SidebarForeground => "sidebarForeground",
    SidebarMutedForeground => "sidebarMutedForeground",
    SidebarControlSurface => "sidebarControlSurface",
    SidebarRowHover => "sidebarRowHover",
    SidebarRowActive => "sidebarRowActive",
    SidebarRowSelected => "sidebarRowSelected",
    SidebarBorder => "sidebarBorder",
    TerminalBackground => "terminalBackground",
    TerminalForeground => "terminalForeground",
    TerminalCursor => "terminalCursor",
    TerminalSelection => "terminalSelection",
    TerminalScrollbar => "terminalScrollbar",
    TerminalScrollbarHover => "terminalScrollbarHover",
}

impl fmt::Display for ThemeColorRole {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ThemeColor {
    red: u8,
    green: u8,
    blue: u8,
    alpha: u8,
}

impl ThemeColor {
    pub const fn rgb(red: u8, green: u8, blue: u8) -> Self {
        Self {
            red,
            green,
            blue,
            alpha: 255,
        }
    }

    pub fn from_hex(value: &str) -> Result<Self, ThemeColorError> {
        let raw = value.trim().strip_prefix('#').unwrap_or(value.trim());
        if raw.is_empty() || !raw.chars().all(|character| character.is_ascii_hexdigit()) {
            return Err(ThemeColorError::Invalid);
        }
        let expanded;
        let raw = if raw.len() == 3 || raw.len() == 4 {
            expanded = raw
                .chars()
                .flat_map(|character| [character, character])
                .collect::<String>();
            expanded.as_str()
        } else {
            raw
        };
        if raw.len() != 6 && raw.len() != 8 {
            return Err(ThemeColorError::Invalid);
        }
        let byte = |offset: usize| {
            u8::from_str_radix(&raw[offset..offset + 2], 16).map_err(|_| ThemeColorError::Invalid)
        };
        Ok(Self {
            red: byte(0)?,
            green: byte(2)?,
            blue: byte(4)?,
            alpha: if raw.len() == 8 { byte(6)? } else { 255 },
        })
    }

    /// Parse a theme colour from hex, CSS `rgb()`/`rgba()`, or `oklch()`.
    ///
    /// Serialization remains canonical hex via [`Self::to_hex`].
    pub fn parse(value: &str) -> Result<Self, ThemeColorError> {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            return Err(ThemeColorError::Invalid);
        }
        let ascii = trimmed.to_ascii_lowercase();
        if ascii.starts_with("rgba(") || ascii.starts_with("rgb(") {
            return parse_css_rgb(trimmed);
        }
        if ascii.starts_with("oklch(") {
            return parse_css_oklch(trimmed);
        }
        Self::from_hex(trimmed)
    }

    pub const fn opaque(self) -> Color {
        Color::rgb(self.red, self.green, self.blue)
    }

    /// Lift a canonical token colour into a palette role value.
    ///
    /// Built-in palettes are projections of `crate::ui::tokens`, so they are
    /// written against the token constants rather than restated as literals.
    pub const fn from_token(color: Color) -> Self {
        Self::rgb(color.red(), color.green(), color.blue())
    }

    pub fn to_hex(self) -> String {
        if self.alpha == 255 {
            format!("#{:02x}{:02x}{:02x}", self.red, self.green, self.blue)
        } else {
            format!(
                "#{:02x}{:02x}{:02x}{:02x}",
                self.red, self.green, self.blue, self.alpha
            )
        }
    }

    fn mix(self, other: Self, amount: f32) -> Self {
        let amount = amount.clamp(0.0, 1.0);
        let channel = |first: u8, second: u8| {
            (f32::from(first) + (f32::from(second) - f32::from(first)) * amount)
                .round()
                .clamp(0.0, 255.0) as u8
        };
        Self {
            red: channel(self.red, other.red),
            green: channel(self.green, other.green),
            blue: channel(self.blue, other.blue),
            alpha: channel(self.alpha, other.alpha),
        }
    }

    fn luminance(self) -> f64 {
        crate::ui::tokens::srgb_luminance(self.opaque())
    }

    fn contrast(self, other: Self) -> f64 {
        let first = self.luminance();
        let second = other.luminance();
        (first.max(second) + 0.05) / (first.min(second) + 0.05)
    }
}

fn parse_css_function_body(value: &str, name: &str) -> Result<String, ThemeColorError> {
    let trimmed = value.trim();
    let ascii = trimmed.to_ascii_lowercase();
    let prefix = format!("{name}(");
    if !ascii.starts_with(&prefix) || !trimmed.ends_with(')') {
        return Err(ThemeColorError::Invalid);
    }
    let body = &trimmed[prefix.len()..trimmed.len() - 1];
    if body.is_empty() || body.contains(['(', ')']) {
        return Err(ThemeColorError::Invalid);
    }
    Ok(body.to_string())
}

fn split_css_color_components(
    body: &str,
) -> Result<(Vec<String>, Option<String>), ThemeColorError> {
    let body = body.trim();
    if body.is_empty() {
        return Err(ThemeColorError::Invalid);
    }
    let (main, alpha) = match body.split_once('/') {
        Some((main, alpha)) => {
            let alpha = alpha.trim();
            if alpha.is_empty() || alpha.contains('/') || alpha.split_whitespace().count() != 1 {
                return Err(ThemeColorError::Invalid);
            }
            (main.trim(), Some(alpha.to_string()))
        }
        None => (body, None),
    };
    if main.is_empty() {
        return Err(ThemeColorError::Invalid);
    }
    let parts = if main.contains(',') {
        main.split(',')
            .map(str::trim)
            .map(str::to_string)
            .collect::<Vec<_>>()
    } else {
        main.split_whitespace()
            .map(str::to_string)
            .collect::<Vec<_>>()
    };
    if parts.is_empty() || parts.iter().any(|part| part.is_empty()) {
        return Err(ThemeColorError::Invalid);
    }
    Ok((parts, alpha))
}

fn parse_css_number(raw: &str) -> Result<f64, ThemeColorError> {
    let trimmed = raw.trim();
    if trimmed.is_empty()
        || !trimmed
            .chars()
            .all(|character| character.is_ascii_digit() || matches!(character, '.' | '+' | '-'))
        || trimmed.matches('.').count() > 1
        || trimmed.matches(['+', '-']).count() > 1
        || (trimmed.contains(['+', '-']) && !trimmed.starts_with(['+', '-']))
    {
        return Err(ThemeColorError::Invalid);
    }
    trimmed.parse::<f64>().map_err(|_| ThemeColorError::Invalid)
}

fn parse_css_channel(raw: &str, percent_scale: f64) -> Result<f64, ThemeColorError> {
    let trimmed = raw.trim();
    if let Some(percent) = trimmed.strip_suffix('%') {
        let value = parse_css_number(percent)?;
        Ok((value / 100.0) * percent_scale)
    } else {
        parse_css_number(trimmed)
    }
}

fn parse_css_alpha(raw: &str) -> Result<u8, ThemeColorError> {
    let trimmed = raw.trim();
    let value = if let Some(percent) = trimmed.strip_suffix('%') {
        parse_css_number(percent)? / 100.0
    } else {
        parse_css_number(trimmed)?
    };
    if !value.is_finite() || !(0.0..=1.0).contains(&value) {
        return Err(ThemeColorError::Invalid);
    }
    Ok((value * 255.0).round().clamp(0.0, 255.0) as u8)
}

fn clamp_u8_channel(value: f64) -> u8 {
    value.round().clamp(0.0, 255.0) as u8
}

fn parse_css_rgb(value: &str) -> Result<ThemeColor, ThemeColorError> {
    let ascii = value.trim().to_ascii_lowercase();
    let name = if ascii.starts_with("rgba(") {
        "rgba"
    } else {
        "rgb"
    };
    let body = parse_css_function_body(value, name)?;
    let (parts, slash_alpha) = split_css_color_components(&body)?;
    let (red_raw, green_raw, blue_raw, alpha_raw) = match parts.as_slice() {
        [red, green, blue] => (red.as_str(), green.as_str(), blue.as_str(), slash_alpha),
        [red, green, blue, alpha] if slash_alpha.is_none() => (
            red.as_str(),
            green.as_str(),
            blue.as_str(),
            Some(alpha.clone()),
        ),
        _ => return Err(ThemeColorError::Invalid),
    };
    if name == "rgba" && alpha_raw.is_none() {
        return Err(ThemeColorError::Invalid);
    }
    let red = parse_css_channel(red_raw, 255.0)?;
    let green = parse_css_channel(green_raw, 255.0)?;
    let blue = parse_css_channel(blue_raw, 255.0)?;
    if ![red, green, blue]
        .into_iter()
        .all(|channel| channel.is_finite() && (0.0..=255.0).contains(&channel))
    {
        return Err(ThemeColorError::Invalid);
    }
    let alpha = match alpha_raw {
        Some(raw) => parse_css_alpha(&raw)?,
        None => 255,
    };
    Ok(ThemeColor {
        red: clamp_u8_channel(red),
        green: clamp_u8_channel(green),
        blue: clamp_u8_channel(blue),
        alpha,
    })
}

fn parse_css_oklch(value: &str) -> Result<ThemeColor, ThemeColorError> {
    let body = parse_css_function_body(value, "oklch")?;
    let (parts, slash_alpha) = split_css_color_components(&body)?;
    let (lightness_raw, chroma_raw, hue_raw, alpha_raw) = match parts.as_slice() {
        [lightness, chroma, hue] => (
            lightness.as_str(),
            chroma.as_str(),
            hue.as_str(),
            slash_alpha,
        ),
        [lightness, chroma, hue, alpha] if slash_alpha.is_none() => (
            lightness.as_str(),
            chroma.as_str(),
            hue.as_str(),
            Some(alpha.clone()),
        ),
        _ => return Err(ThemeColorError::Invalid),
    };
    let lightness = if lightness_raw.trim().ends_with('%') {
        parse_css_channel(lightness_raw, 1.0)?
    } else {
        let value = parse_css_number(lightness_raw)?;
        if !(0.0..=1.0).contains(&value) {
            return Err(ThemeColorError::Invalid);
        }
        value
    };
    let chroma = parse_css_number(chroma_raw)?;
    let hue = parse_css_number(hue_raw)?;
    if ![lightness, chroma, hue].into_iter().all(f64::is_finite)
        || !(0.0..=1.0).contains(&lightness)
        || chroma < 0.0
    {
        return Err(ThemeColorError::Invalid);
    }
    let alpha = match alpha_raw {
        Some(raw) => parse_css_alpha(&raw)?,
        None => 255,
    };
    let (red, green, blue) = oklch_to_srgb(lightness, chroma, hue);
    Ok(ThemeColor {
        red,
        green,
        blue,
        alpha,
    })
}

/// T3-compatible OKLCH → 8-bit sRGB: keep L/hue, binary-search chroma into gamut, then transfer.
fn oklch_to_srgb(lightness: f64, chroma: f64, hue_degrees: f64) -> (u8, u8, u8) {
    let mapped_chroma = map_oklch_chroma_into_srgb_gamut(lightness, chroma, hue_degrees);
    let (linear_r, linear_g, linear_b) =
        oklch_to_linear_srgb(lightness, mapped_chroma, hue_degrees);
    (
        clamp_u8_channel(linear_to_srgb_channel(linear_r) * 255.0),
        clamp_u8_channel(linear_to_srgb_channel(linear_g) * 255.0),
        clamp_u8_channel(linear_to_srgb_channel(linear_b) * 255.0),
    )
}

/// Unclamped OKLCH → linear sRGB (CSS Color Module Level 4 / OKLab matrices).
fn oklch_to_linear_srgb(lightness: f64, chroma: f64, hue_degrees: f64) -> (f64, f64, f64) {
    let hue = hue_degrees.to_radians();
    let a = chroma * hue.cos();
    let b = chroma * hue.sin();

    let l_ = lightness + 0.396_337_777_4 * a + 0.215_803_757_3 * b;
    let m_ = lightness - 0.105_561_345_8 * a - 0.063_854_172_8 * b;
    let s_ = lightness - 0.089_484_177_5 * a - 1.291_485_548_0 * b;

    let l = l_ * l_ * l_;
    let m = m_ * m_ * m_;
    let s = s_ * s_ * s_;

    let linear_r = 4.076_741_662_1 * l - 3.307_711_591_3 * m + 0.230_969_929_2 * s;
    let linear_g = -1.268_438_004_6 * l + 2.609_757_401_1 * m - 0.341_319_396_5 * s;
    let linear_b = -0.004_196_086_3 * l - 0.703_418_614_7 * m + 1.707_614_701_0 * s;
    (linear_r, linear_g, linear_b)
}

const OKLCH_LINEAR_GAMUT_MIN: f64 = -0.000_1;
const OKLCH_LINEAR_GAMUT_MAX: f64 = 1.000_1;
const OKLCH_CHROMA_RESOLUTION: f64 = 0.000_001;

fn linear_oklch_in_srgb_gamut(channels: (f64, f64, f64)) -> bool {
    let (red, green, blue) = channels;
    [red, green, blue]
        .into_iter()
        .all(|channel| (OKLCH_LINEAR_GAMUT_MIN..=OKLCH_LINEAR_GAMUT_MAX).contains(&channel))
}

fn map_oklch_chroma_into_srgb_gamut(lightness: f64, chroma: f64, hue_degrees: f64) -> f64 {
    if chroma <= 0.0
        || linear_oklch_in_srgb_gamut(oklch_to_linear_srgb(lightness, chroma, hue_degrees))
    {
        return chroma.max(0.0);
    }
    let mut low = 0.0;
    let mut high = chroma;
    while high - low > OKLCH_CHROMA_RESOLUTION {
        let mid = (low + high) * 0.5;
        if linear_oklch_in_srgb_gamut(oklch_to_linear_srgb(lightness, mid, hue_degrees)) {
            low = mid;
        } else {
            high = mid;
        }
    }
    low
}

fn linear_to_srgb_channel(value: f64) -> f64 {
    let clipped = value.clamp(0.0, 1.0);
    if clipped <= 0.003_130_8 {
        12.92 * clipped
    } else {
        1.055 * clipped.powf(1.0 / 2.4) - 0.055
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ThemeColorError {
    Invalid,
}

impl fmt::Display for ThemeColorError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("theme color must be hex, rgb()/rgba(), or oklch() with optional alpha")
    }
}

impl std::error::Error for ThemeColorError {}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ThemePalette {
    colors: BTreeMap<ThemeColorRole, ThemeColor>,
}

impl ThemePalette {
    pub fn advanced(colors: BTreeMap<ThemeColorRole, ThemeColor>) -> Result<Self, ThemeFileError> {
        for role in ThemeColorRole::ALL {
            if !colors.contains_key(role) {
                return Err(ThemeFileError::MissingRole(*role));
            }
        }
        Ok(Self { colors })
    }

    pub fn color(&self, role: ThemeColorRole) -> ThemeColor {
        *self
            .colors
            .get(&role)
            .expect("validated theme palette contains every semantic role")
    }

    /// The redesign's dark shell, projected from the canonical token module.
    ///
    /// A fresh profile resolves to built-in Classic, and [`Self::tokens`]
    /// overwrites most of what `crate::ui::tokens::dark` produced with these
    /// role values - so anything the palette does not agree with never reaches
    /// the screen. Deriving the roles from that same function rather than
    /// restating them as literals means the two cannot drift.
    ///
    /// The role set is a lossy projection of `ThemeTokens`: several tokens
    /// share one role. Where they disagree the role follows the token with the
    /// most render call sites, and the losers are named in the comments below.
    pub fn redesign_dark() -> Self {
        let tokens = crate::ui::tokens::dark(Density::Comfortable, Scale::Scale100);
        let role = ThemeColor::from_token;
        let pairs = [
            (ThemeColorRole::Canvas, role(tokens.surfaces.canvas)),
            (ThemeColorRole::Chrome, role(tokens.surfaces.raised)),
            (ThemeColorRole::Toolbar, role(tokens.surfaces.raised)),
            (ThemeColorRole::ToolbarForeground, role(tokens.text.primary)),
            (ThemeColorRole::ToolbarBorder, role(tokens.borders.subtle)),
            (
                ThemeColorRole::ToolbarControl,
                role(tokens.surfaces.overlay),
            ),
            (
                ThemeColorRole::ToolbarControlForeground,
                role(tokens.text.primary),
            ),
            // Drives surfaces.hover.
            (
                ThemeColorRole::ToolbarControlHover,
                role(tokens.surfaces.hover),
            ),
            (ThemeColorRole::Surface, role(tokens.surfaces.raised)),
            (ThemeColorRole::SurfaceRaised, role(tokens.surfaces.raised)),
            (
                ThemeColorRole::SurfaceOverlay,
                role(tokens.surfaces.overlay),
            ),
            (ThemeColorRole::Text, role(tokens.text.primary)),
            (ThemeColorRole::TextMuted, role(tokens.text.muted)),
            (ThemeColorRole::Border, role(tokens.borders.subtle)),
            (ThemeColorRole::Input, role(tokens.borders.default)),
            // Drives borders.focus, which has the render call sites;
            // actions.primary.focus rides along and has none.
            (ThemeColorRole::Focus, role(tokens.borders.focus)),
            // Drives actions.primary.default; borders.selection rides along
            // and becomes the action colour rather than the neutral outline.
            (
                ThemeColorRole::Accent,
                role(tokens.actions.primary.default.background),
            ),
            (
                ThemeColorRole::AccentForeground,
                role(tokens.text.on_accent),
            ),
            (ThemeColorRole::Secondary, role(tokens.surfaces.overlay)),
            (
                ThemeColorRole::SecondaryForeground,
                role(tokens.text.secondary),
            ),
            // Drives surfaces.disabled; borders.disabled and the disabled
            // action backgrounds ride along and flatten onto it.
            (ThemeColorRole::Muted, role(tokens.surfaces.disabled)),
            (ThemeColorRole::MutedForeground, role(tokens.text.muted)),
            (ThemeColorRole::Placeholder, role(tokens.text.disabled)),
            (ThemeColorRole::SecondaryLabel, role(tokens.text.secondary)),
            (ThemeColorRole::IconMuted, role(tokens.text.muted)),
            // Drives status.destructive; actions.destructive.default rides along.
            (ThemeColorRole::Error, role(tokens.status.destructive)),
            (
                ThemeColorRole::ErrorForeground,
                role(tokens.status.destructive_foreground),
            ),
            (
                ThemeColorRole::ErrorSurface,
                role(tokens.status.destructive_surface),
            ),
            (ThemeColorRole::Warning, role(tokens.status.warning)),
            (
                ThemeColorRole::WarningForeground,
                role(tokens.status.warning_foreground),
            ),
            (
                ThemeColorRole::WarningSurface,
                role(tokens.status.warning_surface),
            ),
            (ThemeColorRole::Update, role(tokens.status.external)),
            (
                ThemeColorRole::UpdateForeground,
                role(tokens.status.external_foreground),
            ),
            (
                ThemeColorRole::UpdateSurface,
                role(tokens.status.external_surface),
            ),
            (
                ThemeColorRole::AccentSurface,
                role(tokens.actions.primary.selected.background),
            ),
            (
                ThemeColorRole::AccentSurfaceForeground,
                role(tokens.actions.primary.default.foreground),
            ),
            (
                ThemeColorRole::MessageSurface,
                role(tokens.status.external_surface),
            ),
            (
                ThemeColorRole::MessageForeground,
                role(tokens.status.external_foreground),
            ),
            (ThemeColorRole::MessageAction, role(tokens.status.external)),
            (
                ThemeColorRole::MessageActionForeground,
                role(tokens.text.on_accent),
            ),
            (
                ThemeColorRole::MessageActionHover,
                role(tokens.actions.primary.hover.background),
            ),
            // Drives surfaces.sunken, which is the stream well - a step above
            // the terminal's own background, so the two roles differ here.
            (ThemeColorRole::CodeBackground, role(tokens.surfaces.sunken)),
            (ThemeColorRole::CodeForeground, role(tokens.text.primary)),
            (ThemeColorRole::Sidebar, role(tokens.surfaces.raised)),
            (
                ThemeColorRole::SidebarForeground,
                role(tokens.text.on_selection),
            ),
            (
                ThemeColorRole::SidebarMutedForeground,
                role(tokens.text.muted),
            ),
            (
                ThemeColorRole::SidebarControlSurface,
                role(tokens.surfaces.overlay),
            ),
            (ThemeColorRole::SidebarRowHover, role(tokens.surfaces.hover)),
            (
                ThemeColorRole::SidebarRowActive,
                role(tokens.surfaces.selection),
            ),
            // Drives surfaces.selection. A neutral grey step, never an accent
            // mix: colour on this shell is reserved for needs-you.
            (
                ThemeColorRole::SidebarRowSelected,
                role(tokens.surfaces.selection),
            ),
            (ThemeColorRole::SidebarBorder, role(tokens.borders.strong)),
            (
                ThemeColorRole::TerminalBackground,
                role(tokens.terminal.background),
            ),
            (
                ThemeColorRole::TerminalForeground,
                role(tokens.terminal.foreground),
            ),
            (ThemeColorRole::TerminalCursor, role(tokens.terminal.cursor)),
            (
                ThemeColorRole::TerminalSelection,
                role(tokens.terminal.selection),
            ),
            // These two roles keep their `terminal*` wire names -- ten
            // shipped palettes and every user-saved theme spell them that way
            // -- but they are no longer terminal-only: `ui::scrollbar` paints
            // every scrollable shell surface from the same pair, which is what
            // makes "one look" survive a managed theme. Their values come from
            // the scrollbar tokens rather than borrowed border roles, so the
            // colour contract has exactly one author.
            (
                ThemeColorRole::TerminalScrollbar,
                role(tokens.scrollbar.on_dark.thumb_idle),
            ),
            (
                ThemeColorRole::TerminalScrollbarHover,
                role(tokens.scrollbar.on_dark.thumb_hover),
            ),
        ];
        Self::advanced(pairs.into_iter().collect())
            .expect("redesign dark palette must declare every semantic role")
    }

    pub fn managed(appearance: ThemeAppearance, canvas: ThemeColor, accent: ThemeColor) -> Self {
        let white = ThemeColor::rgb(255, 255, 255);
        let black = ThemeColor::rgb(18, 18, 22);
        let foreground = readable_foreground(canvas);
        let accent_foreground = readable_foreground(accent);
        let toward_foreground = |amount| canvas.mix(foreground, amount);
        let surface = toward_foreground(if appearance == ThemeAppearance::Dark {
            0.055
        } else {
            0.025
        });
        let raised = toward_foreground(if appearance == ThemeAppearance::Dark {
            0.105
        } else {
            0.045
        });
        let overlay = toward_foreground(if appearance == ThemeAppearance::Dark {
            0.16
        } else {
            0.065
        });
        let muted_text = readable_muted_foreground(canvas, foreground);
        let border = toward_foreground(if appearance == ThemeAppearance::Dark {
            0.19
        } else {
            0.13
        });
        let secondary = toward_foreground(if appearance == ThemeAppearance::Dark {
            0.12
        } else {
            0.06
        });
        let accent_surface = canvas.mix(
            accent,
            if appearance == ThemeAppearance::Dark {
                0.24
            } else {
                0.12
            },
        );
        let message_surface = canvas.mix(
            accent,
            if appearance == ThemeAppearance::Dark {
                0.16
            } else {
                0.09
            },
        );
        let error = if appearance == ThemeAppearance::Dark {
            ThemeColor::rgb(235, 104, 121)
        } else {
            ThemeColor::rgb(193, 49, 68)
        };
        let warning = if appearance == ThemeAppearance::Dark {
            ThemeColor::rgb(238, 184, 82)
        } else {
            ThemeColor::rgb(159, 99, 10)
        };
        let update = if accent.contrast(canvas) >= 3.0 {
            accent
        } else if appearance == ThemeAppearance::Dark {
            accent.mix(white, 0.34)
        } else {
            accent.mix(black, 0.28)
        };
        let sidebar = canvas.mix(
            accent,
            if appearance == ThemeAppearance::Dark {
                0.055
            } else {
                0.035
            },
        );
        let sidebar_foreground = readable_foreground(sidebar);
        let sidebar_muted = readable_muted_foreground(sidebar, sidebar_foreground);
        let terminal_background = canvas.mix(
            black,
            if appearance == ThemeAppearance::Dark {
                0.12
            } else {
                0.025
            },
        );
        let terminal_foreground = readable_foreground(terminal_background);

        let pairs = [
            (ThemeColorRole::Canvas, canvas),
            (ThemeColorRole::Chrome, canvas),
            (ThemeColorRole::Toolbar, canvas),
            (ThemeColorRole::ToolbarForeground, foreground),
            (ThemeColorRole::ToolbarBorder, border),
            (ThemeColorRole::ToolbarControl, secondary),
            (ThemeColorRole::ToolbarControlForeground, foreground),
            (ThemeColorRole::ToolbarControlHover, accent_surface),
            (ThemeColorRole::Surface, surface),
            (ThemeColorRole::SurfaceRaised, raised),
            (ThemeColorRole::SurfaceOverlay, overlay),
            (ThemeColorRole::Text, foreground),
            (ThemeColorRole::TextMuted, muted_text),
            (ThemeColorRole::Border, border),
            (ThemeColorRole::Input, toward_foreground(0.24)),
            (ThemeColorRole::Focus, accent),
            (ThemeColorRole::Accent, accent),
            (ThemeColorRole::AccentForeground, accent_foreground),
            (ThemeColorRole::Secondary, secondary),
            (ThemeColorRole::SecondaryForeground, foreground),
            (ThemeColorRole::Muted, toward_foreground(0.09)),
            (ThemeColorRole::MutedForeground, muted_text),
            (ThemeColorRole::Placeholder, muted_text),
            (ThemeColorRole::SecondaryLabel, muted_text),
            (ThemeColorRole::IconMuted, muted_text),
            (ThemeColorRole::Error, error),
            (ThemeColorRole::ErrorForeground, readable_foreground(error)),
            (ThemeColorRole::ErrorSurface, canvas.mix(error, 0.13)),
            (ThemeColorRole::Warning, warning),
            (
                ThemeColorRole::WarningForeground,
                readable_foreground(warning),
            ),
            (ThemeColorRole::WarningSurface, canvas.mix(warning, 0.13)),
            (ThemeColorRole::Update, update),
            (
                ThemeColorRole::UpdateForeground,
                readable_foreground(update),
            ),
            (ThemeColorRole::UpdateSurface, canvas.mix(update, 0.14)),
            (ThemeColorRole::AccentSurface, accent_surface),
            (
                ThemeColorRole::AccentSurfaceForeground,
                readable_foreground(accent_surface),
            ),
            (ThemeColorRole::MessageSurface, message_surface),
            (
                ThemeColorRole::MessageForeground,
                readable_foreground(message_surface),
            ),
            (ThemeColorRole::MessageAction, update),
            (
                ThemeColorRole::MessageActionForeground,
                readable_foreground(update),
            ),
            (
                ThemeColorRole::MessageActionHover,
                update.mix(foreground, 0.12),
            ),
            (ThemeColorRole::CodeBackground, terminal_background),
            (ThemeColorRole::CodeForeground, terminal_foreground),
            (ThemeColorRole::Sidebar, sidebar),
            (ThemeColorRole::SidebarForeground, sidebar_foreground),
            (ThemeColorRole::SidebarMutedForeground, sidebar_muted),
            (
                ThemeColorRole::SidebarControlSurface,
                sidebar.mix(sidebar_foreground, 0.09),
            ),
            (ThemeColorRole::SidebarRowHover, sidebar.mix(accent, 0.12)),
            (ThemeColorRole::SidebarRowActive, sidebar.mix(accent, 0.19)),
            (
                ThemeColorRole::SidebarRowSelected,
                sidebar.mix(accent, 0.25),
            ),
            (
                ThemeColorRole::SidebarBorder,
                sidebar.mix(sidebar_foreground, 0.16),
            ),
            (ThemeColorRole::TerminalBackground, terminal_background),
            (ThemeColorRole::TerminalForeground, terminal_foreground),
            (ThemeColorRole::TerminalCursor, update),
            (
                ThemeColorRole::TerminalSelection,
                terminal_background.mix(accent, 0.28),
            ),
            (
                ThemeColorRole::TerminalScrollbar,
                terminal_background.mix(terminal_foreground, 0.2),
            ),
            (
                ThemeColorRole::TerminalScrollbarHover,
                terminal_background.mix(terminal_foreground, 0.34),
            ),
        ];
        Self {
            colors: pairs.into_iter().collect(),
        }
    }

    pub fn tokens(&self, density: Density, scale: Scale) -> ThemeTokens {
        let appearance = if self.color(ThemeColorRole::Canvas).luminance() >= 0.45 {
            ThemeAppearance::Light
        } else {
            ThemeAppearance::Dark
        };
        let mut tokens = theme(appearance.mode(), density, scale);
        let color = |role| self.color(role).opaque();

        tokens.surfaces.canvas = color(ThemeColorRole::Canvas);
        tokens.surfaces.raised = color(ThemeColorRole::SurfaceRaised);
        tokens.surfaces.overlay = color(ThemeColorRole::SurfaceOverlay);
        tokens.surfaces.sunken = color(ThemeColorRole::CodeBackground);
        tokens.surfaces.hover = color(ThemeColorRole::ToolbarControlHover);
        tokens.surfaces.selection = color(ThemeColorRole::SidebarRowSelected);
        tokens.surfaces.disabled = color(ThemeColorRole::Muted);
        tokens.text.primary = color(ThemeColorRole::Text);
        tokens.text.secondary = color(ThemeColorRole::SecondaryForeground);
        tokens.text.muted = color(ThemeColorRole::MutedForeground);
        tokens.text.disabled = color(ThemeColorRole::Placeholder);
        // `text.inverse` is NOT `AccentForeground`. It is the light-on-dark
        // caption the terminal dock paints over its modal backdrop, and while
        // the accent was a mid-saturation fill the two happened to agree. The
        // redesign's accent foreground is the canvas colour, which would paint
        // that caption at 1.015:1 on the backdrop -- so this keeps the token
        // module's value. Retinting it for a managed theme is a role of its own,
        // not a borrow from this one.
        tokens.text.on_accent = color(ThemeColorRole::AccentForeground);
        tokens.text.on_selection = color(ThemeColorRole::SidebarForeground);
        // A managed or user palette owns the scrollbar's two thumb colours
        // through the roles above; the track keeps the token module's value
        // because no palette declares a role for it and borrowing one would
        // make the track track something it is not.
        // The two roles keep their `terminal*` wire names but drive the whole
        // app now. They carry the DARK-ground pair: the terminal plane is dark
        // in every shipped palette, and a palette that declared a light one
        // would still resolve through `ScrollbarTokens::colors_on`. The light-
        // ground pair keeps the token module's value because no palette
        // declares a role for it.
        tokens.scrollbar.on_dark.thumb_idle = color(ThemeColorRole::TerminalScrollbar);
        tokens.scrollbar.on_dark.thumb_hover = color(ThemeColorRole::TerminalScrollbarHover);
        tokens.borders.subtle = color(ThemeColorRole::Border);
        tokens.borders.default = color(ThemeColorRole::Input);
        tokens.borders.strong = color(ThemeColorRole::SidebarBorder);
        tokens.borders.focus = color(ThemeColorRole::Focus);
        // `borders.selection` is a neutral outline, not the accent. Taking it
        // from `Accent` made every selection outline the action colour, which is
        // the opposite of the rule that colour is reserved for state that needs
        // you -- and under the redesign it would draw the outline in the primary
        // button's near-white fill.
        tokens.borders.disabled = color(ThemeColorRole::Muted);

        set_interaction_colors(
            &mut tokens.actions.primary,
            color(ThemeColorRole::Accent),
            color(ThemeColorRole::MessageActionHover),
            color(ThemeColorRole::Focus),
            color(ThemeColorRole::AccentSurface),
            color(ThemeColorRole::Muted),
            color(ThemeColorRole::AccentForeground),
        );
        set_interaction_colors(
            &mut tokens.actions.destructive,
            color(ThemeColorRole::Error),
            mix_color(
                color(ThemeColorRole::Error),
                color(ThemeColorRole::Text),
                0.12,
            ),
            color(ThemeColorRole::Error),
            color(ThemeColorRole::ErrorSurface),
            color(ThemeColorRole::Muted),
            color(ThemeColorRole::ErrorForeground),
        );

        // status.attention, status.success, text.emphasis and every terminal
        // ANSI slot have no role of their own, so they stay as the token module
        // set them. Attention used to be aliased onto Warning, which quietly
        // replaced the redesign's amber with whatever a palette called warning.
        tokens.status.warning = color(ThemeColorRole::Warning);
        tokens.status.warning_surface = color(ThemeColorRole::WarningSurface);
        tokens.status.warning_foreground = color(ThemeColorRole::WarningForeground);
        tokens.status.destructive = color(ThemeColorRole::Error);
        tokens.status.destructive_surface = color(ThemeColorRole::ErrorSurface);
        tokens.status.destructive_foreground = color(ThemeColorRole::ErrorForeground);
        tokens.status.external = color(ThemeColorRole::Update);
        tokens.status.external_surface = color(ThemeColorRole::UpdateSurface);
        tokens.status.external_foreground = color(ThemeColorRole::UpdateForeground);
        tokens.terminal.background = color(ThemeColorRole::TerminalBackground);
        tokens.terminal.foreground = color(ThemeColorRole::TerminalForeground);
        tokens.terminal.cursor = color(ThemeColorRole::TerminalCursor);
        tokens.terminal.selection = color(ThemeColorRole::TerminalSelection);
        tokens
    }
}

fn set_interaction_colors(
    tokens: &mut crate::ui::tokens::InteractionStateTokens,
    default: Color,
    hover: Color,
    focus: Color,
    selected: Color,
    disabled: Color,
    foreground: Color,
) {
    tokens.default = ActionStateTokens {
        foreground,
        background: default,
        border: default,
    };
    tokens.hover = ActionStateTokens {
        foreground,
        background: hover,
        border: hover,
    };
    tokens.focus = ActionStateTokens {
        foreground,
        background: focus,
        border: focus,
    };
    tokens.selected = ActionStateTokens {
        foreground,
        background: selected,
        border: selected,
    };
    // The disabled state keeps whatever foreground the token module gave it.
    // One accent foreground for all five states is only sound while every fill
    // has the same polarity, and the disabled fill never does: it is
    // `surfaces.disabled`, a near-black on the dark shell, so the redesign's
    // dark accent foreground would paint disabled labels at 1.144:1. This is
    // also what makes `action_primary_disabled_on_surface` describe a real pair.
    tokens.disabled = ActionStateTokens {
        foreground: tokens.disabled.foreground,
        background: disabled,
        border: disabled,
    };
}

fn readable_foreground(background: ThemeColor) -> ThemeColor {
    let light = ThemeColor::rgb(255, 250, 255);
    let dark = ThemeColor::rgb(25, 20, 28);
    if light.contrast(background) >= dark.contrast(background) {
        light
    } else {
        dark
    }
}

fn readable_muted_foreground(background: ThemeColor, foreground: ThemeColor) -> ThemeColor {
    let mut amount = 0.55;
    loop {
        let candidate = background.mix(foreground, amount);
        if candidate.contrast(background) >= 4.5 || amount >= 1.0 {
            return candidate;
        }
        amount += 0.025;
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ThemeDefinition {
    pub id: String,
    pub label: String,
    palettes: BTreeMap<ThemeAppearance, ThemePalette>,
    pub managed: bool,
    pub metadata: JsonMap<String, Value>,
}

impl ThemeDefinition {
    pub fn is_built_in(&self) -> bool {
        is_built_in_theme_id(&self.id)
    }

    pub fn managed(
        id: impl Into<String>,
        label: impl Into<String>,
        appearance: ThemeAppearance,
        canvas: ThemeColor,
        accent: ThemeColor,
    ) -> Self {
        Self {
            id: id.into(),
            label: label.into(),
            palettes: BTreeMap::from([(
                appearance,
                ThemePalette::managed(appearance, canvas, accent),
            )]),
            managed: true,
            metadata: JsonMap::new(),
        }
    }

    pub fn palette(&self, appearance: ThemeAppearance) -> Option<&ThemePalette> {
        self.palettes.get(&appearance)
    }

    fn paired_managed(
        id: &str,
        label: &str,
        light_canvas: &str,
        light_accent: &str,
        dark_canvas: &str,
        dark_accent: &str,
    ) -> Self {
        let palettes = BTreeMap::from([
            (
                ThemeAppearance::Light,
                ThemePalette::managed(
                    ThemeAppearance::Light,
                    ThemeColor::from_hex(light_canvas).expect("built-in light canvas"),
                    ThemeColor::from_hex(light_accent).expect("built-in light accent"),
                ),
            ),
            (
                ThemeAppearance::Dark,
                ThemePalette::managed(
                    ThemeAppearance::Dark,
                    ThemeColor::from_hex(dark_canvas).expect("built-in dark canvas"),
                    ThemeColor::from_hex(dark_accent).expect("built-in dark accent"),
                ),
            ),
        ]);
        Self {
            id: id.to_string(),
            label: label.to_string(),
            palettes,
            managed: true,
            metadata: JsonMap::new(),
        }
    }

    fn paired_semantic(
        id: &str,
        label: &str,
        light_roles: &[(&str, &str)],
        dark_roles: &[(&str, &str)],
    ) -> Self {
        Self::paired_palettes(
            id,
            label,
            palette_from_oklch_roles(light_roles),
            palette_from_oklch_roles(dark_roles),
        )
    }

    /// Pair two already-built palettes, for a theme whose halves do not both
    /// come from a role table.
    fn paired_palettes(id: &str, label: &str, light: ThemePalette, dark: ThemePalette) -> Self {
        Self {
            id: id.to_string(),
            label: label.to_string(),
            palettes: BTreeMap::from([
                (ThemeAppearance::Light, light),
                (ThemeAppearance::Dark, dark),
            ]),
            managed: false,
            metadata: JsonMap::new(),
        }
    }
}

fn palette_from_oklch_roles(roles: &[(&str, &str)]) -> ThemePalette {
    let mut colors = BTreeMap::new();
    for &(name, value) in roles {
        let role = ThemeColorRole::parse(name)
            .unwrap_or_else(|| panic!("built-in theme role `{name}` must match ThemeColorRole"));
        let color = ThemeColor::parse(value).unwrap_or_else(|_| {
            panic!("built-in theme color for `{name}` must parse as ThemeColor")
        });
        colors.insert(role, color);
    }
    ThemePalette::advanced(colors)
        .expect("built-in semantic theme must declare every ThemeColorRole")
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ThemeLibrary {
    themes: Vec<ThemeDefinition>,
}

impl ThemeLibrary {
    pub fn built_in() -> Self {
        Self {
            themes: built_in_themes(),
        }
    }

    pub fn with_custom(custom: Vec<ThemeDefinition>) -> Result<Self, ThemeLibraryError> {
        if custom.len() > MAX_CUSTOM_THEMES {
            return Err(ThemeLibraryError::TooManyThemes);
        }
        let built_ins = built_in_themes();
        let mut ids = built_ins
            .iter()
            .map(|theme| theme.id.clone())
            .collect::<BTreeSet<_>>();
        for theme in &custom {
            validate_identity(&theme.id, &theme.label).map_err(ThemeLibraryError::InvalidTheme)?;
            if !ids.insert(theme.id.clone()) {
                return Err(ThemeLibraryError::DuplicateId(theme.id.clone()));
            }
        }
        Ok(Self {
            themes: built_ins.into_iter().chain(custom).collect(),
        })
    }

    pub fn themes(&self) -> &[ThemeDefinition] {
        &self.themes
    }

    pub fn get(&self, id: &str) -> Option<&ThemeDefinition> {
        self.themes.iter().find(|theme| theme.id == id)
    }

    fn custom_themes(&self) -> impl Iterator<Item = &ThemeDefinition> {
        self.themes
            .iter()
            .filter(|theme| !is_built_in_theme_id(&theme.id))
    }
}

// Exact T3 shared-palette OKLCH sources for semantic built-ins.
// Parsed through ThemeColor::parse (T3-compatible OKLCH gamut map) into ThemeColor.
const T3_CHAT_LIGHT_ROLES: &[(&str, &str)] = &[
    ("canvas", "oklch(0.982446 0.010114 325.653)"),
    ("chrome", "oklch(0.982446 0.010114 325.653)"),
    ("toolbar", "oklch(0.982446 0.010114 325.653)"),
    ("toolbarForeground", "oklch(0.325698 0.116116 325.037)"),
    ("toolbarBorder", "oklch(0.856784 0.082879 328.911)"),
    ("toolbarControl", "oklch(0.939552 0.024286 321.664)"),
    (
        "toolbarControlForeground",
        "oklch(0.325698 0.116116 325.037)",
    ),
    ("toolbarControlHover", "oklch(0.884525 0.041658 337.177)"),
    ("surface", "oklch(0.971835 0.012884 321.894)"),
    ("surfaceRaised", "oklch(0.988235 0.005049 325.615)"),
    ("surfaceOverlay", "oklch(1 0 0)"),
    ("text", "oklch(0.325698 0.116116 325.037)"),
    ("textMuted", "oklch(0.494754 0.190937 354.544)"),
    ("border", "oklch(0.923531 0.021247 328.096)"),
    ("input", "oklch(0.851713 0.055822 336.6)"),
    ("focus", "oklch(0.591646 0.217985 0.584)"),
    ("accent", "oklch(0.591646 0.217985 0.584)"),
    ("accentForeground", "oklch(1 0 0)"),
    ("secondary", "oklch(0.869588 0.06751 334.899)"),
    ("secondaryForeground", "oklch(0.444777 0.134061 324.799)"),
    ("muted", "oklch(0.802407 0.090963 345.892)"),
    ("mutedForeground", "oklch(0.428932 0.163929 354.332)"),
    ("placeholder", "oklch(0.549927 0.090215 323.149)"),
    ("secondaryLabel", "oklch(0.494754 0.190937 354.544)"),
    ("iconMuted", "oklch(0.494754 0.190937 354.544)"),
    ("error", "oklch(0.627117 0.248974 7.734)"),
    ("errorForeground", "oklch(0.458704 0.169677 3.815)"),
    ("errorSurface", "oklch(0.942787 0.032076 344.963)"),
    ("warning", "oklch(0.76859 0.164659 70.08)"),
    ("warningForeground", "oklch(0.54612 0.143036 48.949)"),
    ("warningSurface", "oklch(0.962901 0.015297 48.56)"),
    ("update", "oklch(0.591646 0.217985 0.584)"),
    ("updateForeground", "oklch(0.494754 0.190937 354.544)"),
    ("updateSurface", "oklch(0.930264 0.036194 341.45)"),
    ("accentSurface", "oklch(0.939552 0.024286 321.664)"),
    (
        "accentSurfaceForeground",
        "oklch(0.396296 0.025134 285.196)",
    ),
    ("messageSurface", "oklch(0.926746 0.037898 332.6)"),
    ("messageForeground", "oklch(0.354591 0.093575 307.568)"),
    ("messageAction", "oklch(0.591646 0.217985 0.584)"),
    ("messageActionForeground", "oklch(1 0 0)"),
    ("messageActionHover", "oklch(0.539042 0.197866 0.305)"),
    ("codeBackground", "oklch(0.953855 0.019695 315.668)"),
    ("codeForeground", "oklch(0.445128 0.13005 307.026)"),
    ("sidebar", "oklch(0.928886 0.031178 322.592)"),
    ("sidebarForeground", "oklch(0.396296 0.025134 285.196)"),
    ("sidebarMutedForeground", "oklch(0.494754 0.190937 354.544)"),
    ("sidebarControlSurface", "oklch(0.978851 0.001321 106.424)"),
    ("sidebarRowHover", "oklch(0.978851 0.001321 106.424)"),
    ("sidebarRowActive", "oklch(0.978851 0.001321 106.424)"),
    ("sidebarRowSelected", "oklch(0.978851 0.001321 106.424)"),
    ("sidebarBorder", "oklch(0.938313 0.002552 48.717)"),
    ("terminalBackground", "oklch(0.982446 0.010114 325.653)"),
    ("terminalForeground", "oklch(0.325698 0.116116 325.037)"),
    ("terminalCursor", "oklch(0.591646 0.217985 0.584)"),
    ("terminalSelection", "oklch(0.869588 0.06751 334.899)"),
    ("terminalScrollbar", "oklch(0.851713 0.055822 336.6)"),
    ("terminalScrollbarHover", "oklch(0.802407 0.090963 345.892)"),
];

const T3_CHAT_DARK_ROLES: &[(&str, &str)] = &[
    ("canvas", "oklch(0.22813 0.020366 307.469)"),
    ("chrome", "oklch(0.22813 0.020366 307.469)"),
    ("toolbar", "oklch(0.22813 0.020366 307.469)"),
    ("toolbarForeground", "oklch(0.980735 0.004092 301.426)"),
    ("toolbarBorder", "oklch(0.266943 0.015262 302.425)"),
    ("toolbarControl", "oklch(0.313674 0.030572 310.061)"),
    (
        "toolbarControlForeground",
        "oklch(0.848252 0.038248 307.961)",
    ),
    ("toolbarControlHover", "oklch(0.364912 0.050794 308.491)"),
    ("surface", "oklch(0.267101 0.02016 311.799)"),
    ("surfaceRaised", "oklch(0.279864 0.021572 309.532)"),
    ("surfaceOverlay", "oklch(0.154761 0.01316 338.901)"),
    ("text", "oklch(0.980735 0.004092 301.426)"),
    ("textMuted", "oklch(0.880303 0.03077 342.696)"),
    ("border", "oklch(0.266943 0.015262 302.425)"),
    ("input", "oklch(0.266817 0.02897 344.461)"),
    ("focus", "oklch(0.591646 0.217985 0.584)"),
    ("accent", "oklch(0.460685 0.185347 4.099)"),
    ("accentForeground", "oklch(0.901233 0.057189 343.694)"),
    ("secondary", "oklch(0.313674 0.030572 310.061)"),
    ("secondaryForeground", "oklch(0.848252 0.038248 307.961)"),
    ("muted", "oklch(0.360924 0.021469 316.83)"),
    ("mutedForeground", "oklch(0.880303 0.03077 342.696)"),
    ("placeholder", "oklch(0.657087 0.028226 307.985)"),
    ("secondaryLabel", "oklch(0.880303 0.03077 342.696)"),
    ("iconMuted", "oklch(0.848252 0.038248 307.961)"),
    ("error", "oklch(0.458704 0.169677 3.815)"),
    ("errorForeground", "oklch(0.901233 0.057189 343.694)"),
    ("errorSurface", "oklch(0.259022 0.04799 340.062)"),
    ("warning", "oklch(0.76859 0.164659 70.08)"),
    ("warningForeground", "oklch(0.836861 0.164422 84.429)"),
    ("warningSurface", "oklch(0.321706 0.036256 60.806)"),
    ("update", "oklch(0.460685 0.185347 4.099)"),
    ("updateForeground", "oklch(0.901233 0.057189 343.694)"),
    ("updateSurface", "oklch(0.256077 0.063004 342.914)"),
    ("accentSurface", "oklch(0.364912 0.050794 308.491)"),
    (
        "accentSurfaceForeground",
        "oklch(0.964695 0.009139 341.803)",
    ),
    ("messageSurface", "oklch(0.273791 0.025541 309.079)"),
    ("messageForeground", "oklch(0.949872 0.021269 306.838)"),
    ("messageAction", "oklch(0.460685 0.185347 4.099)"),
    (
        "messageActionForeground",
        "oklch(0.901233 0.057189 343.694)",
    ),
    ("messageActionHover", "oklch(0.458754 0.184639 3.857)"),
    ("codeBackground", "oklch(0.22813 0.020366 307.469)"),
    ("codeForeground", "oklch(0.848703 0.064239 306.645)"),
    ("sidebar", "oklch(0.185778 0.019368 322.159)"),
    ("sidebarForeground", "oklch(0.967434 0.001326 286.375)"),
    ("sidebarMutedForeground", "oklch(0.880303 0.03077 342.696)"),
    ("sidebarControlSurface", "oklch(0.23366 0.026081 338.196)"),
    ("sidebarRowHover", "oklch(0.23366 0.026081 338.196)"),
    ("sidebarRowActive", "oklch(0.23366 0.026081 338.196)"),
    ("sidebarRowSelected", "oklch(0.23366 0.026081 338.196)"),
    ("sidebarBorder", "oklch(0.269132 0.030766 351.067)"),
    ("terminalBackground", "oklch(0.22813 0.020366 307.469)"),
    ("terminalForeground", "oklch(0.980735 0.004092 301.426)"),
    ("terminalCursor", "oklch(0.591646 0.217985 0.584)"),
    ("terminalSelection", "oklch(0.313674 0.030572 310.061)"),
    ("terminalScrollbar", "oklch(0.266817 0.02897 344.461)"),
    ("terminalScrollbarHover", "oklch(0.360924 0.021469 316.83)"),
];

const GROVE_LIGHT_ROLES: &[(&str, &str)] = &[
    ("canvas", "oklch(0.972369 0.005497 157.15)"),
    ("chrome", "oklch(0.972369 0.005497 157.15)"),
    ("toolbar", "oklch(0.972369 0.005497 157.15)"),
    ("toolbarForeground", "oklch(0.222003 0.03479 328.979)"),
    ("toolbarBorder", "oklch(0.909438 0.021521 164.612)"),
    ("toolbarControl", "oklch(0.936464 0.014601 163.554)"),
    (
        "toolbarControlForeground",
        "oklch(0.222003 0.03479 328.979)",
    ),
    ("toolbarControlHover", "oklch(0.909438 0.021521 164.612)"),
    ("surface", "oklch(0.972369 0.005497 157.15)"),
    ("surfaceRaised", "oklch(0.949276 0.004496 159.002)"),
    ("surfaceOverlay", "oklch(0.932695 0.003778 160.944)"),
    ("text", "oklch(0.222003 0.03479 328.979)"),
    ("textMuted", "oklch(0.540472 0.014944 326.176)"),
    ("border", "oklch(0.864831 0.01312 167.255)"),
    ("input", "oklch(0.829746 0.016084 168.234)"),
    ("focus", "oklch(0.523295 0.112292 158.089)"),
    ("accent", "oklch(0.523295 0.112292 158.089)"),
    ("accentForeground", "oklch(0.990339 0.008411 325.64)"),
    ("secondary", "oklch(0.936464 0.014601 163.554)"),
    ("secondaryForeground", "oklch(0.222003 0.03479 328.979)"),
    ("muted", "oklch(0.945455 0.012308 162.879)"),
    ("mutedForeground", "oklch(0.527266 0.012309 320.683)"),
    ("placeholder", "oklch(0.529681 0.01551 326.299)"),
    ("secondaryLabel", "oklch(0.540472 0.014944 326.176)"),
    ("iconMuted", "oklch(0.540472 0.014944 326.176)"),
    ("error", "oklch(0.637823 0.237287 25.436)"),
    ("errorForeground", "oklch(0.509494 0.208583 28.513)"),
    ("errorSurface", "oklch(0.936968 0.014243 26.295)"),
    ("warning", "oklch(0.772406 0.172798 65.367)"),
    ("warningForeground", "oklch(0.545036 0.155019 45.359)"),
    ("warningSurface", "oklch(0.953175 0.02009 93.379)"),
    ("update", "oklch(0.523295 0.112292 158.089)"),
    ("updateForeground", "oklch(0.388012 0.080082 158.768)"),
    ("updateSurface", "oklch(0.900411 0.02384 164.795)"),
    ("accentSurface", "oklch(0.909438 0.021521 164.612)"),
    ("accentSurfaceForeground", "oklch(0.222003 0.03479 328.979)"),
    ("messageSurface", "oklch(0.891377 0.026164 164.929)"),
    ("messageForeground", "oklch(0.222003 0.03479 328.979)"),
    ("messageAction", "oklch(0.535028 0.106403 77.549)"),
    ("messageActionForeground", "oklch(0.990339 0.008411 325.64)"),
    ("messageActionHover", "oklch(0.488753 0.096536 77.829)"),
    ("codeBackground", "oklch(0.955888 0.004783 158.391)"),
    ("codeForeground", "oklch(0.222003 0.03479 328.979)"),
    ("sidebar", "oklch(0.936464 0.014601 163.554)"),
    ("sidebarForeground", "oklch(0.222003 0.03479 328.979)"),
    ("sidebarMutedForeground", "oklch(0.515606 0.011938 318.897)"),
    ("sidebarControlSurface", "oklch(0.88585 0.011734 166.331)"),
    ("sidebarRowHover", "oklch(0.886676 0.027374 164.983)"),
    ("sidebarRowActive", "oklch(0.85335 0.03597 165.158)"),
    ("sidebarRowSelected", "oklch(0.836654 0.040284 165.149)"),
    ("sidebarBorder", "oklch(0.860274 0.010287 168.339)"),
    ("terminalBackground", "oklch(0.972369 0.005497 157.15)"),
    ("terminalForeground", "oklch(0.222003 0.03479 328.979)"),
    ("terminalCursor", "oklch(0.523295 0.112292 158.089)"),
    ("terminalSelection", "oklch(0.891377 0.026164 164.929)"),
    ("terminalScrollbar", "oklch(0.824752 0.001392 294.641)"),
    ("terminalScrollbarHover", "oklch(0.755495 0.004415 318.776)"),
];

const GROVE_DARK_ROLES: &[(&str, &str)] = &[
    ("canvas", "oklch(0.260865 0.02152 162.75)"),
    ("chrome", "oklch(0.260865 0.02152 162.75)"),
    ("toolbar", "oklch(0.260865 0.02152 162.75)"),
    ("toolbarForeground", "oklch(0.990339 0.008411 325.64)"),
    ("toolbarBorder", "oklch(0.464636 0.066083 158.72)"),
    ("toolbarControl", "oklch(0.380487 0.048313 159.608)"),
    (
        "toolbarControlForeground",
        "oklch(0.990339 0.008411 325.64)",
    ),
    ("toolbarControlHover", "oklch(0.437021 0.060312 158.962)"),
    ("surface", "oklch(0.260865 0.02152 162.75)"),
    ("surfaceRaised", "oklch(0.363192 0.016572 165.32)"),
    ("surfaceOverlay", "oklch(0.411828 0.014378 166.627)"),
    ("text", "oklch(0.990339 0.008411 325.64)"),
    ("textMuted", "oklch(0.666747 0.004239 187.292)"),
    ("border", "oklch(0.457475 0.044046 160.971)"),
    ("input", "oklch(0.519849 0.049896 160.863)"),
    ("focus", "oklch(0.796228 0.133058 157.319)"),
    ("accent", "oklch(0.796228 0.133058 157.319)"),
    ("accentForeground", "oklch(0.222003 0.03479 328.979)"),
    ("secondary", "oklch(0.380487 0.048313 159.608)"),
    ("secondaryForeground", "oklch(0.990339 0.008411 325.64)"),
    ("muted", "oklch(0.339728 0.039456 160.274)"),
    ("mutedForeground", "oklch(0.715427 0.010896 171.428)"),
    ("placeholder", "oklch(0.739243 0.002222 223.225)"),
    ("secondaryLabel", "oklch(0.666747 0.004239 187.292)"),
    ("iconMuted", "oklch(0.666747 0.004239 187.292)"),
    ("error", "oklch(0.655108 0.221148 23.473)"),
    ("errorForeground", "oklch(0.704237 0.187511 22.228)"),
    ("errorSurface", "oklch(0.312773 0.02923 32.121)"),
    ("warning", "oklch(0.772406 0.172798 65.367)"),
    ("warningForeground", "oklch(0.829017 0.171221 81.038)"),
    ("warningSurface", "oklch(0.345524 0.046882 99.736)"),
    ("update", "oklch(0.796228 0.133058 157.319)"),
    ("updateForeground", "oklch(0.86276 0.089288 159.704)"),
    ("updateSurface", "oklch(0.448116 0.062637 158.86)"),
    ("accentSurface", "oklch(0.437021 0.060312 158.962)"),
    ("accentSurfaceForeground", "oklch(0.990339 0.008411 325.64)"),
    ("messageSurface", "oklch(0.470111 0.067221 158.676)"),
    ("messageForeground", "oklch(0.990339 0.008411 325.64)"),
    ("messageAction", "oklch(0.791603 0.129713 83.299)"),
    ("messageActionForeground", "oklch(0.222003 0.03479 328.979)"),
    ("messageActionHover", "oklch(0.815227 0.117902 84.21)"),
    ("codeBackground", "oklch(0.312979 0.018942 164.082)"),
    ("codeForeground", "oklch(0.990339 0.008411 325.64)"),
    ("sidebar", "oklch(0.309925 0.032827 160.944)"),
    ("sidebarForeground", "oklch(0.990339 0.008411 325.64)"),
    ("sidebarMutedForeground", "oklch(0.711387 0.007643 175.89)"),
    ("sidebarControlSurface", "oklch(0.432727 0.024549 163.654)"),
    ("sidebarRowHover", "oklch(0.374959 0.047124 159.686)"),
    ("sidebarRowActive", "oklch(0.41688 0.056069 159.165)"),
    ("sidebarRowSelected", "oklch(0.437466 0.060406 158.958)"),
    ("sidebarBorder", "oklch(0.569253 0.015933 167.062)"),
    ("terminalBackground", "oklch(0.260865 0.02152 162.75)"),
    ("terminalForeground", "oklch(0.990339 0.008411 325.64)"),
    ("terminalCursor", "oklch(0.796228 0.133058 157.319)"),
    ("terminalSelection", "oklch(0.464636 0.066083 158.72)"),
    ("terminalScrollbar", "oklch(0.594692 0.006862 176.022)"),
    ("terminalScrollbarHover", "oklch(0.687968 0.00354 193.55)"),
];

const OCEAN_LIGHT_ROLES: &[(&str, &str)] = &[
    ("canvas", "oklch(0.974199 0.002856 241.597)"),
    ("chrome", "oklch(0.974199 0.002856 241.597)"),
    ("toolbar", "oklch(0.974199 0.002856 241.597)"),
    ("toolbarForeground", "oklch(0.222003 0.03479 328.979)"),
    ("toolbarBorder", "oklch(0.91295 0.018827 241.836)"),
    ("toolbarControl", "oklch(0.939254 0.01193 241.729)"),
    (
        "toolbarControlForeground",
        "oklch(0.222003 0.03479 328.979)",
    ),
    ("toolbarControlHover", "oklch(0.91295 0.018827 241.836)"),
    ("surface", "oklch(0.974199 0.002856 241.597)"),
    ("surfaceRaised", "oklch(0.951058 0.002962 258.339)"),
    ("surfaceOverlay", "oklch(0.934442 0.003181 269.1)"),
    ("text", "oklch(0.222003 0.03479 328.979)"),
    ("textMuted", "oklch(0.541555 0.017468 323.531)"),
    ("border", "oklch(0.867646 0.013482 252.362)"),
    ("input", "oklch(0.832939 0.017389 252.598)"),
    ("focus", "oklch(0.536684 0.120219 247.01)"),
    ("accent", "oklch(0.536684 0.120219 247.01)"),
    ("accentForeground", "oklch(0.990339 0.008411 325.64)"),
    ("secondary", "oklch(0.939254 0.01193 241.729)"),
    ("secondaryForeground", "oklch(0.222003 0.03479 328.979)"),
    ("muted", "oklch(0.948004 0.009649 241.695)"),
    ("mutedForeground", "oklch(0.528741 0.01828 313.823)"),
    ("placeholder", "oklch(0.530733 0.01795 323.79)"),
    ("secondaryLabel", "oklch(0.541555 0.017468 323.531)"),
    ("iconMuted", "oklch(0.541555 0.017468 323.531)"),
    ("error", "oklch(0.637823 0.237287 25.436)"),
    ("errorForeground", "oklch(0.509494 0.208583 28.513)"),
    ("errorSurface", "oklch(0.938747 0.016377 7.186)"),
    ("warning", "oklch(0.772406 0.172798 65.367)"),
    ("warningForeground", "oklch(0.546927 0.155556 45.359)"),
    ("warningSurface", "oklch(0.954846 0.016009 81.731)"),
    ("update", "oklch(0.536684 0.120219 247.01)"),
    ("updateForeground", "oklch(0.397497 0.084999 246.523)"),
    ("updateSurface", "oklch(0.904165 0.021144 241.874)"),
    ("accentSurface", "oklch(0.91295 0.018827 241.836)"),
    ("accentSurfaceForeground", "oklch(0.222003 0.03479 328.979)"),
    ("messageSurface", "oklch(0.895373 0.023469 241.913)"),
    ("messageForeground", "oklch(0.222003 0.03479 328.979)"),
    ("messageAction", "oklch(0.493961 0.08175 201.584)"),
    ("messageActionForeground", "oklch(0.990339 0.008411 325.64)"),
    ("messageActionHover", "oklch(0.45151 0.074407 201.516)"),
    ("codeBackground", "oklch(0.957684 0.002906 253.68)"),
    ("codeForeground", "oklch(0.222003 0.03479 328.979)"),
    ("sidebar", "oklch(0.939254 0.01193 241.729)"),
    ("sidebarForeground", "oklch(0.222003 0.03479 328.979)"),
    ("sidebarMutedForeground", "oklch(0.517366 0.018944 311.433)"),
    ("sidebarControlSurface", "oklch(0.888479 0.011475 251.638)"),
    ("sidebarRowHover", "oklch(0.890798 0.024681 241.933)"),
    ("sidebarRowActive", "oklch(0.858363 0.033325 242.089)"),
    ("sidebarRowSelected", "oklch(0.842113 0.037689 242.174)"),
    ("sidebarBorder", "oklch(0.862823 0.011384 256.926)"),
    ("terminalBackground", "oklch(0.974199 0.002856 241.597)"),
    ("terminalForeground", "oklch(0.222003 0.03479 328.979)"),
    ("terminalCursor", "oklch(0.536684 0.120219 247.01)"),
    ("terminalSelection", "oklch(0.895373 0.023469 241.913)"),
    ("terminalScrollbar", "oklch(0.826271 0.006191 305.456)"),
    ("terminalScrollbarHover", "oklch(0.756866 0.008685 313.721)"),
];

const OCEAN_DARK_ROLES: &[(&str, &str)] = &[
    ("canvas", "oklch(0.242641 0.024125 250.573)"),
    ("chrome", "oklch(0.242641 0.024125 250.573)"),
    ("toolbar", "oklch(0.242641 0.024125 250.573)"),
    ("toolbarForeground", "oklch(0.990339 0.008411 325.64)"),
    ("toolbarBorder", "oklch(0.439946 0.0561 243.479)"),
    ("toolbarControl", "oklch(0.358725 0.043145 244.911)"),
    (
        "toolbarControlForeground",
        "oklch(0.990339 0.008411 325.64)",
    ),
    ("toolbarControlHover", "oklch(0.413315 0.051874 243.855)"),
    ("surface", "oklch(0.242641 0.024125 250.573)"),
    ("surfaceRaised", "oklch(0.348439 0.019942 253.696)"),
    ("surfaceOverlay", "oklch(0.398517 0.018232 255.72)"),
    ("text", "oklch(0.990339 0.008411 325.64)"),
    ("textMuted", "oklch(0.652227 0.01149 273.31)"),
    ("border", "oklch(0.438653 0.039496 245.44)"),
    ("input", "oklch(0.500905 0.043574 244.781)"),
    ("focus", "oklch(0.758933 0.105833 241.548)"),
    ("accent", "oklch(0.758933 0.105833 241.548)"),
    ("accentForeground", "oklch(0.222003 0.03479 328.979)"),
    ("secondary", "oklch(0.358725 0.043145 244.911)"),
    ("secondaryForeground", "oklch(0.990339 0.008411 325.64)"),
    ("muted", "oklch(0.319287 0.036766 246.065)"),
    ("mutedForeground", "oklch(0.691936 0.016294 261.588)"),
    ("placeholder", "oklch(0.721641 0.010192 281.271)"),
    ("secondaryLabel", "oklch(0.652227 0.01149 273.31)"),
    ("iconMuted", "oklch(0.652227 0.01149 273.31)"),
    ("error", "oklch(0.655108 0.221148 23.473)"),
    ("errorForeground", "oklch(0.702184 0.189226 22.228)"),
    ("errorSurface", "oklch(0.298933 0.036443 350.094)"),
    ("warning", "oklch(0.772406 0.172798 65.367)"),
    ("warningForeground", "oklch(0.829017 0.171221 81.038)"),
    ("warningSurface", "oklch(0.329449 0.028712 84.495)"),
    ("update", "oklch(0.758933 0.105833 241.548)"),
    ("updateForeground", "oklch(0.840844 0.069217 240.151)"),
    ("updateSurface", "oklch(0.424017 0.053575 243.695)"),
    ("accentSurface", "oklch(0.413315 0.051874 243.855)"),
    ("accentSurfaceForeground", "oklch(0.990339 0.008411 325.64)"),
    ("messageSurface", "oklch(0.445224 0.056936 243.413)"),
    ("messageForeground", "oklch(0.990339 0.008411 325.64)"),
    ("messageAction", "oklch(0.793363 0.105022 199.893)"),
    ("messageActionForeground", "oklch(0.222003 0.03479 328.979)"),
    ("messageActionHover", "oklch(0.815308 0.096174 199.862)"),
    ("codeBackground", "oklch(0.29661 0.021883 251.968)"),
    ("codeForeground", "oklch(0.990339 0.008411 325.64)"),
    ("sidebar", "oklch(0.290387 0.032043 247.274)"),
    ("sidebarForeground", "oklch(0.990339 0.008411 325.64)"),
    ("sidebarMutedForeground", "oklch(0.69099 0.01395 266.424)"),
    ("sidebarControlSurface", "oklch(0.417822 0.02535 250.162)"),
    ("sidebarRowHover", "oklch(0.353381 0.042285 245.043)"),
    ("sidebarRowActive", "oklch(0.393878 0.048778 244.179)"),
    ("sidebarRowSelected", "oklch(0.413744 0.051943 243.848)"),
    ("sidebarBorder", "oklch(0.55859 0.019001 256.223)"),
    ("terminalBackground", "oklch(0.242641 0.024125 250.573)"),
    ("terminalForeground", "oklch(0.990339 0.008411 325.64)"),
    ("terminalCursor", "oklch(0.758933 0.105833 241.548)"),
    ("terminalSelection", "oklch(0.439946 0.0561 243.479)"),
    ("terminalScrollbar", "oklch(0.58613 0.012959 267.22)"),
    ("terminalScrollbarHover", "oklch(0.681569 0.010909 276.465)"),
];

const EMBER_LIGHT_ROLES: &[(&str, &str)] = &[
    ("canvas", "oklch(0.976527 0.002685 60.725)"),
    ("chrome", "oklch(0.976527 0.002685 60.725)"),
    ("toolbar", "oklch(0.976527 0.002685 60.725)"),
    ("toolbarForeground", "oklch(0.222003 0.03479 328.979)"),
    ("toolbarBorder", "oklch(0.916502 0.01832 49.597)"),
    ("toolbarControl", "oklch(0.942267 0.01151 50.785)"),
    (
        "toolbarControlForeground",
        "oklch(0.222003 0.03479 328.979)",
    ),
    ("toolbarControlHover", "oklch(0.916502 0.01832 49.597)"),
    ("surface", "oklch(0.976527 0.002685 60.725)"),
    ("surfaceRaised", "oklch(0.953321 0.002701 42.266)"),
    ("surfaceOverlay", "oklch(0.936659 0.002879 29.96)"),
    ("text", "oklch(0.222003 0.03479 328.979)"),
    ("textMuted", "oklch(0.543023 0.017316 331.964)"),
    ("border", "oklch(0.870631 0.013204 39.431)"),
    ("input", "oklch(0.836213 0.017153 38.661)"),
    ("focus", "oklch(0.552831 0.129438 44.656)"),
    ("accent", "oklch(0.552831 0.129438 44.656)"),
    ("accentForeground", "oklch(0.990339 0.008411 325.64)"),
    ("secondary", "oklch(0.942267 0.01151 50.785)"),
    ("secondaryForeground", "oklch(0.222003 0.03479 328.979)"),
    ("muted", "oklch(0.950842 0.009273 51.528)"),
    ("mutedForeground", "oklch(0.530413 0.018453 341.181)"),
    ("placeholder", "oklch(0.532339 0.017796 331.748)"),
    ("secondaryLabel", "oklch(0.543023 0.017316 331.964)"),
    ("iconMuted", "oklch(0.543023 0.017316 331.964)"),
    ("error", "oklch(0.637823 0.237287 25.436)"),
    ("errorForeground", "oklch(0.509494 0.208583 28.513)"),
    ("errorSurface", "oklch(0.941094 0.019938 19.375)"),
    ("warning", "oklch(0.772406 0.172798 65.367)"),
    ("warningForeground", "oklch(0.549154 0.156188 45.359)"),
    ("warningSurface", "oklch(0.957148 0.020843 76.702)"),
    ("update", "oklch(0.552831 0.129438 44.656)"),
    ("updateForeground", "oklch(0.408647 0.091207 45.037)"),
    ("updateSurface", "oklch(0.907902 0.020621 49.36)"),
    ("accentSurface", "oklch(0.916502 0.01832 49.597)"),
    ("accentSurfaceForeground", "oklch(0.222003 0.03479 328.979)"),
    ("messageSurface", "oklch(0.899296 0.022939 49.163)"),
    ("messageForeground", "oklch(0.222003 0.03479 328.979)"),
    ("messageAction", "oklch(0.516323 0.161628 24.82)"),
    ("messageActionForeground", "oklch(0.990339 0.008411 325.64)"),
    ("messageActionHover", "oklch(0.471223 0.145843 24.688)"),
    ("codeBackground", "oklch(0.959965 0.002668 47.512)"),
    ("codeForeground", "oklch(0.222003 0.03479 328.979)"),
    ("sidebar", "oklch(0.942267 0.01151 50.785)"),
    ("sidebarForeground", "oklch(0.222003 0.03479 328.979)"),
    ("sidebarMutedForeground", "oklch(0.519146 0.019214 343.427)"),
    ("sidebarControlSurface", "oklch(0.891332 0.011179 40.596)"),
    ("sidebarRowHover", "oklch(0.894819 0.024151 49.073)"),
    ("sidebarRowActive", "oklch(0.863104 0.032855 48.586)"),
    ("sidebarRowSelected", "oklch(0.84723 0.037292 48.403)"),
    ("sidebarBorder", "oklch(0.865593 0.011154 35.246)"),
    ("terminalBackground", "oklch(0.976527 0.002685 60.725)"),
    ("terminalForeground", "oklch(0.222003 0.03479 328.979)"),
    ("terminalCursor", "oklch(0.552831 0.129438 44.656)"),
    ("terminalSelection", "oklch(0.899296 0.022939 49.163)"),
    ("terminalScrollbar", "oklch(0.828185 0.005884 349.533)"),
    ("terminalScrollbarHover", "oklch(0.758584 0.008423 341.16)"),
];

const EMBER_DARK_ROLES: &[(&str, &str)] = &[
    ("canvas", "oklch(0.245899 0.019144 42.044)"),
    ("chrome", "oklch(0.245899 0.019144 42.044)"),
    ("toolbar", "oklch(0.245899 0.019144 42.044)"),
    ("toolbarForeground", "oklch(0.990339 0.008411 325.64)"),
    ("toolbarBorder", "oklch(0.442681 0.0608 50.795)"),
    ("toolbarControl", "oklch(0.361499 0.044052 49.515)"),
    (
        "toolbarControlForeground",
        "oklch(0.990339 0.008411 325.64)",
    ),
    ("toolbarControlHover", "oklch(0.416048 0.055354 50.484)"),
    ("surface", "oklch(0.245899 0.019144 42.044)"),
    ("surfaceRaised", "oklch(0.351262 0.01565 37.592)"),
    ("surfaceOverlay", "oklch(0.401111 0.014308 34.896)"),
    ("text", "oklch(0.990339 0.008411 325.64)"),
    ("textMuted", "oklch(0.654017 0.009505 13.287)"),
    ("border", "oklch(0.44099 0.040202 48.807)"),
    ("input", "oklch(0.503003 0.045721 49.44)"),
    ("focus", "oklch(0.762174 0.124117 52.082)"),
    ("accent", "oklch(0.762174 0.124117 52.082)"),
    ("accentForeground", "oklch(0.222003 0.03479 328.979)"),
    ("secondary", "oklch(0.361499 0.044052 49.515)"),
    ("secondaryForeground", "oklch(0.990339 0.008411 325.64)"),
    ("muted", "oklch(0.322144 0.03574 48.309)"),
    ("mutedForeground", "oklch(0.692479 0.015227 30.963)"),
    ("placeholder", "oklch(0.723533 0.008741 4.515)"),
    ("secondaryLabel", "oklch(0.654017 0.009505 13.287)"),
    ("iconMuted", "oklch(0.654017 0.009505 13.287)"),
    ("error", "oklch(0.655108 0.221148 23.473)"),
    ("errorForeground", "oklch(0.702184 0.189226 22.228)"),
    ("errorSurface", "oklch(0.310955 0.059624 24.334)"),
    ("warning", "oklch(0.772406 0.172798 65.367)"),
    ("warningForeground", "oklch(0.829017 0.171221 81.038)"),
    ("warningSurface", "oklch(0.339137 0.055638 66.911)"),
    ("update", "oklch(0.762174 0.124117 52.082)"),
    ("updateForeground", "oklch(0.841456 0.079585 53.521)"),
    ("updateSurface", "oklch(0.426749 0.057547 50.618)"),
    ("accentSurface", "oklch(0.416048 0.055354 50.484)"),
    ("accentSurfaceForeground", "oklch(0.990339 0.008411 325.64)"),
    ("messageSurface", "oklch(0.447961 0.061874 50.849)"),
    ("messageForeground", "oklch(0.990339 0.008411 325.64)"),
    ("messageAction", "oklch(0.747955 0.135578 29.432)"),
    ("messageActionForeground", "oklch(0.222003 0.03479 328.979)"),
    ("messageActionHover", "oklch(0.775116 0.117953 29.014)"),
    ("codeBackground", "oklch(0.299662 0.017229 39.973)"),
    ("codeForeground", "oklch(0.990339 0.008411 325.64)"),
    ("sidebar", "oklch(0.293349 0.029554 46.882)"),
    ("sidebarForeground", "oklch(0.990339 0.008411 325.64)"),
    ("sidebarMutedForeground", "oklch(0.691874 0.012538 24.638)"),
    ("sidebarControlSurface", "oklch(0.420227 0.022893 43.226)"),
    ("sidebarRowHover", "oklch(0.356163 0.042933 49.385)"),
    ("sidebarRowActive", "oklch(0.396617 0.051353 50.201)"),
    ("sidebarRowSelected", "oklch(0.416477 0.055442 50.489)"),
    ("sidebarBorder", "oklch(0.560372 0.016998 36.179)"),
    ("terminalBackground", "oklch(0.245899 0.019144 42.044)"),
    ("terminalForeground", "oklch(0.990339 0.008411 325.64)"),
    ("terminalCursor", "oklch(0.762174 0.124117 52.082)"),
    ("terminalSelection", "oklch(0.442681 0.0608 50.795)"),
    ("terminalScrollbar", "oklch(0.587861 0.010463 20.444)"),
    ("terminalScrollbarHover", "oklch(0.682876 0.009156 9.796)"),
];

const IRIS_LIGHT_ROLES: &[(&str, &str)] = &[
    ("canvas", "oklch(0.976531 0.003855 303.226)"),
    ("chrome", "oklch(0.976531 0.003855 303.226)"),
    ("toolbar", "oklch(0.976531 0.003855 303.226)"),
    ("toolbarForeground", "oklch(0.222003 0.03479 328.979)"),
    ("toolbarBorder", "oklch(0.914882 0.022965 299.986)"),
    ("toolbarControl", "oklch(0.941387 0.014687 300.474)"),
    (
        "toolbarControlForeground",
        "oklch(0.222003 0.03479 328.979)",
    ),
    ("toolbarControlHover", "oklch(0.914882 0.022965 299.986)"),
    ("surface", "oklch(0.976531 0.003855 303.226)"),
    ("surfaceRaised", "oklch(0.953326 0.004536 307.676)"),
    ("surfaceOverlay", "oklch(0.936665 0.005041 310.132)"),
    ("text", "oklch(0.222003 0.03479 328.979)"),
    ("textMuted", "oklch(0.543042 0.018894 325.652)"),
    ("border", "oklch(0.869608 0.018226 303.859)"),
    ("input", "oklch(0.834773 0.023405 303.676)"),
    ("focus", "oklch(0.525348 0.15373 294.176)"),
    ("accent", "oklch(0.525348 0.15373 294.176)"),
    ("accentForeground", "oklch(0.990339 0.008411 325.64)"),
    ("secondary", "oklch(0.941387 0.014687 300.474)"),
    ("secondaryForeground", "oklch(0.222003 0.03479 328.979)"),
    ("muted", "oklch(0.950194 0.011956 300.733)"),
    ("mutedForeground", "oklch(0.529955 0.022319 321.556)"),
    ("placeholder", "oklch(0.532177 0.019333 325.784)"),
    ("secondaryLabel", "oklch(0.543042 0.018894 325.652)"),
    ("iconMuted", "oklch(0.543042 0.018894 325.652)"),
    ("error", "oklch(0.637823 0.237287 25.436)"),
    ("errorForeground", "oklch(0.509494 0.208583 28.513)"),
    ("errorSurface", "oklch(0.941043 0.019582 4.235)"),
    ("warning", "oklch(0.772406 0.172798 65.367)"),
    ("warningForeground", "oklch(0.549154 0.156188 45.359)"),
    ("warningSurface", "oklch(0.957054 0.016197 69.932)"),
    ("update", "oklch(0.525348 0.15373 294.176)"),
    ("updateForeground", "oklch(0.389926 0.10825 294.547)"),
    ("updateSurface", "oklch(0.90602 0.025754 299.867)"),
    ("accentSurface", "oklch(0.914882 0.022965 299.986)"),
    ("accentSurfaceForeground", "oklch(0.222003 0.03479 328.979)"),
    ("messageSurface", "oklch(0.897143 0.028558 299.758)"),
    ("messageForeground", "oklch(0.222003 0.03479 328.979)"),
    ("messageAction", "oklch(0.516084 0.185229 340.776)"),
    ("messageActionForeground", "oklch(0.990339 0.008411 325.64)"),
    ("messageActionHover", "oklch(0.471003 0.16748 340.687)"),
    ("codeBackground", "oklch(0.95997 0.004338 306.542)"),
    ("codeForeground", "oklch(0.222003 0.03479 328.979)"),
    ("sidebar", "oklch(0.941387 0.014687 300.474)"),
    ("sidebarForeground", "oklch(0.222003 0.03479 328.979)"),
    ("sidebarMutedForeground", "oklch(0.518417 0.023683 320.681)"),
    ("sidebarControlSurface", "oklch(0.890512 0.0155 303.803)"),
    ("sidebarRowHover", "oklch(0.892522 0.030022 299.704)"),
    ("sidebarRowActive", "oklch(0.85971 0.040501 299.36)"),
    ("sidebarRowSelected", "oklch(0.843236 0.045818 299.198)"),
    ("sidebarBorder", "oklch(0.864805 0.015938 305.371)"),
    ("terminalBackground", "oklch(0.976531 0.003855 303.226)"),
    ("terminalForeground", "oklch(0.222003 0.03479 328.979)"),
    ("terminalCursor", "oklch(0.525348 0.15373 294.176)"),
    ("terminalSelection", "oklch(0.897143 0.028558 299.758)"),
    ("terminalScrollbar", "oklch(0.828195 0.008526 318.858)"),
    ("terminalScrollbarHover", "oklch(0.758596 0.010892 321.538)"),
];

const IRIS_DARK_ROLES: &[(&str, &str)] = &[
    ("canvas", "oklch(0.225975 0.031062 293.741)"),
    ("chrome", "oklch(0.225975 0.031062 293.741)"),
    ("toolbar", "oklch(0.225975 0.031062 293.741)"),
    ("toolbarForeground", "oklch(0.990339 0.008411 325.64)"),
    ("toolbarBorder", "oklch(0.395417 0.085554 294.182)"),
    ("toolbarControl", "oklch(0.325405 0.063614 294.23)"),
    (
        "toolbarControlForeground",
        "oklch(0.990339 0.008411 325.64)",
    ),
    ("toolbarControlHover", "oklch(0.372436 0.07841 294.204)"),
    ("surface", "oklch(0.225975 0.031062 293.741)"),
    ("surfaceRaised", "oklch(0.335291 0.026008 296.394)"),
    ("surfaceOverlay", "oklch(0.386739 0.024023 297.509)"),
    ("text", "oklch(0.990339 0.008411 325.64)"),
    ("textMuted", "oklch(0.640465 0.016197 304.171)"),
    ("border", "oklch(0.40874 0.058536 295.893)"),
    ("input", "oklch(0.46756 0.065775 296.265)"),
    ("focus", "oklch(0.671712 0.169136 293.929)"),
    ("accent", "oklch(0.671712 0.169136 293.929)"),
    ("accentForeground", "oklch(0.222003 0.03479 328.979)"),
    ("secondary", "oklch(0.325405 0.063614 294.23)"),
    ("secondaryForeground", "oklch(0.990339 0.008411 325.64)"),
    ("muted", "oklch(0.291515 0.05276 294.209)"),
    ("mutedForeground", "oklch(0.663321 0.025932 301.862)"),
    ("placeholder", "oklch(0.706249 0.014508 306.607)"),
    ("secondaryLabel", "oklch(0.640465 0.016197 304.171)"),
    ("iconMuted", "oklch(0.640465 0.016197 304.171)"),
    ("error", "oklch(0.655108 0.221148 23.473)"),
    ("errorForeground", "oklch(0.702184 0.189226 22.228)"),
    ("errorSurface", "oklch(0.291658 0.054707 352.238)"),
    ("warning", "oklch(0.772406 0.172798 65.367)"),
    ("warningForeground", "oklch(0.829017 0.171221 81.038)"),
    ("warningSurface", "oklch(0.318952 0.033845 51.646)"),
    ("update", "oklch(0.671712 0.169136 293.929)"),
    ("updateForeground", "oklch(0.785032 0.108439 296.344)"),
    ("updateSurface", "oklch(0.381668 0.081286 294.195)"),
    ("accentSurface", "oklch(0.372436 0.07841 294.204)"),
    ("accentSurfaceForeground", "oklch(0.990339 0.008411 325.64)"),
    ("messageSurface", "oklch(0.399975 0.086965 294.177)"),
    ("messageForeground", "oklch(0.990339 0.008411 325.64)"),
    ("messageAction", "oklch(0.789904 0.130063 337.621)"),
    ("messageActionForeground", "oklch(0.222003 0.03479 328.979)"),
    ("messageActionHover", "oklch(0.813537 0.114101 337.23)"),
    ("codeBackground", "oklch(0.281873 0.028308 295.193)"),
    ("codeForeground", "oklch(0.990339 0.008411 325.64)"),
    ("sidebar", "oklch(0.266743 0.044689 294.138)"),
    ("sidebarForeground", "oklch(0.990339 0.008411 325.64)"),
    ("sidebarMutedForeground", "oklch(0.668773 0.021522 302.949)"),
    ("sidebarControlSurface", "oklch(0.399977 0.035678 297.031)"),
    ("sidebarRowHover", "oklch(0.320808 0.062152 294.23)"),
    ("sidebarRowActive", "oklch(0.355677 0.073167 294.217)"),
    ("sidebarRowSelected", "oklch(0.372806 0.078525 294.203)"),
    ("sidebarBorder", "oklch(0.545895 0.027522 299.871)"),
    ("terminalBackground", "oklch(0.225975 0.031062 293.741)"),
    ("terminalForeground", "oklch(0.990339 0.008411 325.64)"),
    ("terminalCursor", "oklch(0.671712 0.169136 293.929)"),
    ("terminalSelection", "oklch(0.395417 0.085554 294.182)"),
    ("terminalScrollbar", "oklch(0.578663 0.017888 302.229)"),
    ("terminalScrollbarHover", "oklch(0.676012 0.015271 305.433)"),
];

/// Light companion for Classic so the built-in library stays paired.
const DEVMANAGER_CLASSIC_LIGHT_ROLES: &[(&str, &str)] = &[
    ("canvas", "#fafafa"),
    ("chrome", "#f4f4f5"),
    ("toolbar", "#f4f4f5"),
    ("toolbarForeground", "#18181b"),
    ("toolbarBorder", "#e4e4e7"),
    ("toolbarControl", "#e4e4e7"),
    ("toolbarControlForeground", "#18181b"),
    ("toolbarControlHover", "#d4d4d8"),
    ("surface", "#ffffff"),
    ("surfaceRaised", "#ffffff"),
    ("surfaceOverlay", "#f4f4f5"),
    ("text", "#18181b"),
    ("textMuted", "#71717a"),
    ("border", "#e4e4e7"),
    ("input", "#e4e4e7"),
    ("focus", "#4f46e5"),
    ("accent", "#4f46e5"),
    ("accentForeground", "#f8fafc"),
    ("secondary", "#f4f4f5"),
    ("secondaryForeground", "#18181b"),
    ("muted", "#f4f4f5"),
    ("mutedForeground", "#71717a"),
    ("placeholder", "#a1a1aa"),
    ("secondaryLabel", "#71717a"),
    ("iconMuted", "#71717a"),
    ("error", "#e11d48"),
    ("errorForeground", "#e11d48"),
    ("errorSurface", "#fff1f2"),
    ("warning", "#ca8a04"),
    ("warningForeground", "#a16207"),
    ("warningSurface", "#fefce8"),
    ("update", "#4f46e5"),
    ("updateForeground", "#3730a3"),
    ("updateSurface", "#e0e7ff"),
    ("accentSurface", "#e0e7ff"),
    ("accentSurfaceForeground", "#18181b"),
    ("messageSurface", "#e0e7ff"),
    ("messageForeground", "#18181b"),
    ("messageAction", "#4f46e5"),
    ("messageActionForeground", "#f8fafc"),
    ("messageActionHover", "#4338ca"),
    ("codeBackground", "#f4f4f5"),
    ("codeForeground", "#18181b"),
    ("sidebar", "#f4f4f5"),
    ("sidebarForeground", "#18181b"),
    ("sidebarMutedForeground", "#71717a"),
    ("sidebarControlSurface", "#e4e4e7"),
    ("sidebarRowHover", "#e4e4e7"),
    ("sidebarRowActive", "#e4e4e7"),
    ("sidebarRowSelected", "#c7d2fe"),
    ("sidebarBorder", "#e4e4e7"),
    ("terminalBackground", "#fafafa"),
    ("terminalForeground", "#18181b"),
    ("terminalCursor", "#4f46e5"),
    ("terminalSelection", "#c7d2fe"),
    ("terminalScrollbar", "#d4d4d8"),
    ("terminalScrollbarHover", "#a1a1aa"),
];

fn built_in_themes() -> Vec<ThemeDefinition> {
    vec![
        // The dark half is the redesign shell itself, projected straight from
        // the token module, because this is the theme a fresh profile resolves
        // to and its roles are what the running app actually renders.
        ThemeDefinition::paired_palettes(
            "devmanager-classic",
            "DevManager Classic",
            palette_from_oklch_roles(DEVMANAGER_CLASSIC_LIGHT_ROLES),
            ThemePalette::redesign_dark(),
        ),
        ThemeDefinition::paired_managed(
            "t3-code", "T3 Code", "#fbfafc", "#d60057", "#18151d", "#e0005b",
        ),
        ThemeDefinition::paired_semantic(
            "t3-chat",
            "T3 Chat",
            T3_CHAT_LIGHT_ROLES,
            T3_CHAT_DARK_ROLES,
        ),
        ThemeDefinition::paired_semantic("grove", "Grove", GROVE_LIGHT_ROLES, GROVE_DARK_ROLES),
        ThemeDefinition::paired_semantic("ocean", "Ocean", OCEAN_LIGHT_ROLES, OCEAN_DARK_ROLES),
        ThemeDefinition::paired_semantic("ember", "Ember", EMBER_LIGHT_ROLES, EMBER_DARK_ROLES),
        ThemeDefinition::paired_semantic("iris", "Iris", IRIS_LIGHT_ROLES, IRIS_DARK_ROLES),
    ]
}

fn is_built_in_theme_id(id: &str) -> bool {
    matches!(
        id,
        "devmanager-classic" | "t3-code" | "t3-chat" | "grove" | "ocean" | "ember" | "iris"
    )
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ThemeLibraryError {
    TooManyThemes,
    DuplicateId(String),
    InvalidTheme(ThemeFileError),
}

impl fmt::Display for ThemeLibraryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooManyThemes => formatter.write_str("custom theme library exceeds its bound"),
            Self::DuplicateId(_) => formatter.write_str("theme id is already installed"),
            Self::InvalidTheme(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for ThemeLibraryError {}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ThemeFileError {
    TooLarge,
    InvalidJson,
    InvalidVersion,
    InvalidIdentity,
    InvalidAppearance,
    InvalidColor(ThemeColorRole),
    MissingRole(ThemeColorRole),
    UnknownRole(String),
    MissingVariant,
}

impl fmt::Display for ThemeFileError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooLarge => formatter.write_str("theme file exceeds 256 KiB"),
            Self::InvalidJson => formatter.write_str("theme file is not a JSON object"),
            Self::InvalidVersion => formatter.write_str("theme file version is unsupported"),
            Self::InvalidIdentity => formatter.write_str("theme id or name is invalid"),
            Self::InvalidAppearance => formatter.write_str("theme appearance is invalid"),
            Self::InvalidColor(role) => write!(formatter, "theme color {role} is invalid"),
            Self::MissingRole(role) => write!(formatter, "theme color {role} is missing"),
            Self::UnknownRole(role) => write!(formatter, "theme color {role} is unknown"),
            Self::MissingVariant => formatter.write_str("theme variant is invalid"),
        }
    }
}

impl std::error::Error for ThemeFileError {}

pub fn parse_theme_file(source: &str) -> Result<ThemeDefinition, ThemeFileError> {
    if source.len() as u64 > MAX_THEME_FILE_BYTES {
        return Err(ThemeFileError::TooLarge);
    }
    let mut object = serde_json::from_str::<Value>(source)
        .map_err(|_| ThemeFileError::InvalidJson)?
        .as_object()
        .cloned()
        .ok_or(ThemeFileError::InvalidJson)?;
    let version = object
        .remove("version")
        .and_then(|value| value.as_u64())
        .ok_or(ThemeFileError::InvalidVersion)?;
    if version != u64::from(THEME_FILE_VERSION) {
        return Err(ThemeFileError::InvalidVersion);
    }
    let id = take_string(&mut object, "id")?;
    let label = object
        .remove("name")
        .or_else(|| object.remove("label"))
        .and_then(|value| value.as_str().map(str::to_string))
        .ok_or(ThemeFileError::InvalidIdentity)?;
    validate_identity(&id, &label)?;
    let appearance = take_appearance(&mut object, "appearance")?;
    let managed = object
        .remove("managed")
        .and_then(|value| value.as_bool())
        .unwrap_or(false);
    let base = object
        .remove("colors")
        .and_then(|value| value.as_object().cloned())
        .ok_or(ThemeFileError::InvalidJson)?;
    let mut palettes = BTreeMap::from([(appearance, parse_palette(base, appearance, managed)?)]);
    if let Some(variants) = object.remove("variants") {
        let variants = variants
            .as_object()
            .cloned()
            .ok_or(ThemeFileError::MissingVariant)?;
        for (raw_appearance, value) in variants {
            let variant_appearance = parse_appearance(&raw_appearance)?;
            let colors = value
                .as_object()
                .cloned()
                .ok_or(ThemeFileError::MissingVariant)?;
            palettes.insert(
                variant_appearance,
                parse_palette(colors, variant_appearance, managed)?,
            );
        }
    }
    Ok(ThemeDefinition {
        id,
        label,
        palettes,
        managed,
        metadata: object,
    })
}

pub fn serialize_theme_file(theme: &ThemeDefinition) -> Result<String, ThemeFileError> {
    let (&appearance, palette) = theme
        .palettes
        .iter()
        .next()
        .ok_or(ThemeFileError::MissingVariant)?;
    let mut object = theme.metadata.clone();
    object.insert("version".to_string(), Value::from(THEME_FILE_VERSION));
    object.insert("id".to_string(), Value::String(theme.id.clone()));
    object.insert("name".to_string(), Value::String(theme.label.clone()));
    object.insert(
        "appearance".to_string(),
        Value::String(appearance_name(appearance).to_string()),
    );
    object.insert("managed".to_string(), Value::Bool(theme.managed));
    object.insert(
        "colors".to_string(),
        Value::Object(palette_to_json(palette)),
    );
    let variants = theme
        .palettes
        .iter()
        .filter(|(candidate, _)| **candidate != appearance)
        .map(|(candidate, palette)| {
            (
                appearance_name(*candidate).to_string(),
                Value::Object(palette_to_json(palette)),
            )
        })
        .collect::<JsonMap<String, Value>>();
    if !variants.is_empty() {
        object.insert("variants".to_string(), Value::Object(variants));
    }
    serde_json::to_string_pretty(&Value::Object(object)).map_err(|_| ThemeFileError::InvalidJson)
}

fn parse_palette(
    values: JsonMap<String, Value>,
    appearance: ThemeAppearance,
    managed: bool,
) -> Result<ThemePalette, ThemeFileError> {
    let mut parsed = BTreeMap::new();
    for (name, value) in values {
        let role = ThemeColorRole::parse(&name).ok_or(ThemeFileError::UnknownRole(name))?;
        let value = value.as_str().ok_or(ThemeFileError::InvalidColor(role))?;
        let color = ThemeColor::parse(value).map_err(|_| ThemeFileError::InvalidColor(role))?;
        parsed.insert(role, color);
    }
    if managed {
        let canvas = parsed
            .get(&ThemeColorRole::Canvas)
            .copied()
            .ok_or(ThemeFileError::MissingRole(ThemeColorRole::Canvas))?;
        let accent = parsed
            .get(&ThemeColorRole::Accent)
            .copied()
            .ok_or(ThemeFileError::MissingRole(ThemeColorRole::Accent))?;
        return Ok(ThemePalette::managed(appearance, canvas, accent));
    }
    for role in ThemeColorRole::ALL {
        if !parsed.contains_key(role) {
            return Err(ThemeFileError::MissingRole(*role));
        }
    }
    Ok(ThemePalette { colors: parsed })
}

fn palette_to_json(palette: &ThemePalette) -> JsonMap<String, Value> {
    ThemeColorRole::ALL
        .iter()
        .map(|role| {
            (
                role.as_str().to_string(),
                Value::String(palette.color(*role).to_hex()),
            )
        })
        .collect()
}

fn validate_identity(id: &str, label: &str) -> Result<(), ThemeFileError> {
    let id_valid = !id.is_empty()
        && id.len() <= MAX_THEME_ID_BYTES
        && id.chars().all(|character| {
            character.is_ascii_lowercase() || character.is_ascii_digit() || character == '-'
        });
    let label_valid = !label.trim().is_empty()
        && label.len() <= MAX_THEME_LABEL_BYTES
        && !label.chars().any(char::is_control);
    if id_valid && label_valid {
        Ok(())
    } else {
        Err(ThemeFileError::InvalidIdentity)
    }
}

fn take_string(object: &mut JsonMap<String, Value>, key: &str) -> Result<String, ThemeFileError> {
    object
        .remove(key)
        .and_then(|value| value.as_str().map(str::to_string))
        .ok_or(ThemeFileError::InvalidIdentity)
}

fn take_appearance(
    object: &mut JsonMap<String, Value>,
    key: &str,
) -> Result<ThemeAppearance, ThemeFileError> {
    let value = object
        .remove(key)
        .and_then(|value| value.as_str().map(str::to_string))
        .ok_or(ThemeFileError::InvalidAppearance)?;
    parse_appearance(&value)
}

fn parse_appearance(value: &str) -> Result<ThemeAppearance, ThemeFileError> {
    match value {
        "light" => Ok(ThemeAppearance::Light),
        "dark" => Ok(ThemeAppearance::Dark),
        _ => Err(ThemeFileError::InvalidAppearance),
    }
}

fn appearance_name(appearance: ThemeAppearance) -> &'static str {
    match appearance {
        ThemeAppearance::Light => "light",
        ThemeAppearance::Dark => "dark",
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StoredThemeState {
    pub library: ThemeLibrary,
    pub selection: ThemeSelection,
}

#[derive(Clone, Debug)]
pub struct ThemeStore {
    root: PathBuf,
}

impl ThemeStore {
    pub fn at(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn active() -> Result<Self, ThemeStoreError> {
        crate::persistence::app_config_dir()
            .map(Self::at)
            .map_err(|error| ThemeStoreError::Io(error.to_string()))
    }

    pub fn load(&self) -> Result<StoredThemeState, ThemeStoreError> {
        let path = self.root.join(APPEARANCE_FILE);
        let source = match fs::read_to_string(&path) {
            Ok(source) => source,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Ok(StoredThemeState {
                    library: ThemeLibrary::built_in(),
                    selection: ThemeSelection::default(),
                });
            }
            Err(error) => return Err(ThemeStoreError::Io(error.to_string())),
        };
        if source.len() as u64 > MAX_THEME_FILE_BYTES * 4 {
            return Err(ThemeStoreError::Invalid(
                "appearance library exceeds its bound".into(),
            ));
        }
        let wire = serde_json::from_str::<AppearanceWire>(&source)
            .map_err(|_| ThemeStoreError::Invalid("appearance library is malformed".into()))?;
        if wire.version != THEME_FILE_VERSION {
            return Err(ThemeStoreError::Invalid(
                "appearance library version is unsupported".into(),
            ));
        }
        if wire.themes.len() > MAX_CUSTOM_THEMES {
            return Err(ThemeStoreError::Invalid(
                "custom theme library exceeds its bound".into(),
            ));
        }
        let custom = wire
            .themes
            .into_iter()
            .map(|value| {
                serde_json::to_string(&value)
                    .map_err(|_| ThemeStoreError::Invalid("custom theme is malformed".into()))
                    .and_then(|source| parse_theme_file(&source).map_err(ThemeStoreError::Theme))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let library = ThemeLibrary::with_custom(custom).map_err(ThemeStoreError::Library)?;
        let selection = migrate_untouched_legacy_theme_selection(wire.selection);
        validate_selection(&selection, &library)?;
        Ok(StoredThemeState { library, selection })
    }

    pub fn save(
        &self,
        library: &ThemeLibrary,
        selection: &ThemeSelection,
    ) -> Result<(), ThemeStoreError> {
        validate_selection(selection, library)?;
        fs::create_dir_all(&self.root).map_err(|error| ThemeStoreError::Io(error.to_string()))?;
        let themes = library
            .custom_themes()
            .map(|theme| {
                serialize_theme_file(theme)
                    .map_err(ThemeStoreError::Theme)
                    .and_then(|source| {
                        serde_json::from_str::<Value>(&source).map_err(|_| {
                            ThemeStoreError::Invalid("serialized custom theme is malformed".into())
                        })
                    })
            })
            .collect::<Result<Vec<_>, _>>()?;
        let source = serde_json::to_vec_pretty(&AppearanceWire {
            version: THEME_FILE_VERSION,
            selection: selection.clone(),
            themes,
        })
        .map_err(|_| ThemeStoreError::Invalid("appearance library cannot be encoded".into()))?;
        let target = self.root.join(APPEARANCE_FILE);
        let temporary = self
            .root
            .join(format!(".{APPEARANCE_FILE}.{}.tmp", std::process::id()));
        let mut file = fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&temporary)
            .map_err(|error| ThemeStoreError::Io(error.to_string()))?;
        file.write_all(&source)
            .and_then(|_| file.sync_all())
            .map_err(|error| ThemeStoreError::Io(error.to_string()))?;
        drop(file);
        if target.exists() {
            let backup = self.root.join(format!(".{APPEARANCE_FILE}.bak"));
            let _ = fs::remove_file(&backup);
            fs::rename(&target, &backup).map_err(|error| ThemeStoreError::Io(error.to_string()))?;
            if let Err(error) = fs::rename(&temporary, &target) {
                let _ = fs::rename(&backup, &target);
                return Err(ThemeStoreError::Io(error.to_string()));
            }
            let _ = fs::remove_file(backup);
        } else {
            fs::rename(&temporary, &target)
                .map_err(|error| ThemeStoreError::Io(error.to_string()))?;
        }
        Ok(())
    }
}

fn validate_selection(
    selection: &ThemeSelection,
    library: &ThemeLibrary,
) -> Result<(), ThemeStoreError> {
    for (appearance, id) in [
        (ThemeAppearance::Light, selection.light_theme_id.as_str()),
        (ThemeAppearance::Dark, selection.dark_theme_id.as_str()),
    ] {
        let theme = library
            .get(id)
            .ok_or_else(|| ThemeStoreError::Invalid("selected theme is not installed".into()))?;
        if theme.palette(appearance).is_none() {
            return Err(ThemeStoreError::Invalid(
                "selected theme does not provide the requested appearance".into(),
            ));
        }
    }
    Ok(())
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AppearanceWire {
    version: u16,
    selection: ThemeSelection,
    themes: Vec<Value>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ThemeStoreError {
    Io(String),
    Invalid(String),
    Theme(ThemeFileError),
    Library(ThemeLibraryError),
}

impl fmt::Display for ThemeStoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(_) => formatter.write_str("appearance library could not be stored"),
            Self::Invalid(message) => formatter.write_str(message),
            Self::Theme(error) => error.fmt(formatter),
            Self::Library(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for ThemeStoreError {}

#[derive(Clone, Debug)]
pub struct ThemeController {
    store: ThemeStore,
    state: StoredThemeState,
    preview: Option<ThemePalette>,
}

impl ThemeController {
    pub fn load_at(root: impl Into<PathBuf>) -> Result<Self, ThemeStoreError> {
        let store = ThemeStore::at(root);
        let state = store.load()?;
        Ok(Self {
            store,
            state,
            preview: None,
        })
    }

    pub fn with_defaults_at(root: impl Into<PathBuf>) -> Self {
        Self {
            store: ThemeStore::at(root),
            state: StoredThemeState {
                library: ThemeLibrary::built_in(),
                selection: ThemeSelection::default(),
            },
            preview: None,
        }
    }

    pub fn library(&self) -> &ThemeLibrary {
        &self.state.library
    }

    pub fn selection(&self) -> &ThemeSelection {
        &self.state.selection
    }

    pub fn active_palette(&self, system: ThemeAppearance) -> &ThemePalette {
        if let Some(preview) = &self.preview {
            return preview;
        }
        let resolved = self.state.selection.resolve(system);
        self.state
            .library
            .get(&resolved.theme_id)
            .and_then(|theme| theme.palette(resolved.appearance))
            .or_else(|| {
                self.state
                    .library
                    .get("devmanager-classic")
                    .and_then(|theme| theme.palette(resolved.appearance))
            })
            .or_else(|| {
                self.state
                    .library
                    .get("t3-code")
                    .and_then(|theme| theme.palette(resolved.appearance))
            })
            .expect("built-in Classic or T3 Code theme provides both appearances")
    }

    pub fn set_appearance_preference(
        &mut self,
        appearance: AppearancePreference,
    ) -> Result<(), ThemeStoreError> {
        let previous = self.state.selection.appearance;
        self.state.selection.appearance = appearance;
        if let Err(error) = self.persist() {
            self.state.selection.appearance = previous;
            return Err(error);
        }
        Ok(())
    }

    pub fn select_theme(
        &mut self,
        theme_id: &str,
        appearance: ThemeAppearance,
    ) -> Result<(), ThemeStoreError> {
        let theme =
            self.state.library.get(theme_id).ok_or_else(|| {
                ThemeStoreError::Invalid("selected theme is not installed".into())
            })?;
        if theme.palette(appearance).is_none() {
            return Err(ThemeStoreError::Invalid(
                "selected theme does not provide that appearance".into(),
            ));
        }
        let previous = match appearance {
            ThemeAppearance::Light => std::mem::replace(
                &mut self.state.selection.light_theme_id,
                theme_id.to_string(),
            ),
            ThemeAppearance::Dark => std::mem::replace(
                &mut self.state.selection.dark_theme_id,
                theme_id.to_string(),
            ),
        };
        if let Err(error) = self.persist() {
            match appearance {
                ThemeAppearance::Light => self.state.selection.light_theme_id = previous,
                ThemeAppearance::Dark => self.state.selection.dark_theme_id = previous,
            }
            return Err(error);
        }
        self.preview = None;
        Ok(())
    }

    pub fn duplicate_theme(
        &mut self,
        source_id: &str,
        new_id: &str,
        new_label: &str,
    ) -> Result<ThemeDefinition, ThemeStoreError> {
        validate_identity(new_id, new_label).map_err(ThemeStoreError::Theme)?;
        if self.state.library.get(new_id).is_some() {
            return Err(ThemeStoreError::Invalid(
                "theme id is already installed".into(),
            ));
        }
        let source = self
            .state
            .library
            .get(source_id)
            .cloned()
            .ok_or_else(|| ThemeStoreError::Invalid("source theme is not installed".into()))?;
        let mut duplicate = source;
        duplicate.id = new_id.to_string();
        duplicate.label = new_label.to_string();
        duplicate.metadata.remove("collection");
        let custom = self
            .state
            .library
            .custom_themes()
            .cloned()
            .chain(std::iter::once(duplicate.clone()))
            .collect();
        let previous = self.state.library.clone();
        self.state.library = ThemeLibrary::with_custom(custom).map_err(ThemeStoreError::Library)?;
        if let Err(error) = self.persist() {
            self.state.library = previous;
            return Err(error);
        }
        Ok(duplicate)
    }

    pub fn save_managed_theme(
        &mut self,
        editing_id: Option<&str>,
        id: &str,
        label: &str,
        appearance: ThemeAppearance,
        canvas: ThemeColor,
        accent: ThemeColor,
    ) -> Result<ThemeDefinition, ThemeStoreError> {
        self.save_theme_palette(
            editing_id,
            id,
            label,
            appearance,
            ThemePalette::managed(appearance, canvas, accent),
            true,
        )
    }

    pub fn save_advanced_theme(
        &mut self,
        editing_id: Option<&str>,
        id: &str,
        label: &str,
        appearance: ThemeAppearance,
        palette: ThemePalette,
    ) -> Result<ThemeDefinition, ThemeStoreError> {
        self.save_theme_palette(editing_id, id, label, appearance, palette, false)
    }

    /// Persist a complete custom theme definition in one atomic store write.
    pub fn save_theme_definition(
        &mut self,
        editing_id: Option<&str>,
        id: &str,
        label: &str,
        palettes: BTreeMap<ThemeAppearance, ThemePalette>,
        managed: bool,
    ) -> Result<ThemeDefinition, ThemeStoreError> {
        validate_identity(id, label).map_err(ThemeStoreError::Theme)?;
        if is_built_in_theme_id(id) {
            return Err(ThemeStoreError::Invalid(
                "built-in themes must be duplicated before editing".into(),
            ));
        }
        if palettes.is_empty() {
            return Err(ThemeStoreError::Theme(ThemeFileError::MissingVariant));
        }
        for palette in palettes.values() {
            for role in ThemeColorRole::ALL {
                let _ = palette.color(*role);
            }
        }
        if self.state.library.get(id).is_some() && editing_id != Some(id) {
            return Err(ThemeStoreError::Invalid(
                "theme id is already installed".into(),
            ));
        }
        let mut saved = editing_id
            .and_then(|editing_id| self.state.library.get(editing_id))
            .cloned()
            .unwrap_or(ThemeDefinition {
                id: id.to_string(),
                label: label.to_string(),
                palettes: BTreeMap::new(),
                managed,
                metadata: JsonMap::new(),
            });
        saved.id = id.to_string();
        saved.label = label.to_string();
        saved.managed = managed;
        saved.palettes = palettes;
        let custom = self
            .state
            .library
            .custom_themes()
            .filter(|theme| Some(theme.id.as_str()) != editing_id)
            .cloned()
            .chain(std::iter::once(saved.clone()))
            .collect();
        self.replace_custom_library(custom)?;
        self.preview = None;
        Ok(saved)
    }

    fn save_theme_palette(
        &mut self,
        editing_id: Option<&str>,
        id: &str,
        label: &str,
        appearance: ThemeAppearance,
        palette: ThemePalette,
        managed: bool,
    ) -> Result<ThemeDefinition, ThemeStoreError> {
        let mut palettes = editing_id
            .and_then(|editing_id| self.state.library.get(editing_id))
            .map(|theme| theme.palettes.clone())
            .unwrap_or_default();
        palettes.insert(appearance, palette);
        self.save_theme_definition(editing_id, id, label, palettes, managed)
    }

    pub fn install_theme_file(&mut self, source: &str) -> Result<ThemeDefinition, ThemeStoreError> {
        let theme = parse_theme_file(source).map_err(ThemeStoreError::Theme)?;
        if self.state.library.get(&theme.id).is_some() {
            return Err(ThemeStoreError::Invalid(
                "theme id is already installed".into(),
            ));
        }
        let custom = self
            .state
            .library
            .custom_themes()
            .cloned()
            .chain(std::iter::once(theme.clone()))
            .collect();
        self.replace_custom_library(custom)?;
        Ok(theme)
    }

    pub fn export_theme(&self, theme_id: &str) -> Result<String, ThemeStoreError> {
        let theme = self
            .state
            .library
            .get(theme_id)
            .ok_or_else(|| ThemeStoreError::Invalid("theme is not installed".into()))?;
        serialize_theme_file(theme).map_err(ThemeStoreError::Theme)
    }

    pub fn remove_custom_theme(&mut self, theme_id: &str) -> Result<(), ThemeStoreError> {
        if is_built_in_theme_id(theme_id) {
            return Err(ThemeStoreError::Invalid(
                "built-in themes cannot be removed".into(),
            ));
        }
        if self.state.library.get(theme_id).is_none() {
            return Err(ThemeStoreError::Invalid("theme is not installed".into()));
        }
        let previous_selection = self.state.selection.clone();
        if self.state.selection.light_theme_id == theme_id {
            self.state.selection.light_theme_id = "t3-code".to_string();
        }
        if self.state.selection.dark_theme_id == theme_id {
            self.state.selection.dark_theme_id = "t3-code".to_string();
        }
        let custom = self
            .state
            .library
            .custom_themes()
            .filter(|theme| theme.id != theme_id)
            .cloned()
            .collect();
        if let Err(error) = self.replace_custom_library(custom) {
            self.state.selection = previous_selection;
            return Err(error);
        }
        self.preview = None;
        Ok(())
    }

    pub fn preview(&mut self, palette: ThemePalette) {
        self.preview = Some(palette);
    }

    pub fn cancel_preview(&mut self) {
        self.preview = None;
    }

    fn persist(&self) -> Result<(), ThemeStoreError> {
        self.store.save(&self.state.library, &self.state.selection)
    }

    fn replace_custom_library(
        &mut self,
        custom: Vec<ThemeDefinition>,
    ) -> Result<(), ThemeStoreError> {
        let previous = self.state.library.clone();
        self.state.library = ThemeLibrary::with_custom(custom).map_err(ThemeStoreError::Library)?;
        if let Err(error) = self.persist() {
            self.state.library = previous;
            return Err(error);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;
    use crate::ui::tokens::{contrast_ratio, Density, Scale};

    fn assert_channels_within(
        actual: ThemeColor,
        red: u8,
        green: u8,
        blue: u8,
        alpha: u8,
        tolerance: u8,
    ) {
        assert!(
            actual.red.abs_diff(red) <= tolerance,
            "red {} not within {tolerance} of {red}",
            actual.red
        );
        assert!(
            actual.green.abs_diff(green) <= tolerance,
            "green {} not within {tolerance} of {green}",
            actual.green
        );
        assert!(
            actual.blue.abs_diff(blue) <= tolerance,
            "blue {} not within {tolerance} of {blue}",
            actual.blue
        );
        assert_eq!(actual.alpha, alpha);
    }

    #[test]
    fn theme_color_parse_accepts_representative_rgb_and_rgba_values() {
        assert_eq!(
            ThemeColor::parse("rgb(214, 0, 87)").unwrap().to_hex(),
            "#d60057"
        );
        assert_eq!(
            ThemeColor::parse("rgb(214 0 87)").unwrap().to_hex(),
            "#d60057"
        );
        assert_eq!(
            ThemeColor::parse("rgb(100% 0% 0%)").unwrap().to_hex(),
            "#ff0000"
        );
        assert_eq!(
            ThemeColor::parse("rgba(255, 0, 0, 0.5)").unwrap().to_hex(),
            "#ff000080"
        );
        assert_eq!(
            ThemeColor::parse("rgb(0 128 255 / 40%)").unwrap().to_hex(),
            "#0080ff66"
        );
        assert_eq!(
            ThemeColor::parse("#d60057").unwrap().to_hex(),
            ThemeColor::parse("rgb(214, 0, 87)").unwrap().to_hex()
        );
    }

    #[test]
    fn theme_color_parse_converts_neutral_and_primary_oklch_with_tolerance() {
        assert_channels_within(ThemeColor::parse("oklch(0 0 0)").unwrap(), 0, 0, 0, 255, 0);
        assert_channels_within(
            ThemeColor::parse("oklch(1 0 0)").unwrap(),
            255,
            255,
            255,
            255,
            0,
        );
        // Mid neutral: L=0.5, C=0 → ~#636363 in clipped sRGB.
        assert_channels_within(
            ThemeColor::parse("oklch(0.5 0 0)").unwrap(),
            0x63,
            0x63,
            0x63,
            255,
            2,
        );
        assert_channels_within(
            ThemeColor::parse("oklch(50% 0 0)").unwrap(),
            0x63,
            0x63,
            0x63,
            255,
            2,
        );
        // sRGB primary red ≈ oklch(0.62796 0.25768 29.234)
        assert_channels_within(
            ThemeColor::parse("oklch(0.62796 0.25768 29.234)").unwrap(),
            255,
            0,
            0,
            255,
            3,
        );
        assert_channels_within(
            ThemeColor::parse("oklch(62.796% 0.25768 29.234)").unwrap(),
            255,
            0,
            0,
            255,
            3,
        );
    }

    #[test]
    fn out_of_gamut_oklch_maps_by_reducing_chroma_not_channel_clipping() {
        // Out-of-gamut green: hard channel-clip ≈ (0, 207, 0); T3 chroma search ≈ (0, 191, 52).
        let mapped = ThemeColor::parse("oklch(0.7 0.37 145)").unwrap();
        assert_channels_within(mapped, 0, 191, 52, 255, 3);

        let hard_clipped_green = 207_u8;
        let hard_clipped_blue = 0_u8;
        assert!(
            mapped.green < hard_clipped_green,
            "chroma mapping must darken the clipped green plateau (got {})",
            mapped.green
        );
        assert!(
            mapped.blue.abs_diff(hard_clipped_blue) > 20,
            "chroma mapping must keep hue by retaining blue instead of a clipped-to-zero plateau (got {})",
            mapped.blue
        );

        let original_chroma = 0.37;
        let resolved_chroma = map_oklch_chroma_into_srgb_gamut(0.7, original_chroma, 145.0);
        assert!(
            resolved_chroma < original_chroma - OKLCH_CHROMA_RESOLUTION,
            "out-of-gamut chroma must be reduced (resolved {resolved_chroma})"
        );
        assert!(
            linear_oklch_in_srgb_gamut(oklch_to_linear_srgb(0.7, resolved_chroma, 145.0)),
            "resolved chroma must be inside the T3 linear sRGB tolerance"
        );
        assert!(
            !linear_oklch_in_srgb_gamut(oklch_to_linear_srgb(0.7, original_chroma, 145.0)),
            "sample chroma must remain out of gamut before mapping"
        );

        let (linear_r, linear_g, linear_b) = oklch_to_linear_srgb(0.7, resolved_chroma, 145.0);
        let recomputed = ThemeColor {
            red: clamp_u8_channel(linear_to_srgb_channel(linear_r) * 255.0),
            green: clamp_u8_channel(linear_to_srgb_channel(linear_g) * 255.0),
            blue: clamp_u8_channel(linear_to_srgb_channel(linear_b) * 255.0),
            alpha: 255,
        };
        assert_eq!(recomputed, mapped);
        assert_eq!(
            oklch_to_srgb(0.7, original_chroma, 145.0),
            (mapped.red, mapped.green, mapped.blue)
        );
    }

    #[test]
    fn theme_color_parse_handles_oklch_alpha_and_rejects_malformed_input() {
        assert_channels_within(
            ThemeColor::parse("oklch(0.5 0 0 / 0.5)").unwrap(),
            0x63,
            0x63,
            0x63,
            128,
            2,
        );
        assert_eq!(
            ThemeColor::parse("oklch(50% 0 0 / 40%)").unwrap().alpha,
            102
        );
        assert!(ThemeColor::parse("").is_err());
        assert!(ThemeColor::parse("rgb()").is_err());
        assert!(ThemeColor::parse("rgb(255, 0)").is_err());
        assert!(ThemeColor::parse("rgba(255, 0, 0)").is_err());
        assert!(ThemeColor::parse("rgb(256, 0, 0)").is_err());
        assert!(ThemeColor::parse("oklch(1.5 0 0)").is_err());
        assert!(ThemeColor::parse("oklch(-0.1 0 0)").is_err());
        assert!(ThemeColor::parse("oklch(0.5 -0.1 0)").is_err());
        assert!(ThemeColor::parse("oklch(0.5 0)").is_err());
        assert!(ThemeColor::parse("oklch(0.5 0 0 /)").is_err());
        assert!(ThemeColor::parse("not-a-color").is_err());
        assert!(ThemeColor::parse("hsl(0 100% 50%)").is_err());
    }

    #[test]
    fn managed_t3_style_oklch_import_serializes_canonical_hex() {
        let source = r##"{
          "version": 1,
          "id": "t3-oklch",
          "name": "T3 OKLCH",
          "appearance": "dark",
          "managed": true,
          "colors": {
            "canvas": "oklch(0.18 0.02 290)",
            "accent": "oklch(62.8% 0.2577 29.234 / 1)"
          }
        }"##;

        let theme = parse_theme_file(source).unwrap();
        let palette = theme.palette(ThemeAppearance::Dark).unwrap();
        let canvas = palette.color(ThemeColorRole::Canvas);
        let accent = palette.color(ThemeColorRole::Accent);

        assert_eq!(canvas.alpha, 255);
        assert_eq!(accent.alpha, 255);
        assert!(canvas.to_hex().starts_with('#') && !canvas.to_hex().contains('('));
        assert!(accent.to_hex().starts_with('#') && !accent.to_hex().contains('('));
        assert_channels_within(accent, 255, 0, 0, 255, 4);

        let serialized = serialize_theme_file(&theme).unwrap();
        assert!(serialized.contains(&format!("\"canvas\": \"{}\"", canvas.to_hex())));
        assert!(serialized.contains(&format!("\"accent\": \"{}\"", accent.to_hex())));
        assert!(!serialized.to_ascii_lowercase().contains("oklch("));
        assert!(!serialized.to_ascii_lowercase().contains("rgb("));

        let reparsed = parse_theme_file(&serialized).unwrap();
        assert_eq!(reparsed, theme);
    }

    #[test]
    fn built_in_library_exposes_each_t3_theme_in_light_and_dark() {
        let library = ThemeLibrary::built_in();
        let labels = library
            .themes()
            .iter()
            .map(|theme| theme.label.as_str())
            .collect::<BTreeSet<_>>();

        assert_eq!(
            labels,
            BTreeSet::from([
                "DevManager Classic",
                "Ember",
                "Grove",
                "Iris",
                "Ocean",
                "T3 Chat",
                "T3 Code",
            ])
        );
        assert!(library.themes().iter().all(|theme| {
            theme.palette(ThemeAppearance::Light).is_some()
                && theme.palette(ThemeAppearance::Dark).is_some()
        }));
        let classic = library
            .get("devmanager-classic")
            .expect("legacy-readable default theme");
        let dark = classic.palette(ThemeAppearance::Dark).unwrap();
        // Classic's dark half is the redesign shell, so it is pinned against
        // the token module rather than restated as literals.
        let redesign = crate::ui::tokens::dark(Density::Comfortable, Scale::Scale100);
        assert_eq!(
            dark.color(ThemeColorRole::Canvas).opaque(),
            redesign.surfaces.canvas
        );
        assert_eq!(
            dark.color(ThemeColorRole::Sidebar).opaque(),
            redesign.surfaces.raised
        );
        assert_eq!(
            dark.color(ThemeColorRole::Text).opaque(),
            redesign.text.primary
        );
        assert_eq!(
            dark.color(ThemeColorRole::TerminalBackground).opaque(),
            redesign.terminal.background
        );
        assert_eq!(
            ThemeSelection::default().appearance,
            AppearancePreference::Dark
        );
        assert_eq!(
            ThemeSelection::default().dark_theme_id,
            "devmanager-classic"
        );
    }

    #[test]
    fn untouched_legacy_system_t3_selection_migrates_to_classic_defaults() {
        let legacy = ThemeSelection {
            appearance: AppearancePreference::System,
            light_theme_id: "t3-code".to_string(),
            dark_theme_id: "t3-code".to_string(),
        };
        assert_eq!(
            migrate_untouched_legacy_theme_selection(legacy),
            ThemeSelection::default()
        );

        let explicit = ThemeSelection {
            appearance: AppearancePreference::System,
            light_theme_id: "ocean".to_string(),
            dark_theme_id: "t3-code".to_string(),
        };
        assert_eq!(
            migrate_untouched_legacy_theme_selection(explicit.clone()),
            explicit
        );

        let root = tempfile::tempdir().unwrap();
        let store = ThemeStore::at(root.path());
        let legacy = ThemeSelection {
            appearance: AppearancePreference::System,
            light_theme_id: "t3-code".to_string(),
            dark_theme_id: "t3-code".to_string(),
        };
        store.save(&ThemeLibrary::built_in(), &legacy).unwrap();
        let recovered = store.load().unwrap();
        assert_eq!(recovered.selection, ThemeSelection::default());
    }

    #[test]
    fn system_mode_uses_the_matching_half_without_losing_per_mode_selection() {
        let selection = ThemeSelection {
            appearance: AppearancePreference::System,
            light_theme_id: "ocean".to_string(),
            dark_theme_id: "iris".to_string(),
        };

        assert_eq!(
            selection.resolve(ThemeAppearance::Light),
            ResolvedThemeSelection {
                appearance: ThemeAppearance::Light,
                theme_id: "ocean".to_string(),
            }
        );
        assert_eq!(
            selection.resolve(ThemeAppearance::Dark),
            ResolvedThemeSelection {
                appearance: ThemeAppearance::Dark,
                theme_id: "iris".to_string(),
            }
        );
    }

    #[test]
    fn managed_palette_keeps_text_and_accent_foregrounds_readable() {
        let palette = ThemePalette::managed(
            ThemeAppearance::Dark,
            ThemeColor::from_hex("#1f1a24").unwrap(),
            ThemeColor::from_hex("#d60057").unwrap(),
        );

        assert!(
            contrast_ratio(
                palette.color(ThemeColorRole::Text).opaque(),
                palette.color(ThemeColorRole::Canvas).opaque(),
            ) >= 4.5
        );
        assert!(
            contrast_ratio(
                palette.color(ThemeColorRole::AccentForeground).opaque(),
                palette.color(ThemeColorRole::Accent).opaque(),
            ) >= 4.5
        );
    }

    #[test]
    fn custom_theme_round_trip_preserves_both_palettes_and_unknown_metadata() {
        let source = r##"{
          "version": 1,
          "id": "aurora",
          "name": "Aurora",
          "appearance": "light",
          "colors": { "canvas": "#fafafa", "accent": "#0055cc" },
          "variants": { "dark": { "canvas": "#111319", "accent": "#79a7ff" } },
          "managed": true,
          "author": "Robin"
        }"##;

        let theme = parse_theme_file(source).unwrap();
        let serialized = serialize_theme_file(&theme).unwrap();
        let reparsed = parse_theme_file(&serialized).unwrap();

        assert_eq!(reparsed, theme);
        assert_eq!(
            reparsed
                .metadata
                .get("author")
                .and_then(serde_json::Value::as_str),
            Some("Robin")
        );
        assert_eq!(
            reparsed
                .palette(ThemeAppearance::Dark)
                .unwrap()
                .color(ThemeColorRole::Canvas)
                .to_hex(),
            "#111319"
        );
    }

    #[test]
    fn imported_advanced_theme_rejects_a_missing_semantic_role() {
        let source = r##"{
          "version": 1,
          "id": "broken",
          "name": "Broken",
          "appearance": "dark",
          "colors": { "canvas": "#111111", "text": "#eeeeee" }
        }"##;

        let error = parse_theme_file(source).unwrap_err();

        assert!(matches!(
            error,
            ThemeFileError::MissingRole(ThemeColorRole::Chrome)
        ));
    }

    #[test]
    fn theme_tokens_use_palette_surfaces_actions_status_and_terminal_colors() {
        let palette = ThemePalette::managed(
            ThemeAppearance::Dark,
            ThemeColor::from_hex("#1f1a24").unwrap(),
            ThemeColor::from_hex("#d60057").unwrap(),
        );

        let tokens = palette.tokens(Density::Compact, Scale::Scale125);

        assert_eq!(
            tokens.surfaces.canvas,
            palette.color(ThemeColorRole::Canvas).opaque()
        );
        assert_eq!(
            tokens.actions.primary.default.background,
            palette.color(ThemeColorRole::Accent).opaque()
        );
        assert_eq!(
            tokens.status.destructive,
            palette.color(ThemeColorRole::Error).opaque()
        );
        assert_eq!(
            tokens.terminal.background,
            palette.color(ThemeColorRole::TerminalBackground).opaque()
        );
        assert_eq!(tokens.density.density, Density::Compact);
        assert_eq!(tokens.density.scale, Scale::Scale125);
    }

    /// Drift guard for the redesign shell.
    ///
    /// The running app never reads `crate::ui::tokens::dark` directly: it goes
    /// through `ThemeController::active_palette(..).tokens(..)`, which starts
    /// from the token module and then overwrites most surface, text and border
    /// tokens from the active palette's roles. So a fresh profile's dark look
    /// is the built-in Classic palette, not the token module, and the two can
    /// drift silently. Everything the redesign pins is asserted equal here.
    #[test]
    fn default_dark_palette_tokens_match_the_redesign_token_module() {
        let root = tempfile::tempdir().unwrap();
        let controller = ThemeController::with_defaults_at(root.path());
        let density = Density::Comfortable;
        let scale = Scale::Scale100;

        let tokens = controller
            .active_palette(ThemeAppearance::Dark)
            .tokens(density, scale);
        let expected = crate::ui::tokens::dark(density, scale);

        let checks: &[(&str, Color, Color)] = &[
            (
                "surfaces.canvas",
                tokens.surfaces.canvas,
                expected.surfaces.canvas,
            ),
            (
                "surfaces.raised",
                tokens.surfaces.raised,
                expected.surfaces.raised,
            ),
            (
                "surfaces.sunken",
                tokens.surfaces.sunken,
                expected.surfaces.sunken,
            ),
            (
                "surfaces.selection",
                tokens.surfaces.selection,
                expected.surfaces.selection,
            ),
            (
                "surfaces.hover",
                tokens.surfaces.hover,
                expected.surfaces.hover,
            ),
            (
                "surfaces.overlay",
                tokens.surfaces.overlay,
                expected.surfaces.overlay,
            ),
            (
                "surfaces.disabled",
                tokens.surfaces.disabled,
                expected.surfaces.disabled,
            ),
            ("text.primary", tokens.text.primary, expected.text.primary),
            (
                "text.secondary",
                tokens.text.secondary,
                expected.text.secondary,
            ),
            ("text.muted", tokens.text.muted, expected.text.muted),
            (
                "text.disabled",
                tokens.text.disabled,
                expected.text.disabled,
            ),
            (
                "text.emphasis",
                tokens.text.emphasis,
                expected.text.emphasis,
            ),
            (
                "borders.subtle",
                tokens.borders.subtle,
                expected.borders.subtle,
            ),
            (
                "borders.default",
                tokens.borders.default,
                expected.borders.default,
            ),
            (
                "borders.strong",
                tokens.borders.strong,
                expected.borders.strong,
            ),
            // The roles are a lossy projection of the tokens, so every token a
            // role could quietly overwrite is pinned here: these eight are the
            // ones the redesign's inverted primary action would otherwise drag
            // with it.
            (
                "text.on_accent",
                tokens.text.on_accent,
                expected.text.on_accent,
            ),
            ("text.inverse", tokens.text.inverse, expected.text.inverse),
            (
                "borders.focus",
                tokens.borders.focus,
                expected.borders.focus,
            ),
            (
                "borders.selection",
                tokens.borders.selection,
                expected.borders.selection,
            ),
            (
                "actions.primary.default.background",
                tokens.actions.primary.default.background,
                expected.actions.primary.default.background,
            ),
            (
                "actions.primary.default.foreground",
                tokens.actions.primary.default.foreground,
                expected.actions.primary.default.foreground,
            ),
            (
                "actions.primary.hover.background",
                tokens.actions.primary.hover.background,
                expected.actions.primary.hover.background,
            ),
            (
                "actions.primary.selected.background",
                tokens.actions.primary.selected.background,
                expected.actions.primary.selected.background,
            ),
            (
                "actions.primary.disabled.foreground",
                tokens.actions.primary.disabled.foreground,
                expected.actions.primary.disabled.foreground,
            ),
            // The scrollbar's two thumb colours make the round trip through
            // `terminalScrollbar`/`terminalScrollbarHover`, so they can drift
            // exactly the way the surface roles can.
            (
                "scrollbar.on_dark.thumb_idle",
                tokens.scrollbar.on_dark.thumb_idle,
                expected.scrollbar.on_dark.thumb_idle,
            ),
            (
                "scrollbar.on_dark.thumb_hover",
                tokens.scrollbar.on_dark.thumb_hover,
                expected.scrollbar.on_dark.thumb_hover,
            ),
            (
                "scrollbar.on_dark.track_active",
                tokens.scrollbar.on_dark.track_active,
                expected.scrollbar.on_dark.track_active,
            ),
            (
                "scrollbar.on_light.thumb_idle",
                tokens.scrollbar.on_light.thumb_idle,
                expected.scrollbar.on_light.thumb_idle,
            ),
            (
                "scrollbar.on_light.thumb_hover",
                tokens.scrollbar.on_light.thumb_hover,
                expected.scrollbar.on_light.thumb_hover,
            ),
            (
                "status.attention",
                tokens.status.attention,
                expected.status.attention,
            ),
            (
                "status.destructive",
                tokens.status.destructive,
                expected.status.destructive,
            ),
            (
                "status.success",
                tokens.status.success,
                expected.status.success,
            ),
            (
                "terminal.background",
                tokens.terminal.background,
                expected.terminal.background,
            ),
        ];

        let drift = checks
            .iter()
            .filter(|(_, actual, wanted)| actual != wanted)
            .map(|(name, actual, wanted)| {
                format!(
                    "{name}: palette {} != tokens {}",
                    actual.to_hex(),
                    wanted.to_hex()
                )
            })
            .collect::<Vec<_>>();
        assert!(
            drift.is_empty(),
            "default dark palette drifted from the token module:\n  {}",
            drift.join("\n  ")
        );
    }

    #[test]
    fn advanced_palette_and_controller_require_and_persist_every_semantic_role() {
        let root = tempfile::tempdir().unwrap();
        let source = ThemePalette::managed(
            ThemeAppearance::Dark,
            ThemeColor::from_hex("#151821").unwrap(),
            ThemeColor::from_hex("#6f9dff").unwrap(),
        );
        let mut colors = ThemeColorRole::ALL
            .iter()
            .map(|role| (*role, source.color(*role)))
            .collect::<BTreeMap<_, _>>();
        colors.remove(&ThemeColorRole::TerminalCursor);
        assert!(matches!(
            ThemePalette::advanced(colors),
            Err(ThemeFileError::MissingRole(ThemeColorRole::TerminalCursor))
        ));

        let complete = ThemeColorRole::ALL
            .iter()
            .map(|role| (*role, source.color(*role)))
            .collect::<BTreeMap<_, _>>();
        let mut controller = ThemeController::load_at(root.path()).unwrap();
        let saved = controller
            .save_advanced_theme(
                None,
                "night-operator",
                "Night Operator",
                ThemeAppearance::Dark,
                ThemePalette::advanced(complete).unwrap(),
            )
            .unwrap();

        assert!(!saved.managed);
        assert!(
            !ThemeController::load_at(root.path())
                .unwrap()
                .library()
                .get("night-operator")
                .unwrap()
                .managed
        );
    }

    #[test]
    fn persisted_library_and_selection_recover_without_touching_operational_config() {
        let root = tempfile::tempdir().unwrap();
        let store = ThemeStore::at(root.path());
        let theme = ThemeDefinition::managed(
            "aurora",
            "Aurora",
            ThemeAppearance::Dark,
            ThemeColor::from_hex("#151821").unwrap(),
            ThemeColor::from_hex("#6f9dff").unwrap(),
        );
        let selection = ThemeSelection {
            appearance: AppearancePreference::Dark,
            light_theme_id: "t3-code".to_string(),
            dark_theme_id: "aurora".to_string(),
        };

        store
            .save(
                &ThemeLibrary::with_custom(vec![theme.clone()]).unwrap(),
                &selection,
            )
            .unwrap();
        let recovered = store.load().unwrap();

        assert_eq!(recovered.selection, selection);
        assert_eq!(recovered.library.get("aurora"), Some(&theme));
        assert!(root.path().join("appearance.json").is_file());
        assert!(!root.path().join("config.json").exists());
    }

    #[test]
    fn selecting_a_theme_updates_only_the_active_appearance_half() {
        let root = tempfile::tempdir().unwrap();
        let mut controller = ThemeController::load_at(root.path()).unwrap();

        controller
            .set_appearance_preference(AppearancePreference::Dark)
            .unwrap();
        controller
            .select_theme("iris", ThemeAppearance::Dark)
            .unwrap();

        assert_eq!(controller.selection().light_theme_id, "devmanager-classic");
        assert_eq!(controller.selection().dark_theme_id, "iris");
        let recovered = ThemeController::load_at(root.path()).unwrap();
        assert_eq!(recovered.selection(), controller.selection());
    }

    #[test]
    fn canceling_a_live_preview_restores_the_stored_palette() {
        let root = tempfile::tempdir().unwrap();
        let mut controller = ThemeController::load_at(root.path()).unwrap();
        let original = controller
            .active_palette(ThemeAppearance::Dark)
            .color(ThemeColorRole::Canvas);
        let preview = ThemePalette::managed(
            ThemeAppearance::Dark,
            ThemeColor::from_hex("#030405").unwrap(),
            ThemeColor::from_hex("#66aaff").unwrap(),
        );

        controller.preview(preview.clone());
        assert_eq!(
            controller
                .active_palette(ThemeAppearance::Dark)
                .color(ThemeColorRole::Canvas),
            preview.color(ThemeColorRole::Canvas)
        );
        controller.cancel_preview();

        assert_eq!(
            controller
                .active_palette(ThemeAppearance::Dark)
                .color(ThemeColorRole::Canvas),
            original
        );
    }

    #[test]
    fn duplicating_a_builtin_creates_an_editable_custom_theme() {
        let root = tempfile::tempdir().unwrap();
        let mut controller = ThemeController::load_at(root.path()).unwrap();

        let duplicate = controller
            .duplicate_theme("t3-chat", "t3-chat-copy", "T3 Chat copy")
            .unwrap();

        assert_eq!(duplicate.id, "t3-chat-copy");
        assert_eq!(duplicate.label, "T3 Chat copy");
        assert!(duplicate.palette(ThemeAppearance::Light).is_some());
        assert!(duplicate.palette(ThemeAppearance::Dark).is_some());
        assert_eq!(
            ThemeController::load_at(root.path())
                .unwrap()
                .library()
                .get("t3-chat-copy"),
            Some(&duplicate)
        );
    }

    #[test]
    fn installing_and_removing_a_custom_theme_repairs_only_its_selected_half() {
        let root = tempfile::tempdir().unwrap();
        let mut controller = ThemeController::load_at(root.path()).unwrap();
        let installed = controller
            .save_managed_theme(
                None,
                "aurora",
                "Aurora",
                ThemeAppearance::Dark,
                ThemeColor::from_hex("#151821").unwrap(),
                ThemeColor::from_hex("#6f9dff").unwrap(),
            )
            .unwrap();
        controller
            .select_theme(&installed.id, ThemeAppearance::Dark)
            .unwrap();

        controller.remove_custom_theme("aurora").unwrap();

        assert_eq!(controller.selection().light_theme_id, "devmanager-classic");
        assert_eq!(controller.selection().dark_theme_id, "t3-code");
        assert!(controller.library().get("aurora").is_none());
    }

    #[test]
    fn imported_t3_theme_json_becomes_an_installed_selectable_theme() {
        let root = tempfile::tempdir().unwrap();
        let mut controller = ThemeController::load_at(root.path()).unwrap();
        let source = r##"{
          "version": 1,
          "id": "night-sky",
          "name": "Night Sky",
          "appearance": "dark",
          "colors": { "canvas": "#111827", "accent": "#60a5fa" },
          "managed": true
        }"##;

        let installed = controller.install_theme_file(source).unwrap();
        controller
            .select_theme(&installed.id, ThemeAppearance::Dark)
            .unwrap();

        assert_eq!(controller.selection().dark_theme_id, "night-sky");
        assert_eq!(
            controller.export_theme("night-sky").unwrap(),
            serialize_theme_file(&installed).unwrap()
        );
    }

    #[test]
    fn semantic_built_ins_match_every_exact_t3_source_role() {
        let library = ThemeLibrary::built_in();
        let tables = [
            ("t3-chat", ThemeAppearance::Light, T3_CHAT_LIGHT_ROLES),
            ("t3-chat", ThemeAppearance::Dark, T3_CHAT_DARK_ROLES),
            ("grove", ThemeAppearance::Light, GROVE_LIGHT_ROLES),
            ("grove", ThemeAppearance::Dark, GROVE_DARK_ROLES),
            ("ocean", ThemeAppearance::Light, OCEAN_LIGHT_ROLES),
            ("ocean", ThemeAppearance::Dark, OCEAN_DARK_ROLES),
            ("ember", ThemeAppearance::Light, EMBER_LIGHT_ROLES),
            ("ember", ThemeAppearance::Dark, EMBER_DARK_ROLES),
            ("iris", ThemeAppearance::Light, IRIS_LIGHT_ROLES),
            ("iris", ThemeAppearance::Dark, IRIS_DARK_ROLES),
        ];

        for (theme_id, appearance, roles) in tables {
            let theme = library.get(theme_id).expect("semantic built-in present");
            assert!(
                !theme.managed,
                "{theme_id} must keep full semantic (non-managed) authority"
            );
            let palette = theme
                .palette(appearance)
                .expect("semantic built-in provides appearance");
            assert_eq!(
                roles.len(),
                ThemeColorRole::ALL.len(),
                "{theme_id}/{appearance:?} source table must cover every ThemeColorRole"
            );
            let mut seen = BTreeSet::new();
            for &(role_name, oklch) in roles {
                let role = ThemeColorRole::parse(role_name)
                    .unwrap_or_else(|| panic!("unknown source role {role_name}"));
                assert!(
                    seen.insert(role),
                    "duplicate source role {role_name} in {theme_id}/{appearance:?}"
                );
                let expected = ThemeColor::parse(oklch).unwrap_or_else(|_| {
                    panic!("source OKLCH for {theme_id}/{appearance:?}/{role_name} must parse")
                });
                assert_eq!(
                    palette.color(role),
                    expected,
                    "{theme_id}/{appearance:?}/{role_name} must equal parsed source OKLCH"
                );
            }
            assert_eq!(seen.len(), ThemeColorRole::ALL.len());
            for role in ThemeColorRole::ALL {
                assert!(
                    seen.contains(role),
                    "{theme_id}/{appearance:?} missing role {}",
                    role.as_str()
                );
            }
        }

        let t3_code = library.get("t3-code").expect("T3 Code default");
        assert!(t3_code.managed);
        assert_eq!(
            t3_code
                .palette(ThemeAppearance::Light)
                .unwrap()
                .color(ThemeColorRole::Canvas)
                .to_hex(),
            "#fbfafc"
        );
        assert_eq!(
            t3_code
                .palette(ThemeAppearance::Dark)
                .unwrap()
                .color(ThemeColorRole::Canvas)
                .to_hex(),
            "#18151d"
        );
    }

    #[test]
    fn semantic_built_in_spot_checks_cover_all_five_pairs() {
        let library = ThemeLibrary::built_in();
        let spots = [
            (
                "t3-chat",
                ThemeAppearance::Light,
                ThemeColorRole::Accent,
                "oklch(0.591646 0.217985 0.584)",
            ),
            (
                "t3-chat",
                ThemeAppearance::Dark,
                ThemeColorRole::Sidebar,
                "oklch(0.185778 0.019368 322.159)",
            ),
            (
                "grove",
                ThemeAppearance::Light,
                ThemeColorRole::Canvas,
                "oklch(0.972369 0.005497 157.15)",
            ),
            (
                "grove",
                ThemeAppearance::Dark,
                ThemeColorRole::Accent,
                "oklch(0.796228 0.133058 157.319)",
            ),
            (
                "ocean",
                ThemeAppearance::Light,
                ThemeColorRole::Text,
                "oklch(0.222003 0.03479 328.979)",
            ),
            (
                "ocean",
                ThemeAppearance::Dark,
                ThemeColorRole::TerminalBackground,
                "oklch(0.242641 0.024125 250.573)",
            ),
            (
                "ember",
                ThemeAppearance::Light,
                ThemeColorRole::MessageAction,
                "oklch(0.516323 0.161628 24.82)",
            ),
            (
                "ember",
                ThemeAppearance::Dark,
                ThemeColorRole::Warning,
                "oklch(0.772406 0.172798 65.367)",
            ),
            (
                "iris",
                ThemeAppearance::Light,
                ThemeColorRole::Focus,
                "oklch(0.525348 0.15373 294.176)",
            ),
            (
                "iris",
                ThemeAppearance::Dark,
                ThemeColorRole::SidebarRowSelected,
                "oklch(0.372806 0.078525 294.203)",
            ),
        ];

        for (theme_id, appearance, role, oklch) in spots {
            let actual = library
                .get(theme_id)
                .and_then(|theme| theme.palette(appearance))
                .map(|palette| palette.color(role))
                .expect("spot-check palette");
            assert_eq!(
                actual,
                ThemeColor::parse(oklch).unwrap(),
                "{theme_id}/{appearance:?}/{} spot check",
                role.as_str()
            );
        }
    }

    #[test]
    fn save_theme_definition_persists_paired_light_and_dark_atomically() {
        let root = tempfile::tempdir().unwrap();
        let mut controller = ThemeController::load_at(root.path()).unwrap();
        let light = ThemePalette::managed(
            ThemeAppearance::Light,
            ThemeColor::from_hex("#f5f7fb").unwrap(),
            ThemeColor::from_hex("#2f6fed").unwrap(),
        );
        let dark = ThemePalette::managed(
            ThemeAppearance::Dark,
            ThemeColor::from_hex("#12151c").unwrap(),
            ThemeColor::from_hex("#7aa2ff").unwrap(),
        );
        let palettes = BTreeMap::from([
            (ThemeAppearance::Light, light.clone()),
            (ThemeAppearance::Dark, dark.clone()),
        ]);

        let saved = controller
            .save_theme_definition(None, "paired-aurora", "Paired Aurora", palettes, true)
            .unwrap();

        assert!(saved.managed);
        assert_eq!(
            saved
                .palette(ThemeAppearance::Light)
                .unwrap()
                .color(ThemeColorRole::Canvas),
            light.color(ThemeColorRole::Canvas)
        );
        assert_eq!(
            saved
                .palette(ThemeAppearance::Dark)
                .unwrap()
                .color(ThemeColorRole::Accent),
            dark.color(ThemeColorRole::Accent)
        );

        let recovered = ThemeController::load_at(root.path())
            .unwrap()
            .library()
            .get("paired-aurora")
            .cloned()
            .expect("paired theme recovered");
        assert_eq!(recovered, saved);
        assert!(recovered.palette(ThemeAppearance::Light).is_some());
        assert!(recovered.palette(ThemeAppearance::Dark).is_some());
    }

    #[test]
    fn rejected_theme_definition_edit_leaves_prior_stored_definition_unchanged() {
        let root = tempfile::tempdir().unwrap();
        let mut controller = ThemeController::load_at(root.path()).unwrap();
        let original = controller
            .save_managed_theme(
                None,
                "keep-me",
                "Keep Me",
                ThemeAppearance::Dark,
                ThemeColor::from_hex("#101218").unwrap(),
                ThemeColor::from_hex("#59a1ff").unwrap(),
            )
            .unwrap();
        controller
            .save_managed_theme(
                None,
                "other-theme",
                "Other Theme",
                ThemeAppearance::Light,
                ThemeColor::from_hex("#f0f0f0").unwrap(),
                ThemeColor::from_hex("#3366cc").unwrap(),
            )
            .unwrap();
        controller.preview(original.palette(ThemeAppearance::Dark).unwrap().clone());

        let colliding = controller.save_theme_definition(
            Some("keep-me"),
            "other-theme",
            "Should Fail",
            BTreeMap::from([(
                ThemeAppearance::Dark,
                ThemePalette::managed(
                    ThemeAppearance::Dark,
                    ThemeColor::from_hex("#000000").unwrap(),
                    ThemeColor::from_hex("#ffffff").unwrap(),
                ),
            )]),
            true,
        );
        assert!(colliding.is_err());

        let empty = controller.save_theme_definition(
            Some("keep-me"),
            "keep-me",
            "Keep Me",
            BTreeMap::new(),
            true,
        );
        assert!(matches!(
            empty,
            Err(ThemeStoreError::Theme(ThemeFileError::MissingVariant))
        ));

        let recovered = controller.library().get("keep-me").cloned().unwrap();
        assert_eq!(recovered, original);
        let reloaded = ThemeController::load_at(root.path())
            .unwrap()
            .library()
            .get("keep-me")
            .cloned()
            .unwrap();
        assert_eq!(reloaded, original);
        assert!(
            controller.preview.is_some(),
            "rejected edit must leave preview uncleared"
        );
    }
}
