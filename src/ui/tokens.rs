//! Semantic visual tokens for the native Task Cockpit.
//!
//! This module is deliberately the only place where UI color values are
//! defined. Consumers choose a meaning from [`ThemeTokens`] and do not need
//! to know which palette value implements it.

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ThemeMode {
    Dark,
    Light,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Density {
    Compact,
    Comfortable,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Scale {
    Scale100,
    Scale125,
    Scale150,
    Scale200,
}

pub type WindowsScale = Scale;

impl Scale {
    pub const fn percent(self) -> u16 {
        match self {
            Self::Scale100 => 100,
            Self::Scale125 => 125,
            Self::Scale150 => 150,
            Self::Scale200 => 200,
        }
    }

    pub const fn factor(self) -> f32 {
        self.percent() as f32 / 100.0
    }

    /// Resolve the nearest supported Windows display scale once, before a
    /// render pass starts. Native layout consumes the resulting immutable
    /// snapshot rather than querying the window while painting.
    pub fn from_factor(factor: f32) -> Self {
        let percent = (factor.max(1.0) * 100.0).round() as u16;
        match percent {
            0..=112 => Self::Scale100,
            113..=137 => Self::Scale125,
            138..=174 => Self::Scale150,
            _ => Self::Scale200,
        }
    }
}

/// The only runtime preference input consumed by the native shell. It is a
/// copyable snapshot so appearance and display scale are resolved at window
/// creation (or by a future preferences event), never during GPUI render.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RuntimePreferencesSnapshot {
    mode: ThemeMode,
    density: Density,
    scale: Scale,
}

impl Default for RuntimePreferencesSnapshot {
    fn default() -> Self {
        Self::new(ThemeMode::Dark, Density::Comfortable, Scale::Scale100)
    }
}

impl RuntimePreferencesSnapshot {
    pub const fn new(mode: ThemeMode, density: Density, scale: Scale) -> Self {
        Self {
            mode,
            density,
            scale,
        }
    }

    pub fn from_system(
        appearance: gpui::WindowAppearance,
        scale_factor: f32,
        density: Density,
    ) -> Self {
        let mode = match appearance {
            gpui::WindowAppearance::Light | gpui::WindowAppearance::VibrantLight => {
                ThemeMode::Light
            }
            gpui::WindowAppearance::Dark | gpui::WindowAppearance::VibrantDark => ThemeMode::Dark,
        };
        Self::new(mode, density, Scale::from_factor(scale_factor))
    }

    pub const fn mode(self) -> ThemeMode {
        self.mode
    }

    pub const fn density(self) -> Density {
        self.density
    }

    pub const fn scale(self) -> Scale {
        self.scale
    }

    pub fn tokens(self) -> ThemeTokens {
        theme(self.mode, self.density, self.scale)
    }

    pub fn metrics(self) -> PhysicalDensityMetrics {
        self.tokens().density.physical()
    }

    /// Conservative minimum window check used by acceptance tests and by the
    /// shell's initial bounds. It catches scale regressions before a window
    /// is painted, without requiring a platform capture in unit tests.
    pub fn layout_fits(self, width: u32, height: u32) -> bool {
        let metrics = self.metrics();
        width
            >= metrics
                .label_min_width
                .saturating_add(metrics.control_padding * 8)
            && height >= metrics.control_height.saturating_mul(4)
    }
}

/// An opaque sRGB color. Transparency is intentionally not part of the UI
/// token contract: compositing belongs to the surface that owns a token.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Color {
    rgba: [u8; 4],
}

impl Color {
    pub const fn rgb(red: u8, green: u8, blue: u8) -> Self {
        Self {
            rgba: [red, green, blue, 255],
        }
    }

    pub const fn from_u32(value: u32) -> Self {
        Self::rgb(
            ((value >> 16) & 0xff) as u8,
            ((value >> 8) & 0xff) as u8,
            (value & 0xff) as u8,
        )
    }

    /// Convert a semantic token to GPUI's opaque native color without exposing
    /// palette literals to UI consumers.
    pub fn to_gpui(self) -> gpui::Rgba {
        gpui::rgb(
            (u32::from(self.red()) << 16) | (u32::from(self.green()) << 8) | u32::from(self.blue()),
        )
    }

    pub const fn red(self) -> u8 {
        self.rgba[0]
    }

    pub const fn green(self) -> u8 {
        self.rgba[1]
    }

    pub const fn blue(self) -> u8 {
        self.rgba[2]
    }

    pub const fn alpha(self) -> u8 {
        self.rgba[3]
    }

    pub const fn is_opaque(self) -> bool {
        self.alpha() == 255
    }

    pub const fn to_u32(self) -> u32 {
        ((self.red() as u32) << 16) | ((self.green() as u32) << 8) | self.blue() as u32
    }

    pub fn to_hex(self) -> String {
        format!("#{:02X}{:02X}{:02X}", self.red(), self.green(), self.blue())
    }
}

/// Deterministic preview capture marker. It is intentionally a token-module
/// constant so preview rendering does not become a second color source.
pub const PREVIEW_SENTINEL: Color = Color::from_u32(0x912bd4);

/// Return the relative luminance of an opaque sRGB color using the WCAG 2.x
/// transfer function. The calculation is intentionally independent of a UI
/// renderer so tests and later clients share the same result.
pub fn srgb_luminance(color: Color) -> f64 {
    fn linear_channel(channel: u8) -> f64 {
        let srgb = f64::from(channel) / 255.0;
        if srgb <= 0.04045 {
            srgb / 12.92
        } else {
            ((srgb + 0.055) / 1.055).powf(2.4)
        }
    }

    0.2126 * linear_channel(color.red())
        + 0.7152 * linear_channel(color.green())
        + 0.0722 * linear_channel(color.blue())
}

