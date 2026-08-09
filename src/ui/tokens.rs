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

    const fn factor(self) -> f32 {
        self.percent() as f32 / 100.0
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

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ThemeTokens {
    pub mode: ThemeMode,
    pub text: TextTokens,
    pub surfaces: SurfaceTokens,
    pub borders: BorderTokens,
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
        vec![
            ContrastPair {
                name: "text_primary_on_canvas",
                foreground: self.text.primary,
                background: self.surfaces.canvas,
            },
            ContrastPair {
                name: "text_secondary_on_canvas",
                foreground: self.text.secondary,
                background: self.surfaces.canvas,
            },
            ContrastPair {
                name: "text_muted_on_raised",
                foreground: self.text.muted,
                background: self.surfaces.raised,
            },
            ContrastPair {
                name: "text_on_selection",
                foreground: self.text.on_selection,
                background: self.surfaces.selection,
            },
            ContrastPair {
                name: "terminal_foreground_on_background",
                foreground: self.terminal.foreground,
                background: self.terminal.background,
            },
        ]
    }

    pub fn large_text_contrast_pairs(self) -> Vec<ContrastPair> {
        vec![ContrastPair {
            name: "text_primary_large_on_canvas",
            foreground: self.text.primary,
            background: self.surfaces.canvas,
        }]
    }

    pub fn ui_indicator_contrast_pairs(self) -> Vec<ContrastPair> {
        vec![
            ContrastPair {
                name: "focus_ring_on_canvas",
                foreground: self.borders.focus,
                background: self.surfaces.canvas,
            },
            ContrastPair {
                name: "selection_border_on_canvas",
                foreground: self.borders.selection,
                background: self.surfaces.canvas,
            },
            ContrastPair {
                name: "status_external_on_raised",
                foreground: self.status.external,
                background: self.surfaces.raised,
            },
            ContrastPair {
                name: "status_attention_on_raised",
                foreground: self.status.attention,
                background: self.surfaces.raised,
            },
            ContrastPair {
                name: "status_success_on_raised",
                foreground: self.status.success,
                background: self.surfaces.raised,
            },
            ContrastPair {
                name: "status_warning_on_raised",
                foreground: self.status.warning,
                background: self.surfaces.raised,
            },
            ContrastPair {
                name: "status_destructive_on_raised",
                foreground: self.status.destructive,
                background: self.surfaces.raised,
            },
            ContrastPair {
                name: "status_inactive_on_raised",
                foreground: self.status.inactive,
                background: self.surfaces.raised,
            },
        ]
    }

    pub fn disabled_text_contrast_pairs(self) -> Vec<ContrastPair> {
        vec![
            ContrastPair {
                name: "text_disabled_on_raised",
                foreground: self.text.disabled,
                background: self.surfaces.raised,
            },
            ContrastPair {
                name: "text_disabled_on_disabled_surface",
                foreground: self.text.disabled,
                background: self.surfaces.disabled,
            },
        ]
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
const DARK_TEXT_MUTED: Color = Color::from_u32(0xa1a1aa);
const DARK_TEXT_DISABLED: Color = Color::from_u32(0xa1a1aa);
const DARK_TEXT_INVERSE: Color = Color::from_u32(0x18181b);
const DARK_TEXT_ON_ACCENT: Color = Color::from_u32(0x09090b);
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

const DARK_TERMINAL_BLACK: Color = Color::from_u32(0x18181b);
const DARK_TERMINAL_RED: Color = Color::from_u32(0xef4444);
const DARK_TERMINAL_GREEN: Color = Color::from_u32(0x22c55e);
const DARK_TERMINAL_YELLOW: Color = Color::from_u32(0xeab308);
const DARK_TERMINAL_BLUE: Color = Color::from_u32(0x3b82f6);
const DARK_TERMINAL_MAGENTA: Color = Color::from_u32(0xa855f7);
const DARK_TERMINAL_CYAN: Color = Color::from_u32(0x06b6d4);
const DARK_TERMINAL_WHITE: Color = Color::from_u32(0xe4e4e7);
const DARK_TERMINAL_BRIGHT_BLACK: Color = Color::from_u32(0x71717a);
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
const LIGHT_TEXT_MUTED: Color = Color::from_u32(0x475569);
const LIGHT_TEXT_DISABLED: Color = Color::from_u32(0x64748b);
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

const LIGHT_TERMINAL_BACKGROUND: Color = Color::from_u32(0x1e293b);
const LIGHT_TERMINAL_FOREGROUND: Color = Color::from_u32(0xf8fafc);
const LIGHT_TERMINAL_SELECTION: Color = Color::from_u32(0x475569);
const LIGHT_TERMINAL_BLACK: Color = Color::from_u32(0x0f172a);
const LIGHT_TERMINAL_RED: Color = Color::from_u32(0xb42318);
const LIGHT_TERMINAL_GREEN: Color = Color::from_u32(0x087a42);
const LIGHT_TERMINAL_YELLOW: Color = Color::from_u32(0x8f4b00);
const LIGHT_TERMINAL_BLUE: Color = Color::from_u32(0x075eaf);
const LIGHT_TERMINAL_MAGENTA: Color = Color::from_u32(0x7e22ce);
const LIGHT_TERMINAL_CYAN: Color = Color::from_u32(0x0e7490);
const LIGHT_TERMINAL_WHITE: Color = Color::from_u32(0xf8fafc);
const LIGHT_TERMINAL_BRIGHT_BLACK: Color = Color::from_u32(0x475569);
const LIGHT_TERMINAL_BRIGHT_RED: Color = Color::from_u32(0xdc2626);
const LIGHT_TERMINAL_BRIGHT_GREEN: Color = Color::from_u32(0x16a34a);
const LIGHT_TERMINAL_BRIGHT_YELLOW: Color = Color::from_u32(0xa16207);
const LIGHT_TERMINAL_BRIGHT_BLUE: Color = Color::from_u32(0x1d4ed8);
const LIGHT_TERMINAL_BRIGHT_MAGENTA: Color = Color::from_u32(0x9333ea);
const LIGHT_TERMINAL_BRIGHT_CYAN: Color = Color::from_u32(0x0891b2);
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
        status: StatusTokens {
            external: DARK_STATUS_EXTERNAL,
            attention: DARK_STATUS_ATTENTION,
            success: DARK_STATUS_SUCCESS,
            warning: DARK_STATUS_WARNING,
            destructive: DARK_STATUS_DESTRUCTIVE,
            inactive: DARK_STATUS_INACTIVE,
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
        status: StatusTokens {
            external: LIGHT_STATUS_EXTERNAL,
            attention: LIGHT_STATUS_ATTENTION,
            success: LIGHT_STATUS_SUCCESS,
            warning: LIGHT_STATUS_WARNING,
            destructive: LIGHT_STATUS_DESTRUCTIVE,
            inactive: LIGHT_STATUS_INACTIVE,
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

/// Names retained for the legacy GPUI surface. They are aliases into the
/// dark semantic theme, not a second palette.
pub mod legacy {
    use super::*;

    pub const APP_BG: u32 = DARK_SURFACE_CANVAS.to_u32();
    pub const SIDEBAR_BG: u32 = DARK_SURFACE_RAISED.to_u32();
    pub const PANEL_BG: u32 = DARK_SURFACE_CANVAS.to_u32();
    pub const PANEL_HEADER_BG: u32 = DARK_SURFACE_RAISED.to_u32();
    pub const PANEL_CARD_BG: u32 = DARK_SURFACE_CANVAS.to_u32();
    pub const EDITOR_CARD_BG: u32 = DARK_SURFACE_OVERLAY.to_u32();
    pub const EDITOR_FIELD_BG: u32 = DARK_SURFACE_SUNKEN.to_u32();
    pub const EDITOR_NOTICE_BG: u32 = DARK_SURFACE_RAISED.to_u32();
    pub const TOPBAR_BG: u32 = DARK_SURFACE_RAISED.to_u32();
    pub const TAB_BAR_BG: u32 = DARK_SURFACE_RAISED.to_u32();
    pub const TAB_ACTIVE_BG: u32 = DARK_SURFACE_CANVAS.to_u32();
    pub const TAB_HOVER_BG: u32 = DARK_SURFACE_HOVER.to_u32();
    pub const STATUS_BAR_BG: u32 = DARK_SURFACE_SUNKEN.to_u32();
    pub const TERMINAL_BG: u32 = DARK_SURFACE_SUNKEN.to_u32();

    pub const PROJECT_ROW_BG: u32 = DARK_SURFACE_SELECTION.to_u32();
    pub const AGENT_ROW_BG: u32 = DARK_SURFACE_RAISED.to_u32();

    pub const BORDER_PRIMARY: u32 = DARK_BORDER_DEFAULT.to_u32();
    pub const BORDER_SECONDARY: u32 = DARK_BORDER_SUBTLE.to_u32();
    pub const BORDER_ACCENT: u32 = DARK_BORDER_SELECTION.to_u32();

    pub const TEXT_PRIMARY: u32 = DARK_TEXT_PRIMARY.to_u32();
    pub const TEXT_MUTED: u32 = DARK_TEXT_MUTED.to_u32();
    pub const TEXT_SUBTLE: u32 = DARK_TEXT_MUTED.to_u32();
    pub const TEXT_DIM: u32 = DARK_BORDER_STRONG.to_u32();

    pub const SELECTION_BG: u32 = DARK_SURFACE_SELECTION.to_u32();
    pub const SELECTION_TEXT: u32 = DARK_TEXT_ON_SELECTION.to_u32();
    pub const PROJECT_DOT: u32 = DARK_BORDER_STRONG.to_u32();
    pub const AI_DOT: u32 = DARK_STATUS_ATTENTION.to_u32();
    pub const SSH_DOT: u32 = DARK_STATUS_EXTERNAL.to_u32();
    pub const SUCCESS_BG: u32 = DARK_SURFACE_RAISED.to_u32();
    pub const SUCCESS_TEXT: u32 = DARK_STATUS_SUCCESS.to_u32();
    pub const WARNING_TEXT: u32 = DARK_STATUS_WARNING.to_u32();
    pub const EXTERNAL_TEXT: u32 = DARK_STATUS_EXTERNAL.to_u32();
    pub const DANGER_TEXT: u32 = DARK_STATUS_DESTRUCTIVE.to_u32();
    pub const DANGER_BG_SUBTLE: u32 = DARK_SURFACE_SUNKEN.to_u32();

    pub const PRIMARY: u32 = DARK_BORDER_FOCUS.to_u32();
    pub const PRIMARY_HOVER: u32 = DARK_BORDER_STRONG.to_u32();
    pub const PRIMARY_MUTED: u32 = DARK_SURFACE_SELECTION.to_u32();
    pub const ROW_HOVER_BG: u32 = DARK_SURFACE_HOVER.to_u32();
    pub const BUTTON_HOVER_BG: u32 = DARK_SURFACE_OVERLAY.to_u32();
}