pub fn contrast_ratio(first: Color, second: Color) -> f64 {
    let first_luminance = srgb_luminance(first);
    let second_luminance = srgb_luminance(second);
    let lighter = first_luminance.max(second_luminance);
    let darker = first_luminance.min(second_luminance);
    (lighter + 0.05) / (darker + 0.05)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TextTokens {
    pub primary: Color,
    pub secondary: Color,
    pub muted: Color,
    pub disabled: Color,
    pub inverse: Color,
    pub on_accent: Color,
    pub on_selection: Color,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SurfaceTokens {
    pub canvas: Color,
    pub raised: Color,
    pub overlay: Color,
    pub sunken: Color,
    pub hover: Color,
    pub selection: Color,
    pub disabled: Color,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BorderTokens {
    pub subtle: Color,
    pub default: Color,
    pub strong: Color,
    pub focus: Color,
    pub selection: Color,
    pub disabled: Color,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ActionStateTokens {
    pub foreground: Color,
    pub background: Color,
    pub border: Color,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InteractionStateTokens {
    pub default: ActionStateTokens,
    pub hover: ActionStateTokens,
    pub focus: ActionStateTokens,
    pub selected: ActionStateTokens,
    pub disabled: ActionStateTokens,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ActionTokens {
    /// The primary action is also the semantic accent. It is the only source
    /// for accent/action colors until the native cockpit cutover.
    pub primary: InteractionStateTokens,
    pub destructive: InteractionStateTokens,
}

impl ActionTokens {
    pub const fn accent(self) -> InteractionStateTokens {
        self.primary
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StatusMeaning {
    External,
    Attention,
    Success,
    Warning,
    Destructive,
    Inactive,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StatusTokens {
    pub external: Color,
    pub attention: Color,
    pub success: Color,
    pub warning: Color,
    pub destructive: Color,
    pub inactive: Color,
    pub external_surface: Color,
    pub external_foreground: Color,
    pub attention_surface: Color,
    pub attention_foreground: Color,
    pub success_surface: Color,
    pub success_foreground: Color,
    pub warning_surface: Color,
    pub warning_foreground: Color,
    pub destructive_surface: Color,
    pub destructive_foreground: Color,
    pub inactive_surface: Color,
    pub inactive_foreground: Color,
}

impl StatusTokens {
    pub const fn color(self, meaning: StatusMeaning) -> Color {
        match meaning {
            StatusMeaning::External => self.external,
            StatusMeaning::Attention => self.attention,
            StatusMeaning::Success => self.success,
            StatusMeaning::Warning => self.warning,
            StatusMeaning::Destructive => self.destructive,
            StatusMeaning::Inactive => self.inactive,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TerminalPalette {
    pub background: Color,
    pub foreground: Color,
    pub cursor: Color,
    pub selection: Color,
    pub black: Color,
    pub red: Color,
    pub green: Color,
    pub yellow: Color,
    pub blue: Color,
    pub magenta: Color,
    pub cyan: Color,
    pub white: Color,
    pub bright_black: Color,
    pub bright_red: Color,
    pub bright_green: Color,
    pub bright_yellow: Color,
    pub bright_blue: Color,
    pub bright_magenta: Color,
    pub bright_cyan: Color,
    pub bright_white: Color,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TerminalSlotRole {
    Background,
    NormalForeground,
    CursorIndicator,
    SelectionBackground,
    AnsiForeground,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TerminalColorSlot {
    pub name: &'static str,
    pub color: Color,
    pub role: TerminalSlotRole,
}

impl TerminalColorSlot {
    pub fn is_foreground_capable(self) -> bool {
        matches!(
            self.role,
            TerminalSlotRole::NormalForeground | TerminalSlotRole::AnsiForeground
        )
    }
}

impl TerminalPalette {
    pub fn slots(self) -> Vec<TerminalColorSlot> {
        vec![
            TerminalColorSlot {
                name: "terminal_background",
                color: self.background,
                role: TerminalSlotRole::Background,
            },
            TerminalColorSlot {
                name: "terminal_foreground",
                color: self.foreground,
                role: TerminalSlotRole::NormalForeground,
            },
            TerminalColorSlot {
                name: "terminal_cursor",
                color: self.cursor,
                role: TerminalSlotRole::CursorIndicator,
            },
            TerminalColorSlot {
                name: "terminal_selection",
                color: self.selection,
                role: TerminalSlotRole::SelectionBackground,
            },
            TerminalColorSlot {
                name: "terminal_black",
                color: self.black,
                role: TerminalSlotRole::AnsiForeground,
            },
            TerminalColorSlot {
                name: "terminal_red",
                color: self.red,
                role: TerminalSlotRole::AnsiForeground,
            },
            TerminalColorSlot {
                name: "terminal_green",
                color: self.green,
                role: TerminalSlotRole::AnsiForeground,
            },
            TerminalColorSlot {
                name: "terminal_yellow",
                color: self.yellow,
                role: TerminalSlotRole::AnsiForeground,
            },
            TerminalColorSlot {
                name: "terminal_blue",
                color: self.blue,
                role: TerminalSlotRole::AnsiForeground,
            },
            TerminalColorSlot {
                name: "terminal_magenta",
                color: self.magenta,
                role: TerminalSlotRole::AnsiForeground,
            },
            TerminalColorSlot {
                name: "terminal_cyan",
                color: self.cyan,
                role: TerminalSlotRole::AnsiForeground,
            },
            TerminalColorSlot {
                name: "terminal_white",
                color: self.white,
                role: TerminalSlotRole::AnsiForeground,
            },
            TerminalColorSlot {
                name: "terminal_bright_black",
                color: self.bright_black,
                role: TerminalSlotRole::AnsiForeground,
            },
            TerminalColorSlot {
                name: "terminal_bright_red",
                color: self.bright_red,
                role: TerminalSlotRole::AnsiForeground,
            },
            TerminalColorSlot {
                name: "terminal_bright_green",
                color: self.bright_green,
                role: TerminalSlotRole::AnsiForeground,
            },
            TerminalColorSlot {
                name: "terminal_bright_yellow",
                color: self.bright_yellow,
                role: TerminalSlotRole::AnsiForeground,
            },
            TerminalColorSlot {
                name: "terminal_bright_blue",
                color: self.bright_blue,
                role: TerminalSlotRole::AnsiForeground,
            },
            TerminalColorSlot {
                name: "terminal_bright_magenta",
                color: self.bright_magenta,
                role: TerminalSlotRole::AnsiForeground,
            },
            TerminalColorSlot {
                name: "terminal_bright_cyan",
                color: self.bright_cyan,
                role: TerminalSlotRole::AnsiForeground,
            },
            TerminalColorSlot {
                name: "terminal_bright_white",
                color: self.bright_white,
                role: TerminalSlotRole::AnsiForeground,
            },
        ]
    }

    pub fn foreground_slots(self) -> Vec<TerminalColorSlot> {
        self.slots()
            .into_iter()
            .filter(|slot| slot.is_foreground_capable())
            .collect()
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SpacingTokens {
    pub xxs: f32,
    pub xs: f32,
    pub sm: f32,
    pub md: f32,
    pub lg: f32,
    pub xl: f32,
    pub xxl: f32,
    pub control_gap: f32,
    pub panel_padding: f32,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RadiiTokens {
    pub none: f32,
    pub sm: f32,
    pub md: f32,
    pub lg: f32,
    pub pill: f32,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TypographyTokens {
    pub caption: f32,
    pub body: f32,
    pub body_emphasis: f32,
    pub title: f32,
    pub heading: f32,
    pub code: f32,
    pub caption_line_height: f32,
    pub body_line_height: f32,
    pub title_line_height: f32,
    pub heading_line_height: f32,
    pub code_line_height: f32,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct IconTokens {
    pub xs: f32,
    pub sm: f32,
    pub md: f32,
    pub lg: f32,
    pub xl: f32,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ControlTokens {
    pub control_height: f32,
    pub input_height: f32,
    pub button_height: f32,
    pub row_height: f32,
    pub control_padding: f32,
    pub row_padding: f32,
    pub icon_gap: f32,
    pub focus_ring_width: f32,
    pub focus_ring_offset: f32,
    pub label_min_width: f32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MotionTokens {
    pub instant_ms: u16,
    pub fast_ms: u16,
    pub normal_ms: u16,
    pub slow_ms: u16,
    pub tooltip_delay_ms: u16,
    pub reduced_motion_ms: u16,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DensityMetrics {
    pub density: Density,
    pub scale: Scale,
    pub spacing: SpacingTokens,
    pub radii: RadiiTokens,
    pub typography: TypographyTokens,
    pub icons: IconTokens,
    pub controls: ControlTokens,
    pub motion: MotionTokens,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PhysicalDensityMetrics {
    pub control_height: u32,
    pub row_height: u32,
    pub icon_size: u32,
    pub control_padding: u32,
    pub row_padding: u32,
    pub terminal_line_height: u32,
    pub code_line_height: u32,
    pub body_line_height: u32,
    pub focus_ring_width: u32,
    pub label_min_width: u32,
}

impl DensityMetrics {
    pub const fn spacing(self) -> SpacingTokens {
        self.spacing
    }

    pub const fn radii(self) -> RadiiTokens {
        self.radii
    }

    pub const fn typography(self) -> TypographyTokens {
        self.typography
    }

    pub const fn icons(self) -> IconTokens {
        self.icons
    }

    pub const fn controls(self) -> ControlTokens {
        self.controls
    }

    pub const fn motion(self) -> MotionTokens {
        self.motion
    }

    pub fn physical(self) -> PhysicalDensityMetrics {
        let scale = self.scale.factor();
        let pixels = |value: f32| (value * scale).ceil().max(1.0) as u32;
        PhysicalDensityMetrics {
            control_height: pixels(self.controls.control_height),
            row_height: pixels(self.controls.row_height),
            icon_size: pixels(self.icons.md),
            control_padding: pixels(self.controls.control_padding),
            row_padding: pixels(self.controls.row_padding),
            terminal_line_height: pixels(self.typography.code_line_height),
            code_line_height: pixels(self.typography.code_line_height),
            body_line_height: pixels(self.typography.body_line_height),
            focus_ring_width: pixels(self.controls.focus_ring_width),
            label_min_width: pixels(self.controls.label_min_width),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SemanticColorToken {
    pub name: &'static str,
    pub color: Color,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ContrastPair {
    pub name: &'static str,
    pub foreground: Color,
    pub background: Color,
}

const fn contrast_pair(name: &'static str, foreground: Color, background: Color) -> ContrastPair {
    ContrastPair {
        name,
        foreground,
        background,
    }
}

fn terminal_foreground_pair_name(name: &'static str) -> &'static str {
    match name {
        "terminal_foreground" => "terminal_foreground_on_background",
        "terminal_black" => "terminal_black_on_background",
        "terminal_red" => "terminal_red_on_background",
        "terminal_green" => "terminal_green_on_background",
        "terminal_yellow" => "terminal_yellow_on_background",
        "terminal_blue" => "terminal_blue_on_background",
        "terminal_magenta" => "terminal_magenta_on_background",
        "terminal_cyan" => "terminal_cyan_on_background",
        "terminal_white" => "terminal_white_on_background",
        "terminal_bright_black" => "terminal_bright_black_on_background",
        "terminal_bright_red" => "terminal_bright_red_on_background",
        "terminal_bright_green" => "terminal_bright_green_on_background",
        "terminal_bright_yellow" => "terminal_bright_yellow_on_background",
        "terminal_bright_blue" => "terminal_bright_blue_on_background",
        "terminal_bright_magenta" => "terminal_bright_magenta_on_background",
        "terminal_bright_cyan" => "terminal_bright_cyan_on_background",
        "terminal_bright_white" => "terminal_bright_white_on_background",
        _ => unreachable!("terminal foreground slot must have a declared pair name"),
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ThemeTokens {
    pub mode: ThemeMode,
    pub text: TextTokens,
    pub surfaces: SurfaceTokens,
    pub borders: BorderTokens,
    pub actions: ActionTokens,
    pub status: StatusTokens,
    pub terminal: TerminalPalette,
    pub density: DensityMetrics,
}

impl ThemeTokens {
    pub fn semantic_color_tokens(self) -> Vec<SemanticColorToken> {
        vec![
            SemanticColorToken {
                name: "text_primary",
                color: self.text.primary,
            },
            SemanticColorToken {
                name: "text_secondary",
                color: self.text.secondary,
            },
            SemanticColorToken {
                name: "text_muted",
                color: self.text.muted,
            },
            SemanticColorToken {
                name: "text_disabled",
                color: self.text.disabled,
            },
            SemanticColorToken {
                name: "text_inverse",
                color: self.text.inverse,
            },
            SemanticColorToken {
                name: "text_on_accent",
                color: self.text.on_accent,
            },
            SemanticColorToken {
                name: "text_on_selection",
                color: self.text.on_selection,
            },
            SemanticColorToken {
                name: "surface_canvas",
                color: self.surfaces.canvas,
            },
            SemanticColorToken {
                name: "surface_raised",
                color: self.surfaces.raised,
            },
            SemanticColorToken {
                name: "surface_overlay",
                color: self.surfaces.overlay,
            },
            SemanticColorToken {
                name: "surface_sunken",
                color: self.surfaces.sunken,
            },
            SemanticColorToken {
                name: "surface_hover",
                color: self.surfaces.hover,
            },
            SemanticColorToken {
                name: "surface_selected",
                color: self.surfaces.selection,
            },
            SemanticColorToken {
                name: "surface_disabled",
                color: self.surfaces.disabled,
            },
            SemanticColorToken {
                name: "border_subtle",
                color: self.borders.subtle,
            },
            SemanticColorToken {
                name: "border_default",
                color: self.borders.default,
            },
            SemanticColorToken {
                name: "border_strong",
                color: self.borders.strong,
            },
            SemanticColorToken {
                name: "border_focus",
                color: self.borders.focus,
            },
            SemanticColorToken {
                name: "border_selection",
                color: self.borders.selection,
            },
            SemanticColorToken {
                name: "border_disabled",
                color: self.borders.disabled,
            },
            SemanticColorToken {
                name: "action_primary_default",
                color: self.actions.primary.default.background,
            },
            SemanticColorToken {
                name: "action_primary_hover",
                color: self.actions.primary.hover.background,
            },
            SemanticColorToken {
                name: "action_primary_focus",
                color: self.actions.primary.focus.background,
            },
            SemanticColorToken {
                name: "action_primary_selected",
                color: self.actions.primary.selected.background,
            },
            SemanticColorToken {
                name: "action_primary_disabled",
                color: self.actions.primary.disabled.background,
            },
            SemanticColorToken {
                name: "action_primary_foreground",
                color: self.actions.primary.default.foreground,
            },
            SemanticColorToken {
                name: "action_destructive_default",
                color: self.actions.destructive.default.background,
            },
            SemanticColorToken {
                name: "action_destructive_hover",
                color: self.actions.destructive.hover.background,
            },
            SemanticColorToken {
                name: "action_destructive_focus",
                color: self.actions.destructive.focus.background,
            },
            SemanticColorToken {
                name: "action_destructive_selected",
                color: self.actions.destructive.selected.background,
            },
            SemanticColorToken {
                name: "action_destructive_disabled",
                color: self.actions.destructive.disabled.background,
            },
            SemanticColorToken {
                name: "action_destructive_foreground",
                color: self.actions.destructive.default.foreground,
            },
            SemanticColorToken {
                name: "status_external",
                color: self.status.external,
            },
            SemanticColorToken {
                name: "status_attention",
                color: self.status.attention,
            },
            SemanticColorToken {
                name: "status_success",
                color: self.status.success,
            },
            SemanticColorToken {
                name: "status_warning",
                color: self.status.warning,
            },
            SemanticColorToken {
                name: "status_destructive",
                color: self.status.destructive,
            },
            SemanticColorToken {
                name: "status_inactive",
                color: self.status.inactive,
            },
            SemanticColorToken {
                name: "status_external_surface",
                color: self.status.external_surface,
            },
            SemanticColorToken {
                name: "status_external_foreground",
                color: self.status.external_foreground,
            },
            SemanticColorToken {
                name: "status_attention_surface",
                color: self.status.attention_surface,
            },
            SemanticColorToken {
                name: "status_attention_foreground",
                color: self.status.attention_foreground,
            },
            SemanticColorToken {
                name: "status_success_surface",
                color: self.status.success_surface,
            },
            SemanticColorToken {
                name: "status_success_foreground",
                color: self.status.success_foreground,
            },
            SemanticColorToken {
                name: "status_warning_surface",
                color: self.status.warning_surface,
            },
            SemanticColorToken {
                name: "status_warning_foreground",
                color: self.status.warning_foreground,
            },
            SemanticColorToken {
                name: "status_destructive_surface",
                color: self.status.destructive_surface,
            },
            SemanticColorToken {
                name: "status_destructive_foreground",
                color: self.status.destructive_foreground,
            },
            SemanticColorToken {
                name: "status_inactive_surface",
                color: self.status.inactive_surface,
            },
            SemanticColorToken {
                name: "status_inactive_foreground",
                color: self.status.inactive_foreground,
            },
            SemanticColorToken {
                name: "terminal_background",
                color: self.terminal.background,
            },
            SemanticColorToken {
                name: "terminal_foreground",
                color: self.terminal.foreground,
            },
            SemanticColorToken {
                name: "terminal_cursor",
                color: self.terminal.cursor,
            },
            SemanticColorToken {
                name: "terminal_selection",
                color: self.terminal.selection,
            },
            SemanticColorToken {
                name: "terminal_black",
                color: self.terminal.black,
            },
            SemanticColorToken {
                name: "terminal_red",
                color: self.terminal.red,
            },
            SemanticColorToken {
                name: "terminal_green",
                color: self.terminal.green,
            },
            SemanticColorToken {
                name: "terminal_yellow",
                color: self.terminal.yellow,
            },
            SemanticColorToken {
                name: "terminal_blue",
                color: self.terminal.blue,
            },
            SemanticColorToken {
                name: "terminal_magenta",
                color: self.terminal.magenta,
            },
            SemanticColorToken {
                name: "terminal_cyan",
                color: self.terminal.cyan,
            },
            SemanticColorToken {
                name: "terminal_white",
                color: self.terminal.white,
            },
            SemanticColorToken {
                name: "terminal_bright_black",
                color: self.terminal.bright_black,
            },
            SemanticColorToken {
                name: "terminal_bright_red",
                color: self.terminal.bright_red,
            },
            SemanticColorToken {
                name: "terminal_bright_green",
                color: self.terminal.bright_green,
            },
            SemanticColorToken {
                name: "terminal_bright_yellow",
                color: self.terminal.bright_yellow,
            },
            SemanticColorToken {
                name: "terminal_bright_blue",
                color: self.terminal.bright_blue,
            },
            SemanticColorToken {
                name: "terminal_bright_magenta",
                color: self.terminal.bright_magenta,
            },
            SemanticColorToken {
                name: "terminal_bright_cyan",
                color: self.terminal.bright_cyan,
            },
            SemanticColorToken {
                name: "terminal_bright_white",
                color: self.terminal.bright_white,
            },
        ]
    }

    pub fn normal_text_contrast_pairs(self) -> Vec<ContrastPair> {
        let surfaces = [
            ("canvas", self.surfaces.canvas),
            ("raised", self.surfaces.raised),
            ("overlay", self.surfaces.overlay),
            ("sunken", self.surfaces.sunken),
            ("hover", self.surfaces.hover),
            ("selection", self.surfaces.selection),
            ("disabled", self.surfaces.disabled),
        ];
        let mut pairs = Vec::with_capacity(28 + 2 + 8 + 17 + 10 + 6);
        for (surface_name, surface) in surfaces {
            let names = [
                ("text_primary", self.text.primary),
                ("text_secondary", self.text.secondary),
                ("text_muted", self.text.muted),
                ("text_disabled", self.text.disabled),
            ];
            for (text_name, foreground) in names {
                let name = match (text_name, surface_name) {
                    ("text_primary", "canvas") => "text_primary_on_canvas",
                    ("text_primary", "raised") => "text_primary_on_raised",
                    ("text_primary", "overlay") => "text_primary_on_overlay",
                    ("text_primary", "sunken") => "text_primary_on_sunken",
                    ("text_primary", "hover") => "text_primary_on_hover",
                    ("text_primary", "selection") => "text_primary_on_selection",
                    ("text_primary", "disabled") => "text_primary_on_disabled_surface",
                    ("text_secondary", "canvas") => "text_secondary_on_canvas",
                    ("text_secondary", "raised") => "text_secondary_on_raised",
                    ("text_secondary", "overlay") => "text_secondary_on_overlay",
                    ("text_secondary", "sunken") => "text_secondary_on_sunken",
                    ("text_secondary", "hover") => "text_secondary_on_hover",
                    ("text_secondary", "selection") => "text_secondary_on_selection",
                    ("text_secondary", "disabled") => "text_secondary_on_disabled_surface",
                    ("text_muted", "canvas") => "text_muted_on_canvas",
                    ("text_muted", "raised") => "text_muted_on_raised",
                    ("text_muted", "overlay") => "text_muted_on_overlay",
                    ("text_muted", "sunken") => "text_muted_on_sunken",
                    ("text_muted", "hover") => "text_muted_on_hover",
                    ("text_muted", "selection") => "text_muted_on_selection",
                    ("text_muted", "disabled") => "text_muted_on_disabled_surface",
                    ("text_disabled", "canvas") => "text_disabled_on_canvas",
                    ("text_disabled", "raised") => "text_disabled_on_raised",
                    ("text_disabled", "overlay") => "text_disabled_on_overlay",
                    ("text_disabled", "sunken") => "text_disabled_on_sunken",
                    ("text_disabled", "hover") => "text_disabled_on_hover",
                    ("text_disabled", "selection") => "text_disabled_on_selection",
                    ("text_disabled", "disabled") => "text_disabled_on_disabled_surface",
                    _ => unreachable!("semantic text/surface pair must be declared"),
                };
                pairs.push(contrast_pair(name, foreground, surface));
            }
        }
        pairs.push(contrast_pair(
            "text_on_selection",
            self.text.on_selection,
            self.surfaces.selection,
        ));
        pairs.push(contrast_pair(
            "text_inverse_on_terminal_background",
            self.text.inverse,
            self.terminal.background,
        ));
        pairs.push(contrast_pair(
            "text_on_accent_on_action_primary_default",
            self.text.on_accent,
            self.actions.primary.default.background,
        ));
        pairs.push(contrast_pair(
            "text_on_accent_on_action_primary_hover",
            self.text.on_accent,
            self.actions.primary.hover.background,
        ));
        pairs.push(contrast_pair(
            "text_on_accent_on_action_primary_focus",
            self.text.on_accent,
            self.actions.primary.focus.background,
        ));
        pairs.push(contrast_pair(
            "text_on_accent_on_action_primary_selected",
            self.text.on_accent,
            self.actions.primary.selected.background,
        ));
        pairs.push(contrast_pair(
            "text_on_accent_on_action_destructive_default",
            self.text.on_accent,
            self.actions.destructive.default.background,
        ));
        pairs.push(contrast_pair(
            "text_on_accent_on_action_destructive_hover",
            self.text.on_accent,
            self.actions.destructive.hover.background,
        ));
        pairs.push(contrast_pair(
            "text_on_accent_on_action_destructive_focus",
            self.text.on_accent,
            self.actions.destructive.focus.background,
        ));
        pairs.push(contrast_pair(
            "text_on_accent_on_action_destructive_selected",
            self.text.on_accent,
            self.actions.destructive.selected.background,
        ));
        pairs.extend(self.terminal_foreground_contrast_pairs());
        pairs.extend(self.interaction_state_contrast_pairs());
        pairs.extend(self.status_surface_contrast_pairs());
        pairs
    }

    pub fn terminal_foreground_contrast_pairs(self) -> Vec<ContrastPair> {
        self.terminal
            .foreground_slots()
            .into_iter()
            .map(|slot| {
                contrast_pair(
                    terminal_foreground_pair_name(slot.name),
                    slot.color,
                    self.terminal.background,
                )
            })
            .collect()
    }

    pub fn large_text_contrast_pairs(self) -> Vec<ContrastPair> {
        vec![ContrastPair {
            name: "text_primary_large_on_canvas",
            foreground: self.text.primary,
            background: self.surfaces.canvas,
        }]
    }

    pub fn ui_indicator_contrast_pairs(self) -> Vec<ContrastPair> {
        let mut pairs = vec![
            contrast_pair(
                "focus_ring_on_canvas",
                self.borders.focus,
                self.surfaces.canvas,
            ),
            contrast_pair(
                "selection_border_on_canvas",
                self.borders.selection,
                self.surfaces.canvas,
            ),
        ];
        pairs.extend(self.action_indicator_contrast_pairs());
        pairs.extend(self.status_indicator_contrast_pairs());
        pairs
    }

    pub fn action_indicator_contrast_pairs(self) -> Vec<ContrastPair> {
        let owner = self.surfaces.canvas;
        let primary = self.actions.primary;
        let destructive = self.actions.destructive;
        vec![
            contrast_pair(
                "action_primary_default_background_on_canvas",
                primary.default.background,
                owner,
            ),
            contrast_pair(
                "action_primary_default_border_on_canvas",
                primary.default.border,
                owner,
            ),
            contrast_pair(
                "action_primary_hover_background_on_canvas",
                primary.hover.background,
                owner,
            ),
            contrast_pair(
                "action_primary_hover_border_on_canvas",
                primary.hover.border,
                owner,
            ),
            contrast_pair(
                "action_primary_focus_background_on_canvas",
                primary.focus.background,
                owner,
            ),
            contrast_pair(
                "action_primary_focus_border_on_canvas",
                primary.focus.border,
                owner,
            ),
            contrast_pair(
                "action_primary_selected_background_on_canvas",
                primary.selected.background,
                owner,
            ),
            contrast_pair(
                "action_primary_selected_border_on_canvas",
                primary.selected.border,
                owner,
            ),
            contrast_pair(
                "action_primary_disabled_background_on_canvas",
                primary.disabled.background,
                owner,
            ),
            contrast_pair(
                "action_primary_disabled_border_on_canvas",
                primary.disabled.border,
                owner,
            ),
            contrast_pair(
                "action_destructive_default_background_on_canvas",
                destructive.default.background,
                owner,
            ),
            contrast_pair(
                "action_destructive_default_border_on_canvas",
                destructive.default.border,
                owner,
            ),
            contrast_pair(
                "action_destructive_hover_background_on_canvas",
                destructive.hover.background,
                owner,
            ),
            contrast_pair(
                "action_destructive_hover_border_on_canvas",
                destructive.hover.border,
                owner,
            ),
            contrast_pair(
                "action_destructive_focus_background_on_canvas",
                destructive.focus.background,
                owner,
            ),
            contrast_pair(
                "action_destructive_focus_border_on_canvas",
                destructive.focus.border,
                owner,
            ),
            contrast_pair(
                "action_destructive_selected_background_on_canvas",
                destructive.selected.background,
                owner,
            ),
            contrast_pair(
                "action_destructive_selected_border_on_canvas",
                destructive.selected.border,
                owner,
            ),
            contrast_pair(
                "action_destructive_disabled_background_on_canvas",
                destructive.disabled.background,
                owner,
            ),
            contrast_pair(
                "action_destructive_disabled_border_on_canvas",
                destructive.disabled.border,
                owner,
            ),
        ]
    }

    pub fn status_indicator_contrast_pairs(self) -> Vec<ContrastPair> {
        vec![
            contrast_pair(
                "status_external_indicator_on_surface",
                self.status.external,
                self.status.external_surface,
            ),
            contrast_pair(
                "status_attention_indicator_on_surface",
                self.status.attention,
                self.status.attention_surface,
            ),
            contrast_pair(
                "status_success_indicator_on_surface",
                self.status.success,
                self.status.success_surface,
            ),
            contrast_pair(
                "status_warning_indicator_on_surface",
                self.status.warning,
                self.status.warning_surface,
            ),
            contrast_pair(
                "status_destructive_indicator_on_surface",
                self.status.destructive,
                self.status.destructive_surface,
            ),
            contrast_pair(
                "status_inactive_indicator_on_surface",
                self.status.inactive,
                self.status.inactive_surface,
            ),
        ]
    }

    pub fn interaction_state_contrast_pairs(self) -> Vec<ContrastPair> {
        let primary = self.actions.primary;
        let destructive = self.actions.destructive;
        vec![
            contrast_pair(
                "action_primary_default_on_surface",
                primary.default.foreground,
                primary.default.background,
            ),
            contrast_pair(
                "action_primary_hover_on_surface",
                primary.hover.foreground,
                primary.hover.background,
            ),
            contrast_pair(
                "action_primary_focus_on_surface",
                primary.focus.foreground,
                primary.focus.background,
            ),
            contrast_pair(
                "action_primary_selected_on_surface",
                primary.selected.foreground,
                primary.selected.background,
            ),
            contrast_pair(
                "action_primary_disabled_on_surface",
                primary.disabled.foreground,
                primary.disabled.background,
            ),
            contrast_pair(
                "action_destructive_default_on_surface",
                destructive.default.foreground,
                destructive.default.background,
            ),
            contrast_pair(
                "action_destructive_hover_on_surface",
                destructive.hover.foreground,
                destructive.hover.background,
            ),
            contrast_pair(
                "action_destructive_focus_on_surface",
                destructive.focus.foreground,
                destructive.focus.background,
            ),
            contrast_pair(
                "action_destructive_selected_on_surface",
                destructive.selected.foreground,
                destructive.selected.background,
            ),
            contrast_pair(
                "action_destructive_disabled_on_surface",
                destructive.disabled.foreground,
                destructive.disabled.background,
            ),
        ]
    }

    pub fn status_surface_contrast_pairs(self) -> Vec<ContrastPair> {
        vec![
            contrast_pair(
                "status_external_surface",
                self.status.external_foreground,
                self.status.external_surface,
            ),
            contrast_pair(
                "status_attention_surface",
                self.status.attention_foreground,
                self.status.attention_surface,
            ),
            contrast_pair(
                "status_success_surface",
                self.status.success_foreground,
                self.status.success_surface,
            ),
            contrast_pair(
                "status_warning_surface",
                self.status.warning_foreground,
                self.status.warning_surface,
            ),
            contrast_pair(
                "status_destructive_surface",
                self.status.destructive_foreground,
                self.status.destructive_surface,
            ),
            contrast_pair(
                "status_inactive_surface",
                self.status.inactive_foreground,
                self.status.inactive_surface,
            ),
        ]
    }

    pub fn disabled_text_contrast_pairs(self) -> Vec<ContrastPair> {
        let surfaces = [
            ("raised", self.surfaces.raised),
            ("canvas", self.surfaces.canvas),
            ("overlay", self.surfaces.overlay),
            ("sunken", self.surfaces.sunken),
            ("hover", self.surfaces.hover),
            ("selection", self.surfaces.selection),
            ("disabled", self.surfaces.disabled),
        ];
        surfaces
            .into_iter()
            .map(|(surface, background)| {
                let name = match surface {
                    "raised" => "text_disabled_on_raised",
                    "canvas" => "text_disabled_on_canvas",
                    "overlay" => "text_disabled_on_overlay",
                    "sunken" => "text_disabled_on_sunken",
                    "hover" => "text_disabled_on_hover",
                    "selection" => "text_disabled_on_selection",
                    "disabled" => "text_disabled_on_disabled_surface",
                    _ => unreachable!("disabled text surface must be declared"),
                };
                contrast_pair(name, self.text.disabled, background)
            })
            .collect()
    }
}

const DARK_SURFACE_CANVAS: Color = Color::from_u32(0x18181b);
const DARK_SURFACE_RAISED: Color = Color::from_u32(0x27272a);
const DARK_SURFACE_OVERLAY: Color = Color::from_u32(0x323238);
const DARK_SURFACE_SUNKEN: Color = Color::from_u32(0x09090b);
const DARK_SURFACE_HOVER: Color = Color::from_u32(0x323238);
const DARK_SURFACE_SELECTION: Color = Color::from_u32(0x3f3f46);
const DARK_SURFACE_DISABLED: Color = Color::from_u32(0x27272a);

const DARK_TEXT_PRIMARY: Color = Color::from_u32(0xe4e4e7);
const DARK_TEXT_SECONDARY: Color = Color::from_u32(0xd4d4d8);
const DARK_TEXT_MUTED: Color = Color::from_u32(0xc4c4cc);
const DARK_TEXT_DISABLED: Color = Color::from_u32(0xb8b8c0);
const DARK_TEXT_INVERSE: Color = Color::from_u32(0xf8fafc);
const DARK_TEXT_ON_ACCENT: Color = Color::from_u32(0xf8fafc);
const DARK_TEXT_ON_SELECTION: Color = Color::from_u32(0xf8fafc);

const DARK_BORDER_SUBTLE: Color = Color::from_u32(0x27272a);
const DARK_BORDER_DEFAULT: Color = Color::from_u32(0x3f3f46);
const DARK_BORDER_STRONG: Color = Color::from_u32(0x52525b);
const DARK_BORDER_FOCUS: Color = Color::from_u32(0xfacc15);
const DARK_BORDER_SELECTION: Color = Color::from_u32(0xa1a1aa);
const DARK_BORDER_DISABLED: Color = Color::from_u32(0x52525b);

const DARK_STATUS_EXTERNAL: Color = Color::from_u32(0x60a5fa);
const DARK_STATUS_ATTENTION: Color = Color::from_u32(0xf59e0b);
const DARK_STATUS_SUCCESS: Color = Color::from_u32(0x4ade80);
const DARK_STATUS_WARNING: Color = Color::from_u32(0xfacc15);
const DARK_STATUS_DESTRUCTIVE: Color = Color::from_u32(0xfb7185);
const DARK_STATUS_INACTIVE: Color = Color::from_u32(0xa1a1aa);

const DARK_ACTION_PRIMARY_DEFAULT: Color = Color::from_u32(0x5757c8);
const DARK_ACTION_PRIMARY_HOVER: Color = Color::from_u32(0x5959d0);
const DARK_ACTION_PRIMARY_FOCUS: Color = Color::from_u32(0x5b5bd6);
const DARK_ACTION_PRIMARY_SELECTED: Color = Color::from_u32(0x5c5bd6);
const DARK_ACTION_PRIMARY_DISABLED: Color = Color::from_u32(0x606876);
const DARK_ACTION_PRIMARY_FOREGROUND: Color = Color::from_u32(0xf8fafc);
const DARK_ACTION_DESTRUCTIVE_DEFAULT: Color = Color::from_u32(0xc62828);
const DARK_ACTION_DESTRUCTIVE_HOVER: Color = Color::from_u32(0xc92a2a);
const DARK_ACTION_DESTRUCTIVE_FOCUS: Color = Color::from_u32(0xc62828);
const DARK_ACTION_DESTRUCTIVE_SELECTED: Color = Color::from_u32(0xc92a2a);
const DARK_ACTION_DESTRUCTIVE_DISABLED: Color = Color::from_u32(0x606876);
const DARK_ACTION_DESTRUCTIVE_FOREGROUND: Color = Color::from_u32(0xffffff);

const DARK_STATUS_EXTERNAL_SURFACE: Color = Color::from_u32(0x172554);
const DARK_STATUS_EXTERNAL_FOREGROUND: Color = Color::from_u32(0xdbeafe);
const DARK_STATUS_ATTENTION_SURFACE: Color = Color::from_u32(0x451a03);
const DARK_STATUS_ATTENTION_FOREGROUND: Color = Color::from_u32(0xffedd5);
const DARK_STATUS_SUCCESS_SURFACE: Color = Color::from_u32(0x052e16);
const DARK_STATUS_SUCCESS_FOREGROUND: Color = Color::from_u32(0xdcfce7);
const DARK_STATUS_WARNING_SURFACE: Color = Color::from_u32(0x422006);
const DARK_STATUS_WARNING_FOREGROUND: Color = Color::from_u32(0xfef3c7);
const DARK_STATUS_DESTRUCTIVE_SURFACE: Color = Color::from_u32(0x450a0a);
const DARK_STATUS_DESTRUCTIVE_FOREGROUND: Color = Color::from_u32(0xffe4e6);
const DARK_STATUS_INACTIVE_SURFACE: Color = Color::from_u32(0x27272a);
const DARK_STATUS_INACTIVE_FOREGROUND: Color = Color::from_u32(0xd4d4d8);

const DARK_TERMINAL_BLACK: Color = Color::from_u32(0x818894);
const DARK_TERMINAL_RED: Color = Color::from_u32(0xef4444);
const DARK_TERMINAL_GREEN: Color = Color::from_u32(0x22c55e);
const DARK_TERMINAL_YELLOW: Color = Color::from_u32(0xeab308);
const DARK_TERMINAL_BLUE: Color = Color::from_u32(0x3b82f6);
const DARK_TERMINAL_MAGENTA: Color = Color::from_u32(0xa855f7);
const DARK_TERMINAL_CYAN: Color = Color::from_u32(0x06b6d4);
const DARK_TERMINAL_WHITE: Color = Color::from_u32(0xe4e4e7);
const DARK_TERMINAL_BRIGHT_BLACK: Color = Color::from_u32(0xa1a1aa);
const DARK_TERMINAL_BRIGHT_RED: Color = Color::from_u32(0xf87171);
const DARK_TERMINAL_BRIGHT_GREEN: Color = Color::from_u32(0x4ade80);
const DARK_TERMINAL_BRIGHT_YELLOW: Color = Color::from_u32(0xfacc15);
const DARK_TERMINAL_BRIGHT_BLUE: Color = Color::from_u32(0x60a5fa);
const DARK_TERMINAL_BRIGHT_MAGENTA: Color = Color::from_u32(0xc084fc);
const DARK_TERMINAL_BRIGHT_CYAN: Color = Color::from_u32(0x22d3ee);
const DARK_TERMINAL_BRIGHT_WHITE: Color = Color::from_u32(0xfafafa);

const LIGHT_SURFACE_CANVAS: Color = Color::from_u32(0xf8fafc);
const LIGHT_SURFACE_RAISED: Color = Color::from_u32(0xf1f5f9);
const LIGHT_SURFACE_OVERLAY: Color = Color::from_u32(0xffffff);
const LIGHT_SURFACE_SUNKEN: Color = Color::from_u32(0xe2e8f0);
const LIGHT_SURFACE_HOVER: Color = Color::from_u32(0xe2e8f0);
const LIGHT_SURFACE_SELECTION: Color = Color::from_u32(0xcbd5e1);
const LIGHT_SURFACE_DISABLED: Color = Color::from_u32(0xf1f5f9);

const LIGHT_TEXT_PRIMARY: Color = Color::from_u32(0x0f172a);
const LIGHT_TEXT_SECONDARY: Color = Color::from_u32(0x1e293b);
const LIGHT_TEXT_MUTED: Color = Color::from_u32(0x334155);
const LIGHT_TEXT_DISABLED: Color = Color::from_u32(0x475569);
const LIGHT_TEXT_INVERSE: Color = Color::from_u32(0xffffff);
const LIGHT_TEXT_ON_ACCENT: Color = Color::from_u32(0xffffff);
const LIGHT_TEXT_ON_SELECTION: Color = Color::from_u32(0x0f172a);

const LIGHT_BORDER_SUBTLE: Color = Color::from_u32(0xe2e8f0);
const LIGHT_BORDER_DEFAULT: Color = Color::from_u32(0xcbd5e1);
const LIGHT_BORDER_STRONG: Color = Color::from_u32(0x94a3b8);
const LIGHT_BORDER_FOCUS: Color = Color::from_u32(0x334155);
const LIGHT_BORDER_SELECTION: Color = Color::from_u32(0x475569);
const LIGHT_BORDER_DISABLED: Color = Color::from_u32(0xcbd5e1);

const LIGHT_STATUS_EXTERNAL: Color = Color::from_u32(0x075eaf);
const LIGHT_STATUS_ATTENTION: Color = Color::from_u32(0x9a4f00);
const LIGHT_STATUS_SUCCESS: Color = Color::from_u32(0x087a42);
const LIGHT_STATUS_WARNING: Color = Color::from_u32(0x8f4b00);
const LIGHT_STATUS_DESTRUCTIVE: Color = Color::from_u32(0xb42318);
const LIGHT_STATUS_INACTIVE: Color = Color::from_u32(0x64748b);

const LIGHT_ACTION_PRIMARY_DEFAULT: Color = Color::from_u32(0x4f46e5);
const LIGHT_ACTION_PRIMARY_HOVER: Color = Color::from_u32(0x4338ca);
const LIGHT_ACTION_PRIMARY_FOCUS: Color = Color::from_u32(0x3730a3);
const LIGHT_ACTION_PRIMARY_SELECTED: Color = Color::from_u32(0x312e81);
const LIGHT_ACTION_PRIMARY_DISABLED: Color = Color::from_u32(0x475569);
const LIGHT_ACTION_PRIMARY_FOREGROUND: Color = Color::from_u32(0xffffff);
const LIGHT_ACTION_DESTRUCTIVE_DEFAULT: Color = Color::from_u32(0xb42318);
const LIGHT_ACTION_DESTRUCTIVE_HOVER: Color = Color::from_u32(0x991b1b);
const LIGHT_ACTION_DESTRUCTIVE_FOCUS: Color = Color::from_u32(0x7f1d1d);
const LIGHT_ACTION_DESTRUCTIVE_SELECTED: Color = Color::from_u32(0x7f1d1d);
const LIGHT_ACTION_DESTRUCTIVE_DISABLED: Color = Color::from_u32(0x475569);
const LIGHT_ACTION_DESTRUCTIVE_FOREGROUND: Color = Color::from_u32(0xffffff);

const LIGHT_STATUS_EXTERNAL_SURFACE: Color = Color::from_u32(0xeff6ff);
const LIGHT_STATUS_EXTERNAL_FOREGROUND: Color = Color::from_u32(0x1e3a8a);
const LIGHT_STATUS_ATTENTION_SURFACE: Color = Color::from_u32(0xfff7ed);
const LIGHT_STATUS_ATTENTION_FOREGROUND: Color = Color::from_u32(0x7c2d12);
const LIGHT_STATUS_SUCCESS_SURFACE: Color = Color::from_u32(0xf0fdf4);
const LIGHT_STATUS_SUCCESS_FOREGROUND: Color = Color::from_u32(0x14532d);
const LIGHT_STATUS_WARNING_SURFACE: Color = Color::from_u32(0xfffbeb);
const LIGHT_STATUS_WARNING_FOREGROUND: Color = Color::from_u32(0x78350f);
const LIGHT_STATUS_DESTRUCTIVE_SURFACE: Color = Color::from_u32(0xfff1f2);
const LIGHT_STATUS_DESTRUCTIVE_FOREGROUND: Color = Color::from_u32(0x881337);
const LIGHT_STATUS_INACTIVE_SURFACE: Color = Color::from_u32(0xf1f5f9);
const LIGHT_STATUS_INACTIVE_FOREGROUND: Color = Color::from_u32(0x334155);

const LIGHT_TERMINAL_BACKGROUND: Color = Color::from_u32(0x1e293b);
const LIGHT_TERMINAL_FOREGROUND: Color = Color::from_u32(0xf8fafc);
const LIGHT_TERMINAL_SELECTION: Color = Color::from_u32(0x475569);
const LIGHT_TERMINAL_BLACK: Color = Color::from_u32(0x94a3b8);
const LIGHT_TERMINAL_RED: Color = Color::from_u32(0xff7b72);
const LIGHT_TERMINAL_GREEN: Color = Color::from_u32(0x7ee787);
const LIGHT_TERMINAL_YELLOW: Color = Color::from_u32(0xf0c674);
const LIGHT_TERMINAL_BLUE: Color = Color::from_u32(0x8ab4f8);
const LIGHT_TERMINAL_MAGENTA: Color = Color::from_u32(0xd8a0ff);
const LIGHT_TERMINAL_CYAN: Color = Color::from_u32(0x67e8f9);
const LIGHT_TERMINAL_WHITE: Color = Color::from_u32(0xf8fafc);
const LIGHT_TERMINAL_BRIGHT_BLACK: Color = Color::from_u32(0xe2e8f0);
const LIGHT_TERMINAL_BRIGHT_RED: Color = Color::from_u32(0xffa198);
const LIGHT_TERMINAL_BRIGHT_GREEN: Color = Color::from_u32(0xa7f3d0);
const LIGHT_TERMINAL_BRIGHT_YELLOW: Color = Color::from_u32(0xfde68a);
const LIGHT_TERMINAL_BRIGHT_BLUE: Color = Color::from_u32(0xbfdbfe);
const LIGHT_TERMINAL_BRIGHT_MAGENTA: Color = Color::from_u32(0xe9d5ff);
const LIGHT_TERMINAL_BRIGHT_CYAN: Color = Color::from_u32(0xa5f3fc);
const LIGHT_TERMINAL_BRIGHT_WHITE: Color = Color::from_u32(0xffffff);

fn density_metrics(density: Density, scale: Scale) -> DensityMetrics {
    let compact = density == Density::Compact;
    let spacing = if compact {
        SpacingTokens {
            xxs: 2.0,
            xs: 4.0,
            sm: 6.0,
            md: 8.0,
            lg: 12.0,
            xl: 16.0,
            xxl: 24.0,
            control_gap: 6.0,
            panel_padding: 12.0,
        }
    } else {
        SpacingTokens {
            xxs: 3.0,
            xs: 5.0,
            sm: 8.0,
            md: 10.0,
            lg: 16.0,
            xl: 20.0,
            xxl: 28.0,
            control_gap: 8.0,
            panel_padding: 16.0,
        }
    };
    let radii = if compact {
        RadiiTokens {
            none: 0.0,
            sm: 3.0,
            md: 5.0,
            lg: 7.0,
            pill: 999.0,
        }
    } else {
        RadiiTokens {
            none: 0.0,
            sm: 4.0,
            md: 6.0,
            lg: 8.0,
            pill: 999.0,
        }
    };
    let typography = if compact {
        TypographyTokens {
            caption: 11.0,
            body: 13.0,
            body_emphasis: 13.0,
            title: 18.0,
            heading: 15.0,
            code: 12.0,
            caption_line_height: 15.0,
            body_line_height: 18.0,
            title_line_height: 24.0,
            heading_line_height: 20.0,
            code_line_height: 18.0,
        }
    } else {
        TypographyTokens {
            caption: 12.0,
            body: 14.0,
            body_emphasis: 14.0,
            title: 20.0,
            heading: 16.0,
            code: 13.0,
            caption_line_height: 16.0,
            body_line_height: 20.0,
            title_line_height: 26.0,
            heading_line_height: 22.0,
            code_line_height: 20.0,
        }
    };
    let icons = if compact {
        IconTokens {
            xs: 12.0,
            sm: 14.0,
            md: 16.0,
            lg: 18.0,
            xl: 22.0,
        }
    } else {
        IconTokens {
            xs: 13.0,
            sm: 16.0,
            md: 18.0,
            lg: 20.0,
            xl: 24.0,
        }
    };
    let controls = if compact {
        ControlTokens {
            control_height: 30.0,
            input_height: 32.0,
            button_height: 30.0,
            row_height: 32.0,
            control_padding: 6.0,
            row_padding: 4.0,
            icon_gap: 6.0,
            focus_ring_width: 2.0,
            focus_ring_offset: 1.0,
            label_min_width: 48.0,
        }
    } else {
        ControlTokens {
            control_height: 34.0,
            input_height: 36.0,
            button_height: 34.0,
            row_height: 40.0,
            control_padding: 8.0,
            row_padding: 6.0,
            icon_gap: 8.0,
            focus_ring_width: 2.0,
            focus_ring_offset: 2.0,
            label_min_width: 52.0,
        }
    };
    let motion = MotionTokens {
        instant_ms: 0,
        fast_ms: if compact { 80 } else { 100 },
        normal_ms: if compact { 140 } else { 160 },
        slow_ms: if compact { 220 } else { 240 },
        tooltip_delay_ms: 500,
        reduced_motion_ms: 0,
    };

    DensityMetrics {
        density,
        scale,
        spacing,
        radii,
        typography,
        icons,
        controls,
        motion,
    }
}

fn dark_theme(density: Density, scale: Scale) -> ThemeTokens {
    ThemeTokens {
        mode: ThemeMode::Dark,
        text: TextTokens {
            primary: DARK_TEXT_PRIMARY,
            secondary: DARK_TEXT_SECONDARY,
            muted: DARK_TEXT_MUTED,
            disabled: DARK_TEXT_DISABLED,
            inverse: DARK_TEXT_INVERSE,
            on_accent: DARK_TEXT_ON_ACCENT,
            on_selection: DARK_TEXT_ON_SELECTION,
        },
        surfaces: SurfaceTokens {
            canvas: DARK_SURFACE_CANVAS,
            raised: DARK_SURFACE_RAISED,
            overlay: DARK_SURFACE_OVERLAY,
            sunken: DARK_SURFACE_SUNKEN,
            hover: DARK_SURFACE_HOVER,
            selection: DARK_SURFACE_SELECTION,
            disabled: DARK_SURFACE_DISABLED,
        },
        borders: BorderTokens {
            subtle: DARK_BORDER_SUBTLE,
            default: DARK_BORDER_DEFAULT,
            strong: DARK_BORDER_STRONG,
            focus: DARK_BORDER_FOCUS,
            selection: DARK_BORDER_SELECTION,
            disabled: DARK_BORDER_DISABLED,
        },
        actions: ActionTokens {
            primary: InteractionStateTokens {
                default: ActionStateTokens {
                    foreground: DARK_ACTION_PRIMARY_FOREGROUND,
                    background: DARK_ACTION_PRIMARY_DEFAULT,
                    border: DARK_ACTION_PRIMARY_DEFAULT,
                },
                hover: ActionStateTokens {
                    foreground: DARK_ACTION_PRIMARY_FOREGROUND,
                    background: DARK_ACTION_PRIMARY_HOVER,
                    border: DARK_ACTION_PRIMARY_HOVER,
                },
                focus: ActionStateTokens {
                    foreground: DARK_ACTION_PRIMARY_FOREGROUND,
                    background: DARK_ACTION_PRIMARY_FOCUS,
                    border: DARK_BORDER_FOCUS,
                },
                selected: ActionStateTokens {
                    foreground: DARK_ACTION_PRIMARY_FOREGROUND,
                    background: DARK_ACTION_PRIMARY_SELECTED,
                    border: DARK_BORDER_SELECTION,
                },
                disabled: ActionStateTokens {
                    foreground: DARK_ACTION_PRIMARY_FOREGROUND,
                    background: DARK_ACTION_PRIMARY_DISABLED,
                    border: DARK_BORDER_SELECTION,
                },
            },
            destructive: InteractionStateTokens {
                default: ActionStateTokens {
                    foreground: DARK_ACTION_DESTRUCTIVE_FOREGROUND,
                    background: DARK_ACTION_DESTRUCTIVE_DEFAULT,
                    border: DARK_ACTION_DESTRUCTIVE_DEFAULT,
                },
                hover: ActionStateTokens {
                    foreground: DARK_ACTION_DESTRUCTIVE_FOREGROUND,
                    background: DARK_ACTION_DESTRUCTIVE_HOVER,
                    border: DARK_ACTION_DESTRUCTIVE_HOVER,
                },
                focus: ActionStateTokens {
                    foreground: DARK_ACTION_DESTRUCTIVE_FOREGROUND,
                    background: DARK_ACTION_DESTRUCTIVE_FOCUS,
                    border: DARK_BORDER_FOCUS,
                },
                selected: ActionStateTokens {
                    foreground: DARK_ACTION_DESTRUCTIVE_FOREGROUND,
                    background: DARK_ACTION_DESTRUCTIVE_SELECTED,
                    border: DARK_BORDER_SELECTION,
                },
                disabled: ActionStateTokens {
                    foreground: DARK_ACTION_DESTRUCTIVE_FOREGROUND,
                    background: DARK_ACTION_DESTRUCTIVE_DISABLED,
                    border: DARK_BORDER_SELECTION,
                },
            },
        },
        status: StatusTokens {
            external: DARK_STATUS_EXTERNAL,
            attention: DARK_STATUS_ATTENTION,
            success: DARK_STATUS_SUCCESS,
            warning: DARK_STATUS_WARNING,
            destructive: DARK_STATUS_DESTRUCTIVE,
            inactive: DARK_STATUS_INACTIVE,
            external_surface: DARK_STATUS_EXTERNAL_SURFACE,
            external_foreground: DARK_STATUS_EXTERNAL_FOREGROUND,
            attention_surface: DARK_STATUS_ATTENTION_SURFACE,
            attention_foreground: DARK_STATUS_ATTENTION_FOREGROUND,
            success_surface: DARK_STATUS_SUCCESS_SURFACE,
            success_foreground: DARK_STATUS_SUCCESS_FOREGROUND,
            warning_surface: DARK_STATUS_WARNING_SURFACE,
            warning_foreground: DARK_STATUS_WARNING_FOREGROUND,
            destructive_surface: DARK_STATUS_DESTRUCTIVE_SURFACE,
            destructive_foreground: DARK_STATUS_DESTRUCTIVE_FOREGROUND,
            inactive_surface: DARK_STATUS_INACTIVE_SURFACE,
            inactive_foreground: DARK_STATUS_INACTIVE_FOREGROUND,
        },
        terminal: TerminalPalette {
            background: DARK_SURFACE_SUNKEN,
            foreground: DARK_TEXT_PRIMARY,
            cursor: DARK_TEXT_PRIMARY,
            selection: DARK_SURFACE_SELECTION,
            black: DARK_TERMINAL_BLACK,
            red: DARK_TERMINAL_RED,
            green: DARK_TERMINAL_GREEN,
            yellow: DARK_TERMINAL_YELLOW,
            blue: DARK_TERMINAL_BLUE,
            magenta: DARK_TERMINAL_MAGENTA,
            cyan: DARK_TERMINAL_CYAN,
            white: DARK_TERMINAL_WHITE,
            bright_black: DARK_TERMINAL_BRIGHT_BLACK,
            bright_red: DARK_TERMINAL_BRIGHT_RED,
            bright_green: DARK_TERMINAL_BRIGHT_GREEN,
            bright_yellow: DARK_TERMINAL_BRIGHT_YELLOW,
            bright_blue: DARK_TERMINAL_BRIGHT_BLUE,
            bright_magenta: DARK_TERMINAL_BRIGHT_MAGENTA,
            bright_cyan: DARK_TERMINAL_BRIGHT_CYAN,
            bright_white: DARK_TERMINAL_BRIGHT_WHITE,
        },
        density: density_metrics(density, scale),
    }
}

fn light_theme(density: Density, scale: Scale) -> ThemeTokens {
    ThemeTokens {
        mode: ThemeMode::Light,
        text: TextTokens {
            primary: LIGHT_TEXT_PRIMARY,
            secondary: LIGHT_TEXT_SECONDARY,
            muted: LIGHT_TEXT_MUTED,
            disabled: LIGHT_TEXT_DISABLED,
            inverse: LIGHT_TEXT_INVERSE,
            on_accent: LIGHT_TEXT_ON_ACCENT,
            on_selection: LIGHT_TEXT_ON_SELECTION,
        },
        surfaces: SurfaceTokens {
            canvas: LIGHT_SURFACE_CANVAS,
            raised: LIGHT_SURFACE_RAISED,
            overlay: LIGHT_SURFACE_OVERLAY,
            sunken: LIGHT_SURFACE_SUNKEN,
            hover: LIGHT_SURFACE_HOVER,
            selection: LIGHT_SURFACE_SELECTION,
            disabled: LIGHT_SURFACE_DISABLED,
        },
        borders: BorderTokens {
            subtle: LIGHT_BORDER_SUBTLE,
            default: LIGHT_BORDER_DEFAULT,
            strong: LIGHT_BORDER_STRONG,
            focus: LIGHT_BORDER_FOCUS,
            selection: LIGHT_BORDER_SELECTION,
            disabled: LIGHT_BORDER_DISABLED,
        },
        actions: ActionTokens {
            primary: InteractionStateTokens {
                default: ActionStateTokens {
                    foreground: LIGHT_ACTION_PRIMARY_FOREGROUND,
                    background: LIGHT_ACTION_PRIMARY_DEFAULT,
                    border: LIGHT_ACTION_PRIMARY_DEFAULT,
                },
                hover: ActionStateTokens {
                    foreground: LIGHT_ACTION_PRIMARY_FOREGROUND,
                    background: LIGHT_ACTION_PRIMARY_HOVER,
                    border: LIGHT_ACTION_PRIMARY_HOVER,
                },
                focus: ActionStateTokens {
                    foreground: LIGHT_ACTION_PRIMARY_FOREGROUND,
                    background: LIGHT_ACTION_PRIMARY_FOCUS,
                    border: LIGHT_BORDER_FOCUS,
                },
                selected: ActionStateTokens {
                    foreground: LIGHT_ACTION_PRIMARY_FOREGROUND,
                    background: LIGHT_ACTION_PRIMARY_SELECTED,
                    border: LIGHT_BORDER_SELECTION,
                },
                disabled: ActionStateTokens {
                    foreground: LIGHT_TEXT_INVERSE,
                    background: LIGHT_ACTION_PRIMARY_DISABLED,
                    border: LIGHT_BORDER_FOCUS,
                },
            },
            destructive: InteractionStateTokens {
                default: ActionStateTokens {
                    foreground: LIGHT_ACTION_DESTRUCTIVE_FOREGROUND,
                    background: LIGHT_ACTION_DESTRUCTIVE_DEFAULT,
                    border: LIGHT_ACTION_DESTRUCTIVE_DEFAULT,
                },
                hover: ActionStateTokens {
                    foreground: LIGHT_ACTION_DESTRUCTIVE_FOREGROUND,
                    background: LIGHT_ACTION_DESTRUCTIVE_HOVER,
                    border: LIGHT_ACTION_DESTRUCTIVE_HOVER,
                },
                focus: ActionStateTokens {
                    foreground: LIGHT_ACTION_DESTRUCTIVE_FOREGROUND,
                    background: LIGHT_ACTION_DESTRUCTIVE_FOCUS,
                    border: LIGHT_BORDER_FOCUS,
                },
                selected: ActionStateTokens {
                    foreground: LIGHT_ACTION_DESTRUCTIVE_FOREGROUND,
                    background: LIGHT_ACTION_DESTRUCTIVE_SELECTED,
                    border: LIGHT_BORDER_SELECTION,
                },
                disabled: ActionStateTokens {
                    foreground: LIGHT_TEXT_INVERSE,
                    background: LIGHT_ACTION_DESTRUCTIVE_DISABLED,
                    border: LIGHT_BORDER_FOCUS,
                },
            },
        },
        status: StatusTokens {
            external: LIGHT_STATUS_EXTERNAL,
            attention: LIGHT_STATUS_ATTENTION,
            success: LIGHT_STATUS_SUCCESS,
            warning: LIGHT_STATUS_WARNING,
            destructive: LIGHT_STATUS_DESTRUCTIVE,
            inactive: LIGHT_STATUS_INACTIVE,
            external_surface: LIGHT_STATUS_EXTERNAL_SURFACE,
            external_foreground: LIGHT_STATUS_EXTERNAL_FOREGROUND,
            attention_surface: LIGHT_STATUS_ATTENTION_SURFACE,
            attention_foreground: LIGHT_STATUS_ATTENTION_FOREGROUND,
            success_surface: LIGHT_STATUS_SUCCESS_SURFACE,
            success_foreground: LIGHT_STATUS_SUCCESS_FOREGROUND,
            warning_surface: LIGHT_STATUS_WARNING_SURFACE,
            warning_foreground: LIGHT_STATUS_WARNING_FOREGROUND,
            destructive_surface: LIGHT_STATUS_DESTRUCTIVE_SURFACE,
            destructive_foreground: LIGHT_STATUS_DESTRUCTIVE_FOREGROUND,
            inactive_surface: LIGHT_STATUS_INACTIVE_SURFACE,
            inactive_foreground: LIGHT_STATUS_INACTIVE_FOREGROUND,
        },
        terminal: TerminalPalette {
            background: LIGHT_TERMINAL_BACKGROUND,
            foreground: LIGHT_TERMINAL_FOREGROUND,
            cursor: LIGHT_TERMINAL_FOREGROUND,
            selection: LIGHT_TERMINAL_SELECTION,
            black: LIGHT_TERMINAL_BLACK,
            red: LIGHT_TERMINAL_RED,
            green: LIGHT_TERMINAL_GREEN,
            yellow: LIGHT_TERMINAL_YELLOW,
            blue: LIGHT_TERMINAL_BLUE,
            magenta: LIGHT_TERMINAL_MAGENTA,
            cyan: LIGHT_TERMINAL_CYAN,
            white: LIGHT_TERMINAL_WHITE,
            bright_black: LIGHT_TERMINAL_BRIGHT_BLACK,
            bright_red: LIGHT_TERMINAL_BRIGHT_RED,
            bright_green: LIGHT_TERMINAL_BRIGHT_GREEN,
            bright_yellow: LIGHT_TERMINAL_BRIGHT_YELLOW,
            bright_blue: LIGHT_TERMINAL_BRIGHT_BLUE,
            bright_magenta: LIGHT_TERMINAL_BRIGHT_MAGENTA,
            bright_cyan: LIGHT_TERMINAL_BRIGHT_CYAN,
            bright_white: LIGHT_TERMINAL_BRIGHT_WHITE,
        },
        density: density_metrics(density, scale),
    }
}

pub fn theme(mode: ThemeMode, density: Density, scale: Scale) -> ThemeTokens {
    match mode {
        ThemeMode::Dark => dark_theme(density, scale),
        ThemeMode::Light => light_theme(density, scale),
    }
}

pub fn dark(density: Density, scale: Scale) -> ThemeTokens {
    dark_theme(density, scale)
}

pub fn light(density: Density, scale: Scale) -> ThemeTokens {
    light_theme(density, scale)
}

pub type Theme = ThemeTokens;

/// Byte-preserving names retained for the legacy GPUI surface.
///
/// The compatibility module is deliberately kept in this canonical token
/// source.  Legacy callers still receive their existing palette values while
/// the native cockpit uses the semantic [`ThemeTokens`] contract above; the
/// `theme` module only re-exports this module and cannot become a second color
/// source.
pub mod legacy {
    use super::Color;

    pub const APP_BG: u32 = Color::from_u32(0x18181b).to_u32();
    pub const SIDEBAR_BG: u32 = Color::from_u32(0x27272a).to_u32();
    pub const PANEL_BG: u32 = Color::from_u32(0x18181b).to_u32();
    pub const PANEL_HEADER_BG: u32 = Color::from_u32(0x27272a).to_u32();
    pub const PANEL_CARD_BG: u32 = Color::from_u32(0x18181b).to_u32();
    pub const EDITOR_CARD_BG: u32 = Color::from_u32(0x202127).to_u32();
    pub const EDITOR_FIELD_BG: u32 = Color::from_u32(0x121318).to_u32();
    pub const EDITOR_NOTICE_BG: u32 = Color::from_u32(0x1a202a).to_u32();
    pub const TOPBAR_BG: u32 = Color::from_u32(0x27272a).to_u32();
    pub const TAB_BAR_BG: u32 = Color::from_u32(0x27272a).to_u32();
    pub const TAB_ACTIVE_BG: u32 = Color::from_u32(0x18181b).to_u32();
    pub const TAB_HOVER_BG: u32 = Color::from_u32(0x323238).to_u32();
    pub const STATUS_BAR_BG: u32 = Color::from_u32(0x09090b).to_u32();
    pub const TERMINAL_BG: u32 = Color::from_u32(0x09090b).to_u32();

    pub const PROJECT_ROW_BG: u32 = Color::from_u32(0x3f3f46).to_u32();
    pub const AGENT_ROW_BG: u32 = Color::from_u32(0x27272a).to_u32();

    pub const BORDER_PRIMARY: u32 = Color::from_u32(0x3f3f46).to_u32();
    pub const BORDER_SECONDARY: u32 = Color::from_u32(0x27272a).to_u32();
    pub const BORDER_ACCENT: u32 = Color::from_u32(0x243040).to_u32();

    pub const TEXT_PRIMARY: u32 = Color::from_u32(0xe4e4e7).to_u32();
    pub const TEXT_MUTED: u32 = Color::from_u32(0xa1a1aa).to_u32();
    pub const TEXT_SUBTLE: u32 = Color::from_u32(0x71717a).to_u32();
    pub const TEXT_DIM: u32 = Color::from_u32(0x52525b).to_u32();

    pub const SELECTION_BG: u32 = Color::from_u32(0x22364d).to_u32();
    pub const SELECTION_TEXT: u32 = Color::from_u32(0xf8fafc).to_u32();
    pub const PROJECT_DOT: u32 = Color::from_u32(0x6366f1).to_u32();
    pub const AI_DOT: u32 = Color::from_u32(0xf59e0b).to_u32();
    pub const SSH_DOT: u32 = Color::from_u32(0x06b6d4).to_u32();
    pub const SUCCESS_BG: u32 = Color::from_u32(0x142117).to_u32();
    pub const SUCCESS_TEXT: u32 = Color::from_u32(0x4ade80).to_u32();
    pub const WARNING_TEXT: u32 = Color::from_u32(0xfacc15).to_u32();
    pub const EXTERNAL_TEXT: u32 = Color::from_u32(0x60a5fa).to_u32();
    pub const DANGER_TEXT: u32 = Color::from_u32(0xfb7185).to_u32();
    pub const DANGER_BG_SUBTLE: u32 = Color::from_u32(0x2a1517).to_u32();

    pub const PRIMARY: u32 = Color::from_u32(0x4f46e5).to_u32();
    pub const PRIMARY_HOVER: u32 = Color::from_u32(0x4338ca).to_u32();
    pub const PRIMARY_MUTED: u32 = Color::from_u32(0x2c266b).to_u32();
    pub const ROW_HOVER_BG: u32 = Color::from_u32(0x323238).to_u32();
    pub const BUTTON_HOVER_BG: u32 = Color::from_u32(0x52525b).to_u32();
}

pub use legacy::*;

pub fn parse_hex_color(value: Option<&str>, fallback: u32) -> u32 {
    let Some(value) = value.map(str::trim) else {
        return fallback;
    };
    let hex = value.strip_prefix('#').unwrap_or(value);
    if hex.len() != 6 {
        return fallback;
    }
    u32::from_str_radix(hex, 16).unwrap_or(fallback)
}
