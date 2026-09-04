//! Semantic visual tokens for the native Task Cockpit.
//!
//! This module is deliberately the only place where UI color values are
//! defined. Consumers choose a meaning from [`ThemeTokens`] and do not need
//! to know which palette value implements it.

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ThemeMode {
    Dark,
    Light,
    HighContrast,
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

/// An sRGB color, opaque by construction: every token constructor
/// ([`Color::rgb`], [`Color::from_u32`]) yields a fully opaque colour, so no
/// token carries transparency of its own.
///
/// The one route to a translucent colour is [`Color::with_alpha`], which a
/// painter calls on a token it already holds -- the state-dot halo and the
/// needs-you / blocked row borders. Compositing therefore stays a decision of
/// the surface that owns the token rather than a property of the palette.
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

    /// Convert a semantic token to GPUI's native color without exposing palette
    /// literals to UI consumers. Every token constructor is opaque, so this is
    /// `rgb()` for a token; only a colour narrowed by [`Color::with_alpha`]
    /// carries transparency through to the compositor.
    pub fn to_gpui(self) -> gpui::Rgba {
        gpui::rgba(
            (u32::from(self.red()) << 24)
                | (u32::from(self.green()) << 16)
                | (u32::from(self.blue()) << 8)
                | u32::from(self.alpha()),
        )
    }

    /// The same hue at a fraction of its opacity, for a halo or a tinted
    /// border that must read as the state colour without becoming a second
    /// saturated colour on screen. Deliberately not a token: the token
    /// contract stays opaque and the caller owns the compositing decision.
    pub fn with_alpha(self, alpha: f32) -> Self {
        let alpha = (alpha.clamp(0.0, 1.0) * 255.0).round() as u8;
        Self {
            rgba: [self.red(), self.green(), self.blue(), alpha],
        }
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

/// Resolve a fractional foreground/surface recipe into one opaque token
/// colour. GPUI cannot rely on browser-style alpha compositing, and the token
/// contract deliberately stays opaque, so shared conversation surfaces blend
/// at their owning boundary.
pub(crate) fn mix_color(base: Color, other: Color, amount: f32) -> Color {
    let amount = amount.clamp(0.0, 1.0);
    let blend = |a: u8, b: u8| -> u8 {
        let a = f32::from(a);
        let b = f32::from(b);
        (a + (b - a) * amount).round().clamp(0.0, 255.0) as u8
    };
    Color::rgb(
        blend(base.red(), other.red()),
        blend(base.green(), other.green()),
        blend(base.blue(), other.blue()),
    )
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
    /// One step above `primary`, for the few places a title must read as
    /// louder than every other title around it -- the board's needs-you rows.
    /// Pure white on dark, pure black on light: there is nowhere above it to go.
    pub emphasis: Color,
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

/// The one visual contract every scrollbar in the app renders from.
///
/// There are two painters -- the shared `ui::scrollbar` element used by every
/// scrollable shell surface, and the terminal's hand-painted gutter, which has
/// to stay hand-painted because it lives inside a `canvas` element. Neither
/// carries a width, a length, a radius or a colour of its own: both read this
/// struct, so "one look" is a property of the type rather than of two lists of
/// constants that happen to agree today.
///
/// Widths are logical pixels and are deliberately NOT density-scaled: a
/// scrollbar is a pointer target, and the redesign spec pins 4 px idle /
/// 10 px on hover or drag at every density.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ScrollbarTokens {
    /// Thumb width at rest. Thin and delicate, but painted at full opacity so
    /// it stays visible -- it never fades out.
    pub idle_thumb_width: f32,
    /// Thumb width while the pointer is anywhere in the gutter, or while the
    /// thumb is being dragged. Easy to grab.
    pub active_thumb_width: f32,
    /// Width reserved for the gutter, in every state. The thumb is centred in
    /// it, so growing from idle to active widens the thumb symmetrically and
    /// never reflows the content beside it.
    pub gutter_width: f32,
    /// Shortest thumb the track will paint, so a very long document still
    /// leaves something grabbable.
    pub min_thumb_length: f32,
    /// Distance the track is inset from the top and bottom of the gutter.
    pub track_inset_y: f32,
    /// Corner radius as a fraction of the current thumb width. `0.5` is a
    /// pill at both widths, which is why one ratio replaces two radii.
    pub thumb_radius_ratio: f32,
    /// Colours for a scrollbar on a dark ground.
    pub on_dark: ScrollbarColors,
    /// Colours for a scrollbar on a light ground.
    ///
    /// Two triples rather than one because a scrollbar has to be visible on
    /// whatever it lands on, and the surfaces are not all one polarity: the
    /// light theme puts the terminal on `0x1e293b`, a dark slate island inside
    /// a near-white shell, so a single thumb colour is either invisible on the
    /// shell or invisible on the terminal. Measured, not guessed -- the first
    /// single-triple attempt failed `scrollbar_thumbs_clear_the_non_text_contrast_floor`
    /// at 1.413:1 on exactly that surface. The geometry stays single: one look,
    /// resolved against its ground, which is the same rule `text.inverse`
    /// already follows.
    pub on_light: ScrollbarColors,
}

/// One scrollbar's three colours. Split from the geometry so the polarity
/// rule has something to return.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ScrollbarColors {
    /// Thumb at rest.
    pub thumb_idle: Color,
    /// Thumb while the gutter is hovered or the thumb is being dragged.
    pub thumb_hover: Color,
    /// Track behind the thumb. Painted only in the active state: at rest the
    /// bar is the thumb alone, which is what makes the idle look delicate.
    pub track_active: Color,
}

impl ScrollbarColors {
    pub fn thumb(self, active: bool) -> Color {
        if active {
            self.thumb_hover
        } else {
            self.thumb_idle
        }
    }
}

impl ScrollbarTokens {
    /// The colours for a scrollbar painted over `background`.
    ///
    /// The 0.45 luminance split is the same one `ThemePalette::tokens` uses to
    /// decide a palette's appearance, so a surface cannot be "light" to one and
    /// "dark" to the other.
    pub fn colors_on(self, background: Color) -> ScrollbarColors {
        if srgb_luminance(background) >= 0.45 {
            self.on_light
        } else {
            self.on_dark
        }
    }

    pub fn thumb_width(self, active: bool) -> f32 {
        if active {
            self.active_thumb_width
        } else {
            self.idle_thumb_width
        }
    }

    pub fn thumb_radius(self, active: bool) -> f32 {
        self.thumb_width(active) * self.thumb_radius_ratio
    }
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
    pub scrollbar: ScrollbarTokens,
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
                name: "text_emphasis",
                color: self.text.emphasis,
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
        // There is deliberately no `text.on_accent` pair against the destructive
        // fills. `text.on_accent` is the foreground `AccentForeground` paints
        // over `Accent`, which is the PRIMARY action fill; nothing anywhere
        // paints it on a destructive fill, which carries its own foreground.
        // Holding both families against one token was only satisfiable while the
        // two fills had the same polarity -- the redesign's primary is a light
        // slab and destructive is still a dark red, and no single colour clears
        // 4.5:1 on both (proved by exhaustion over the grey ramp: the primary
        // side needs luminance <= 0.172, the destructive side >= 0.791). The
        // real destructive pairs are `action_destructive_*_on_surface` in
        // `interaction_state_contrast_pairs`, at the same 4.5:1 floor.
        //
        // The other surface `text.on_accent` really lands on:
        // `MessageActionForeground` over `MessageAction`, which is
        // `status.external`. Ungated until now, and the pre-redesign near-white
        // read 2.43:1 there.
        pairs.push(contrast_pair(
            "text_on_accent_on_status_external",
            self.text.on_accent,
            self.status.external,
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

// The redesign's dark shell is a near-black neutral grey stack: the board
// canvas sits darkest, row boxes lift one step, and colour is reserved for
// attention (amber) and destruction (red) so nothing else competes for the
// eye. Values come from the redesign spec rather than the old plum shell.
// One scrollbar geometry for the whole app, shared by all three modes. The
// redesign spec pins the two widths; the rest is derived so that the thumb
// grows symmetrically inside a fixed gutter and never reflows its neighbour:
// idle leaves (14 - 4) / 2 = 5 px each side, active leaves (14 - 10) / 2 = 2.
const SCROLLBAR_IDLE_THUMB_WIDTH: f32 = 4.0;
const SCROLLBAR_ACTIVE_THUMB_WIDTH: f32 = 10.0;
const SCROLLBAR_GUTTER_WIDTH: f32 = 14.0;
// 24 px, between the terminal's old 18 (too small to hit reliably) and
// gpui-component's 48 (which swallows the position signal on a long log).
const SCROLLBAR_MIN_THUMB_LENGTH: f32 = 24.0;
const SCROLLBAR_TRACK_INSET_Y: f32 = 2.0;
const SCROLLBAR_THUMB_RADIUS_RATIO: f32 = 0.5;

// Idle thumbs are measured against every background a scrollbar can land on
// -- canvas, sunken, raised, overlay and the terminal plane, in all three
// modes -- and clear the 3:1 WCAG 1.4.11 non-text floor on every one of them;
// hover clears 6:1. Asserted by
// `scrollbar_thumbs_clear_the_non_text_contrast_floor`, which is what found
// the light theme's dark terminal island in the first place.
//
// The dark triple is deliberately one step brighter than the redesign's
// `text.muted` candidate: the binding surface is not the dark shell (where
// 0x6b6b74 reads 3.60:1) but the LIGHT theme's terminal plane at 0x1e293b,
// where the same value drops to 2.773:1. 0x76767f clears it at 3.252:1 and
// stays delicate on the dark shell at 4.222:1.
const SCROLLBAR_ON_DARK_THUMB_IDLE: Color = Color::from_u32(0x76767f);
const SCROLLBAR_ON_DARK_THUMB_HOVER: Color = Color::from_u32(0xb0b0b9);
const SCROLLBAR_ON_DARK_TRACK_ACTIVE: Color = Color::from_u32(0x26262b);

const SCROLLBAR_ON_LIGHT_THUMB_IDLE: Color = Color::from_u32(0x64748b);
const SCROLLBAR_ON_LIGHT_THUMB_HOVER: Color = Color::from_u32(0x334155);
const SCROLLBAR_ON_LIGHT_TRACK_ACTIVE: Color = Color::from_u32(0xcbd5e1);

// High contrast has no light surface, so both polarities resolve to its own
// louder pair rather than to the shared one -- 3.6:1 is a correct scrollbar
// and a wrong high-contrast scrollbar.
const HC_SCROLLBAR_THUMB_IDLE: Color = Color::from_u32(0xd6d6d6);
const HC_SCROLLBAR_THUMB_HOVER: Color = Color::from_u32(0xffffff);
const HC_SCROLLBAR_TRACK_ACTIVE: Color = Color::from_u32(0x2a2a2a);

const DARK_SURFACE_CANVAS: Color = Color::from_u32(0x101013);
const DARK_SURFACE_RAISED: Color = Color::from_u32(0x151518);
const DARK_SURFACE_OVERLAY: Color = Color::from_u32(0x1a1a1f);
const DARK_SURFACE_SUNKEN: Color = Color::from_u32(0x111114);
const DARK_SURFACE_HOVER: Color = Color::from_u32(0x17171c);
// The selected row has to read as a filled slab, not a hairline lift, so this
// climbs as far above `raised` as the text gates allow. `0x26262b` (the subtle
// border) was ruled but drops `text_disabled_on_selection` to 4.118:1;
// `0x1e1e23` is the ceiling for that 4.5:1 floor (4.539:1) and is what ships.
// `surfaces.disabled` stepped down to `0x1b1b20` to keep the two distinct --
// disabled text reads 4.691:1 there, so the floor is still clear on both.
const DARK_SURFACE_SELECTION: Color = Color::from_u32(0x1e1e23);
const DARK_SURFACE_DISABLED: Color = Color::from_u32(0x1b1b20);

const DARK_TEXT_PRIMARY: Color = Color::from_u32(0xe6e6ea);
const DARK_TEXT_EMPHASIS: Color = Color::from_u32(0xffffff);
const DARK_TEXT_SECONDARY: Color = Color::from_u32(0x9a9aa3);
// Spec asks 0x6b6b74, but that is 3.45:1 on raised and fails the 4.5:1 AA gate; 0x86868f is 5.05:1.
const DARK_TEXT_MUTED: Color = Color::from_u32(0x86868f);
// Dimmest step of the ramp. Darker reads better but 0x85858e is the floor: below it disabled
// text drops under 4.5:1 on the lightest surface it lands on, surfaces.selection (0x1e1e23),
// where it reads 4.539:1.
const DARK_TEXT_DISABLED: Color = Color::from_u32(0x85858e);
const DARK_TEXT_INVERSE: Color = Color::from_u32(0xf8fafc);
// The redesign's loud control is a light neutral slab, so the text that lands
// on it is the canvas colour rather than a near-white. This is the foreground
// `ThemeColorRole::AccentForeground` paints over `Accent`
// (`actions.primary.default.background`), and it also fixes the message-action
// pair, where the old near-white read 2.43:1 on `status.external`.
const DARK_TEXT_ON_ACCENT: Color = Color::from_u32(0x101013);
const DARK_TEXT_ON_SELECTION: Color = Color::from_u32(0xffffff);

const DARK_BORDER_SUBTLE: Color = Color::from_u32(0x26262b);
const DARK_BORDER_DEFAULT: Color = Color::from_u32(0x2c2c33);
const DARK_BORDER_STRONG: Color = Color::from_u32(0x34343c);
// A focus ring is a control, not a status, so it is a grey on the ramp rather
// than the warning yellow it used to borrow: 5.27:1 on the canvas, which clears
// the 3:1 UI-indicator floor at 1-2 px without adding a colour to the shell.
const DARK_BORDER_FOCUS: Color = Color::from_u32(0x86868f);
const DARK_BORDER_SELECTION: Color = Color::from_u32(0xa1a1aa);
const DARK_BORDER_DISABLED: Color = Color::from_u32(0x52525b);

const DARK_STATUS_EXTERNAL: Color = Color::from_u32(0x60a5fa);
const DARK_STATUS_ATTENTION: Color = Color::from_u32(0xf2b441);
const DARK_STATUS_SUCCESS: Color = Color::from_u32(0x7fb07f);
const DARK_STATUS_WARNING: Color = Color::from_u32(0xfacc15);
const DARK_STATUS_DESTRUCTIVE: Color = Color::from_u32(0xe5484d);
const DARK_STATUS_INACTIVE: Color = Color::from_u32(0x86868f);

// The primary action is the one "loud" control on a shell where colour means
// "this needs you", so it is loud by inversion rather than by hue: a light grey
// slab carrying the canvas colour as its text. Focus keeps the default fill and
// says so with `borders.focus`; the pressed state steps down one.
const DARK_ACTION_PRIMARY_DEFAULT: Color = Color::from_u32(0xe6e6ea);
const DARK_ACTION_PRIMARY_HOVER: Color = Color::from_u32(0xffffff);
const DARK_ACTION_PRIMARY_FOCUS: Color = Color::from_u32(0xe6e6ea);
const DARK_ACTION_PRIMARY_SELECTED: Color = Color::from_u32(0xd0d0d6);
const DARK_ACTION_PRIMARY_DISABLED: Color = Color::from_u32(0x606876);
const DARK_ACTION_PRIMARY_FOREGROUND: Color = Color::from_u32(0x101013);
// The disabled fill is the one primary state that stays darker than its text:
// dark-on-`0x606876` is 3.20:1, so the disabled slab keeps a light foreground.
const DARK_ACTION_PRIMARY_DISABLED_FOREGROUND: Color = Color::from_u32(0xf8fafc);
const DARK_ACTION_DESTRUCTIVE_DEFAULT: Color = Color::from_u32(0xc62828);
const DARK_ACTION_DESTRUCTIVE_HOVER: Color = Color::from_u32(0xc92a2a);
const DARK_ACTION_DESTRUCTIVE_FOCUS: Color = Color::from_u32(0xc62828);
const DARK_ACTION_DESTRUCTIVE_SELECTED: Color = Color::from_u32(0xc92a2a);
const DARK_ACTION_DESTRUCTIVE_DISABLED: Color = Color::from_u32(0x606876);
const DARK_ACTION_DESTRUCTIVE_FOREGROUND: Color = Color::from_u32(0xffffff);

/// Shared translucent scrim for native modal surfaces. Kept in the canonical
/// token module so native views never own ad-hoc RGB(A) literals.
pub const MODAL_BACKDROP_RGBA: u32 = 0x00000059;

/// One muted hue per project, in the order the board's colour book hands them
/// out. Dim and cool by construction so amber and red stay the only saturated
/// colours on screen (spec 5.3). Defined here rather than beside the colour
/// book because this module is the only place a colour literal may be written;
/// `crate::ui::board::project_colour` re-exports it.
pub const PROJECT_PALETTE: [Color; 8] = [
    Color::from_u32(0x5aa3a0), // teal
    Color::from_u32(0x7a86c4), // slate
    Color::from_u32(0xa78a5c), // sand
    Color::from_u32(0x8c6fa8), // mauve
    Color::from_u32(0x7a9a6a), // moss
    Color::from_u32(0x9a7a8a), // dusk
    Color::from_u32(0x6f8fa8), // steel
    Color::from_u32(0xa8806f), // clay
];

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

const DARK_TERMINAL_BACKGROUND: Color = Color::from_u32(0x0b0b0d);
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
const LIGHT_TEXT_EMPHASIS: Color = Color::from_u32(0x000000);
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

const HC_SURFACE_CANVAS: Color = Color::from_u32(0x000000);
const HC_SURFACE_RAISED: Color = Color::from_u32(0x141414);
const HC_SURFACE_OVERLAY: Color = Color::from_u32(0x1f1f1f);
const HC_SURFACE_SUNKEN: Color = Color::from_u32(0x000000);
const HC_SURFACE_HOVER: Color = Color::from_u32(0x1f1f1f);
const HC_SURFACE_SELECTION: Color = Color::from_u32(0x2a2a2a);
const HC_SURFACE_DISABLED: Color = Color::from_u32(0x141414);
const HC_TEXT_PRIMARY: Color = Color::from_u32(0xffffff);
const HC_TEXT_SECONDARY: Color = Color::from_u32(0xf5f5f5);
const HC_TEXT_MUTED: Color = Color::from_u32(0xe8e8e8);
const HC_TEXT_DISABLED: Color = Color::from_u32(0xd6d6d6);
const HC_TEXT_INVERSE: Color = Color::from_u32(0xffffff);
const HC_BORDER_FOCUS: Color = Color::from_u32(0xffff00);
const HC_BORDER_DEFAULT: Color = Color::from_u32(0xd6d6d6);
const HC_BORDER_STRONG: Color = Color::from_u32(0xffffff);
const HC_BORDER_SUBTLE: Color = Color::from_u32(0xa3a3a3);
const HC_BORDER_SELECTION: Color = Color::from_u32(0xffff00);
const HC_BORDER_DISABLED: Color = Color::from_u32(0xa3a3a3);

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
            emphasis: DARK_TEXT_EMPHASIS,
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
        scrollbar: ScrollbarTokens {
            idle_thumb_width: SCROLLBAR_IDLE_THUMB_WIDTH,
            active_thumb_width: SCROLLBAR_ACTIVE_THUMB_WIDTH,
            gutter_width: SCROLLBAR_GUTTER_WIDTH,
            min_thumb_length: SCROLLBAR_MIN_THUMB_LENGTH,
            track_inset_y: SCROLLBAR_TRACK_INSET_Y,
            thumb_radius_ratio: SCROLLBAR_THUMB_RADIUS_RATIO,
            on_dark: ScrollbarColors {
                thumb_idle: SCROLLBAR_ON_DARK_THUMB_IDLE,
                thumb_hover: SCROLLBAR_ON_DARK_THUMB_HOVER,
                track_active: SCROLLBAR_ON_DARK_TRACK_ACTIVE,
            },
            on_light: ScrollbarColors {
                thumb_idle: SCROLLBAR_ON_LIGHT_THUMB_IDLE,
                thumb_hover: SCROLLBAR_ON_LIGHT_THUMB_HOVER,
                track_active: SCROLLBAR_ON_LIGHT_TRACK_ACTIVE,
            },
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
                    foreground: DARK_ACTION_PRIMARY_DISABLED_FOREGROUND,
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
            background: DARK_TERMINAL_BACKGROUND,
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
            emphasis: LIGHT_TEXT_EMPHASIS,
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
        scrollbar: ScrollbarTokens {
            idle_thumb_width: SCROLLBAR_IDLE_THUMB_WIDTH,
            active_thumb_width: SCROLLBAR_ACTIVE_THUMB_WIDTH,
            gutter_width: SCROLLBAR_GUTTER_WIDTH,
            min_thumb_length: SCROLLBAR_MIN_THUMB_LENGTH,
            track_inset_y: SCROLLBAR_TRACK_INSET_Y,
            thumb_radius_ratio: SCROLLBAR_THUMB_RADIUS_RATIO,
            on_dark: ScrollbarColors {
                thumb_idle: SCROLLBAR_ON_DARK_THUMB_IDLE,
                thumb_hover: SCROLLBAR_ON_DARK_THUMB_HOVER,
                track_active: SCROLLBAR_ON_DARK_TRACK_ACTIVE,
            },
            on_light: ScrollbarColors {
                thumb_idle: SCROLLBAR_ON_LIGHT_THUMB_IDLE,
                thumb_hover: SCROLLBAR_ON_LIGHT_THUMB_HOVER,
                track_active: SCROLLBAR_ON_LIGHT_TRACK_ACTIVE,
            },
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

fn high_contrast_theme(density: Density, scale: Scale) -> ThemeTokens {
    let mut tokens = dark_theme(density, scale);
    tokens.mode = ThemeMode::HighContrast;
    tokens.text.primary = HC_TEXT_PRIMARY;
    // High contrast keeps white as its loudest text; there is no step above it.
    tokens.text.emphasis = DARK_TEXT_EMPHASIS;
    tokens.text.secondary = HC_TEXT_SECONDARY;
    tokens.text.muted = HC_TEXT_MUTED;
    tokens.text.disabled = HC_TEXT_DISABLED;
    tokens.text.inverse = HC_TEXT_INVERSE;
    tokens.text.on_selection = HC_TEXT_PRIMARY;
    tokens.surfaces.canvas = HC_SURFACE_CANVAS;
    tokens.surfaces.raised = HC_SURFACE_RAISED;
    tokens.surfaces.overlay = HC_SURFACE_OVERLAY;
    tokens.surfaces.sunken = HC_SURFACE_SUNKEN;
    tokens.surfaces.hover = HC_SURFACE_HOVER;
    tokens.surfaces.selection = HC_SURFACE_SELECTION;
    tokens.surfaces.disabled = HC_SURFACE_DISABLED;
    let hc_scrollbar = ScrollbarColors {
        thumb_idle: HC_SCROLLBAR_THUMB_IDLE,
        thumb_hover: HC_SCROLLBAR_THUMB_HOVER,
        track_active: HC_SCROLLBAR_TRACK_ACTIVE,
    };
    tokens.scrollbar.on_dark = hc_scrollbar;
    tokens.scrollbar.on_light = hc_scrollbar;
    tokens.borders.subtle = HC_BORDER_SUBTLE;
    tokens.borders.default = HC_BORDER_DEFAULT;
    tokens.borders.strong = HC_BORDER_STRONG;
    tokens.borders.focus = HC_BORDER_FOCUS;
    tokens.borders.selection = HC_BORDER_SELECTION;
    tokens.borders.disabled = HC_BORDER_DISABLED;
    tokens.terminal.background = HC_SURFACE_SUNKEN;
    tokens.terminal.foreground = HC_TEXT_PRIMARY;
    tokens.terminal.cursor = HC_TEXT_PRIMARY;
    tokens.terminal.selection = HC_SURFACE_SELECTION;
    tokens
}

pub fn theme(mode: ThemeMode, density: Density, scale: Scale) -> ThemeTokens {
    match mode {
        ThemeMode::Dark => dark_theme(density, scale),
        ThemeMode::Light => light_theme(density, scale),
        ThemeMode::HighContrast => high_contrast_theme(density, scale),
    }
}

pub fn dark(density: Density, scale: Scale) -> ThemeTokens {
    dark_theme(density, scale)
}

pub fn light(density: Density, scale: Scale) -> ThemeTokens {
    light_theme(density, scale)
}

pub fn high_contrast(density: Density, scale: Scale) -> ThemeTokens {
    high_contrast_theme(density, scale)
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
    /// Visible secondary/artifact text on [`PANEL_BG`].
    ///
    /// Previously `0x52525b` (~2.29:1 against panel). Aligned to the dark
    /// semantic muted token so normal visible dim text meets WCAG AA (≥4.5:1)
    /// without introducing a second palette.
    pub const TEXT_DIM: u32 = Color::from_u32(0x86868f).to_u32();

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

/// Visible secondary/artifact text for the active theme mode.
///
/// Dark mode keeps the legacy [`TEXT_DIM`] compatibility alias (now AA-safe on
/// [`PANEL_BG`]). Light mode uses the semantic muted token against canvas so
/// both supported themes meet WCAG AA for normal text without a second palette.
pub fn visible_dim_text(mode: ThemeMode) -> Color {
    match mode {
        ThemeMode::Dark => Color::from_u32(TEXT_DIM),
        ThemeMode::Light => LIGHT_TEXT_MUTED,
        ThemeMode::HighContrast => HC_TEXT_MUTED,
    }
}

/// Panel/surface paired with [`visible_dim_text`] for contrast checks and
/// native task UI secondary copy.
pub fn visible_dim_panel_surface(mode: ThemeMode) -> Color {
    match mode {
        ThemeMode::Dark => Color::from_u32(PANEL_BG),
        ThemeMode::Light => LIGHT_SURFACE_CANVAS,
        ThemeMode::HighContrast => HC_SURFACE_CANVAS,
    }
}

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opaque_mix_preserves_endpoints_and_clamps_the_fraction() {
        let dark = Color::rgb(10, 20, 30);
        let light = Color::rgb(210, 220, 230);
        assert_eq!(mix_color(dark, light, 0.0), dark);
        assert_eq!(mix_color(dark, light, 1.0), light);
        assert_eq!(mix_color(dark, light, -1.0), dark);
        assert_eq!(mix_color(dark, light, 2.0), light);
    }

    #[test]
    fn legacy_text_dim_meets_aa_on_panel_bg() {
        let ratio = contrast_ratio(Color::from_u32(TEXT_DIM), Color::from_u32(PANEL_BG));
        assert!(
            ratio >= 4.5,
            "TEXT_DIM on PANEL_BG must be WCAG AA, got {ratio}"
        );
        assert_eq!(TEXT_DIM, DARK_TEXT_MUTED.to_u32());
    }

    #[test]
    fn visible_dim_text_meets_aa_in_both_theme_modes() {
        for mode in [ThemeMode::Dark, ThemeMode::Light, ThemeMode::HighContrast] {
            let ratio = contrast_ratio(visible_dim_text(mode), visible_dim_panel_surface(mode));
            assert!(
                ratio >= 4.5,
                "visible dim text must be AA for {mode:?}, got {ratio}"
            );
        }
    }

    #[test]
    fn semantic_muted_text_still_meets_aa_on_raised_surfaces() {
        for mode in [ThemeMode::Dark, ThemeMode::Light, ThemeMode::HighContrast] {
            let tokens = theme(mode, Density::Comfortable, Scale::Scale100);
            let ratio = contrast_ratio(tokens.text.muted, tokens.surfaces.raised);
            assert!(
                ratio >= 4.5,
                "semantic muted on raised must stay AA for {mode:?}, got {ratio}"
            );
        }
    }

    #[test]
    fn dark_palette_matches_the_redesign_spec_or_its_ruled_gate_values() {
        let t = dark(Density::Comfortable, Scale::Scale100);
        assert_eq!(t.surfaces.canvas, Color::from_u32(0x101013), "column");
        assert_eq!(t.surfaces.raised, Color::from_u32(0x151518), "row box");
        // Ruled 0x26262b so the selected row reads as a filled slab; that value
        // drops text_disabled_on_selection to 4.118:1. 0x1e1e23 is the ceiling
        // for the 4.5:1 floor (4.539:1) and is what ships; surfaces.disabled
        // moved down to 0x1b1b20 so the two stay distinct.
        assert_eq!(
            t.surfaces.selection,
            Color::from_u32(0x1e1e23),
            "selected row"
        );
        assert_ne!(
            t.surfaces.selection, t.surfaces.disabled,
            "a selected row must not read as a disabled one"
        );
        // Colour is reserved for state that needs you, so neither the focus
        // ring nor the primary action may be a hue.
        assert_eq!(t.borders.focus, Color::from_u32(0x86868f), "focus ring");
        assert_eq!(t.borders.selection, Color::from_u32(0xa1a1aa));
        assert_eq!(
            t.actions.primary.default.background,
            Color::from_u32(0xe6e6ea),
            "primary action fill"
        );
        assert_eq!(
            t.actions.primary.default.foreground,
            Color::from_u32(0x101013),
            "primary action text"
        );
        assert_eq!(
            t.actions.primary.hover.background,
            Color::from_u32(0xffffff)
        );
        assert_eq!(
            t.actions.primary.selected.background,
            Color::from_u32(0xd0d0d6)
        );
        assert_eq!(t.text.on_accent, Color::from_u32(0x101013));
        // The status yellow is a status, not a control: it keeps its hue.
        assert_eq!(t.status.warning, Color::from_u32(0xfacc15));
        assert_eq!(t.surfaces.sunken, Color::from_u32(0x111114), "stream");
        assert_eq!(
            t.surfaces.disabled,
            Color::from_u32(0x1b1b20),
            "disabled row"
        );
        assert_eq!(t.terminal.background, Color::from_u32(0x0b0b0d), "terminal");
        assert_eq!(t.borders.subtle, Color::from_u32(0x26262b));
        assert_eq!(t.borders.strong, Color::from_u32(0x34343c));
        assert_eq!(t.text.primary, Color::from_u32(0xe6e6ea));
        // The reference PNG measures pure white on a needs-you row title and
        // 0xe6e6ea on Working and Blocked, so emphasis sits one step above
        // primary rather than replacing it.
        assert_eq!(t.text.emphasis, Color::from_u32(0xffffff));
        assert_eq!(t.text.secondary, Color::from_u32(0x9a9aa3));
        // The spec asks for 0x6b6b74; the AA gate on raised surfaces outranks
        // it, so the nearest passing step on the same ramp is what ships.
        assert_eq!(t.text.muted, Color::from_u32(0x86868f));
        assert_eq!(t.status.attention, Color::from_u32(0xf2b441));
        assert_eq!(t.status.destructive, Color::from_u32(0xe5484d));
        assert_eq!(t.status.success, Color::from_u32(0x7fb07f));
        assert_eq!(t.status.inactive, Color::from_u32(0x86868f));
        // The ramp must descend: primary > secondary > muted > disabled. 0x85858e is the
        // darkest disabled step that still clears 4.5:1 on surfaces.disabled.
        assert_eq!(t.text.disabled, Color::from_u32(0x85858e));
    }

    /// Every background a scrollbar can land on, in one place, so the
    /// contrast assertion below cannot quietly stop looking at one of them.
    fn scrollbar_backgrounds(tokens: ThemeTokens) -> Vec<(&'static str, Color)> {
        vec![
            ("surfaces.canvas", tokens.surfaces.canvas),
            ("surfaces.sunken", tokens.surfaces.sunken),
            ("surfaces.raised", tokens.surfaces.raised),
            ("surfaces.overlay", tokens.surfaces.overlay),
            ("terminal.background", tokens.terminal.background),
        ]
    }

    /// The three `(density, scale)` pairs every projected sweep runs over, each
    /// with a label, so a measurement can say WHICH pair produced it.
    const SWEEP_PAIRS: [(&str, Density, Scale); 3] = [
        (
            "Comfortable/Scale100",
            Density::Comfortable,
            Scale::Scale100,
        ),
        ("Compact/Scale200", Density::Compact, Scale::Scale200),
        (
            "Comfortable/Scale125",
            Density::Comfortable,
            Scale::Scale125,
        ),
    ];

    /// Every shipped palette, in both appearances, at every sweep pair, as the
    /// app ACTUALLY projects it -- which is not what the token module says.
    ///
    /// `ThemePalette::tokens` overwrites `scrollbar.on_dark.thumb_idle` and
    /// `thumb_hover` from the `TerminalScrollbar` / `TerminalScrollbarHover`
    /// roles, whose managed fallback is a mix of the palette's own terminal
    /// background and foreground. So a gate that measures `dark(..)` and
    /// `light(..)` measures three token triples that no user with a theme
    /// selected is looking at. This is the same enumeration the tokens lane's
    /// projected gates use (`tests/ui_tokens.rs`): the built-in library, each
    /// definition's palette per appearance, projected.
    ///
    /// Each entry carries the pair's label, so a caller can key a measurement by
    /// the pair that produced it rather than collapsing three runs into one.
    fn shipped_palette_projections() -> Vec<(String, &'static str, ThemeTokens)> {
        use crate::ui::theme_system::{ThemeAppearance, ThemeLibrary};

        let library = ThemeLibrary::built_in();
        let mut projections = Vec::new();
        for definition in library.themes() {
            for appearance in [ThemeAppearance::Dark, ThemeAppearance::Light] {
                let Some(palette) = definition.palette(appearance) else {
                    continue;
                };
                for (pair, density, scale) in SWEEP_PAIRS {
                    projections.push((
                        format!("{}/{appearance:?}", definition.id),
                        pair,
                        palette.tokens(density, scale),
                    ));
                }
            }
        }
        // The denominator, asserted rather than assumed: a library that stopped
        // yielding palettes would make every assertion below vacuously true.
        assert!(
            projections.len() >= 12,
            "only {} palette projections enumerated -- the library is not answering",
            projections.len()
        );
        projections
    }

    /// The forty-eight (palette, ground, state) rows the SHIPPED palettes fail
    /// today, measured on this branch by the sweep below.
    ///
    /// Six built-in dark palettes -- `ember`, `grove`, `iris`, `ocean`, `t3-chat`
    /// and `t3-code` -- overwrite the scrollbar's two thumb colours from their own
    /// `TerminalScrollbar` / `TerminalScrollbarHover` roles, whose managed fallback
    /// is `terminal_background.mix(terminal_foreground, 0.2)` (and `0.34` for
    /// hover). A twenty-percent mix toward the foreground is not a contrast rule,
    /// so on a dark palette it lands wherever it lands: `t3-chat` idles at
    /// 1.047:1 on its own raised surface, which is an invisible scrollbar.
    ///
    /// This is a pin, not a waiver, and it is not the scrollbar spec's defect:
    /// `devmanager-classic`, the default, clears both floors on every ground in
    /// both appearances, and so does every palette's LIGHT half. The fix is a
    /// contrast rule in `ThemePalette::tokens`' fallback, which belongs to the
    /// tokens owner -- ledgered in
    /// `.superpowers/sdd/2026-09-03-ui-redesign-2-panel-chrome-and-needs-you/lane-5-report.md`.
    /// Repairing it fails this test as loudly as a regression would.
    ///
    /// `(theme/appearance, ground, state, ratio)`. The ratio is the value measured
    /// on this branch and is enforced as a floor, so a pinned defect cannot quietly
    /// absorb a further regression in the same row.
    type ScrollbarDrift = (&'static str, &'static str, &'static str, f64);

    const SHIPPED_PALETTE_SCROLLBAR_DRIFT: &[ScrollbarDrift] = &[
        ("ember/Dark", "surfaces.canvas", "hover", 5.679),
        ("ember/Dark", "surfaces.overlay", "idle", 2.221),
        ("ember/Dark", "surfaces.overlay", "hover", 3.247),
        ("ember/Dark", "surfaces.raised", "idle", 2.710),
        ("ember/Dark", "surfaces.raised", "hover", 3.963),
        ("ember/Dark", "surfaces.sunken", "hover", 4.801),
        ("ember/Dark", "terminal.background", "hover", 5.679),
        ("grove/Dark", "surfaces.canvas", "hover", 5.464),
        ("grove/Dark", "surfaces.overlay", "idle", 2.176),
        ("grove/Dark", "surfaces.overlay", "hover", 3.119),
        ("grove/Dark", "surfaces.raised", "idle", 2.644),
        ("grove/Dark", "surfaces.raised", "hover", 3.790),
        ("grove/Dark", "surfaces.sunken", "hover", 4.625),
        ("grove/Dark", "terminal.background", "hover", 5.464),
        ("iris/Dark", "surfaces.canvas", "hover", 5.848),
        ("iris/Dark", "surfaces.overlay", "idle", 2.268),
        ("iris/Dark", "surfaces.overlay", "hover", 3.330),
        ("iris/Dark", "surfaces.raised", "idle", 2.799),
        ("iris/Dark", "surfaces.raised", "hover", 4.111),
        ("iris/Dark", "surfaces.sunken", "hover", 4.961),
        ("iris/Dark", "terminal.background", "hover", 5.848),
        ("ocean/Dark", "surfaces.canvas", "hover", 5.675),
        ("ocean/Dark", "surfaces.overlay", "idle", 2.215),
        ("ocean/Dark", "surfaces.overlay", "hover", 3.222),
        ("ocean/Dark", "surfaces.raised", "idle", 2.715),
        ("ocean/Dark", "surfaces.raised", "hover", 3.949),
        ("ocean/Dark", "surfaces.sunken", "hover", 4.792),
        ("ocean/Dark", "terminal.background", "hover", 5.675),
        ("t3-chat/Dark", "surfaces.canvas", "idle", 1.108),
        ("t3-chat/Dark", "surfaces.canvas", "hover", 1.561),
        ("t3-chat/Dark", "surfaces.overlay", "idle", 1.273),
        ("t3-chat/Dark", "surfaces.overlay", "hover", 1.794),
        ("t3-chat/Dark", "surfaces.raised", "idle", 1.047),
        ("t3-chat/Dark", "surfaces.raised", "hover", 1.346),
        ("t3-chat/Dark", "surfaces.sunken", "idle", 1.108),
        ("t3-chat/Dark", "surfaces.sunken", "hover", 1.561),
        ("t3-chat/Dark", "terminal.background", "idle", 1.108),
        ("t3-chat/Dark", "terminal.background", "hover", 1.561),
        ("t3-code/Dark", "surfaces.canvas", "idle", 1.849),
        ("t3-code/Dark", "surfaces.canvas", "hover", 3.053),
        ("t3-code/Dark", "surfaces.overlay", "idle", 1.145),
        ("t3-code/Dark", "surfaces.overlay", "hover", 1.890),
        ("t3-code/Dark", "surfaces.raised", "idle", 1.386),
        ("t3-code/Dark", "surfaces.raised", "hover", 2.289),
        ("t3-code/Dark", "surfaces.sunken", "idle", 1.855),
        ("t3-code/Dark", "surfaces.sunken", "hover", 3.062),
        ("t3-code/Dark", "terminal.background", "idle", 1.855),
        ("t3-code/Dark", "terminal.background", "hover", 3.062),
    ];

    /// A pinned ratio may not get worse by more than this. Ratios are pinned to
    /// three decimals, so the tolerance only absorbs that rounding. Same value the
    /// tokens lane's projected gates use.
    const SCROLLBAR_PINNED_RATIO_TOLERANCE: f64 = 0.005;

    /// A 4 px bar is a non-text UI component, so the floor is WCAG 1.4.11's
    /// 3:1 rather than 4.5:1 -- but it has to clear it against every surface a
    /// scrollbar can sit on, in every mode, not just the one that was looked
    /// at while picking the colour.
    ///
    /// Two sweeps, because the app renders two different things. The token
    /// module's own three modes are held HARD: that is the spec this lane
    /// wrote, and nothing may loosen it. Then every SHIPPED palette, projected
    /// the way the app actually renders it -- which overwrites the two thumb
    /// colours from palette roles, so the first sweep cannot see it at all.
    /// The default palette passes that sweep hard as well; six dark palettes do
    /// not, and are held to exactly their measured drift.
    #[test]
    fn scrollbar_thumbs_clear_the_non_text_contrast_floor() {
        let density = Density::Comfortable;
        let scale = Scale::Scale100;
        for (mode, tokens) in [
            (ThemeMode::Dark, dark(density, scale)),
            (ThemeMode::Light, light(density, scale)),
            (ThemeMode::HighContrast, high_contrast(density, scale)),
        ] {
            for (name, background) in scrollbar_backgrounds(tokens) {
                // Resolve through the polarity rule, so this asserts the RULE
                // rather than one triple -- picking the wrong side is exactly
                // the failure it exists to catch.
                let colors = tokens.scrollbar.colors_on(background);
                let idle = contrast_ratio(colors.thumb_idle, background);
                assert!(
                    idle >= 3.0,
                    "{mode:?} idle scrollbar thumb is {idle:.3}:1 on {name}, under the 3:1 floor"
                );
                let hover = contrast_ratio(colors.thumb_hover, background);
                assert!(
                    hover >= 6.0,
                    "{mode:?} hover scrollbar thumb is {hover:.3}:1 on {name}, under the 6:1 floor"
                );
                assert!(
                    hover > idle,
                    "{mode:?} hover scrollbar thumb must read louder than idle on {name}"
                );
                assert_ne!(
                    colors.track_active, colors.thumb_idle,
                    "{mode:?} track and thumb must not be the same colour on {name}"
                );
            }
        }

        // Second sweep: the projections a user with a theme selected is
        // actually looking at.
        let expected = SHIPPED_PALETTE_SCROLLBAR_DRIFT
            .iter()
            .map(|(label, ground, state, _)| (*label, *ground, *state))
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(
            expected.len(),
            SHIPPED_PALETTE_SCROLLBAR_DRIFT.len(),
            "the pinned list repeats a row, so one of its ratios is never checked"
        );

        // Keyed by (palette, ground, PAIR): the pair is in the key because
        // without it the three density/scale runs overwrite one another, and
        // 210 "measurements" would be 70 subjects counted three times. Today
        // the three agree -- `SurfaceTokens` and `ScrollbarTokens` are module
        // constants and the projection only overwrites two thumb colours -- and
        // that agreement is asserted below rather than assumed, so a colour
        // that ever does vary with scale fails loudly instead of silently
        // deciding the pin by whichever pair ran last.
        let mut samples: std::collections::BTreeMap<(String, &str, &str), (f64, f64)> =
            std::collections::BTreeMap::new();
        let mut measured = std::collections::BTreeMap::new();
        for (label, pair, tokens) in shipped_palette_projections() {
            for (name, background) in scrollbar_backgrounds(tokens) {
                let colors = tokens.scrollbar.colors_on(background);
                let idle = contrast_ratio(colors.thumb_idle, background);
                let hover = contrast_ratio(colors.thumb_hover, background);
                assert!(
                    samples
                        .insert((label.clone(), name, pair), (idle, hover))
                        .is_none(),
                    "{label} {name} {pair} measured twice -- the key does not identify a measurement"
                );
                // These two hold for every shipped palette today and are NOT
                // pinned: a thumb that reads louder idle than hovered, or a
                // track the colour of its own thumb, is a broken bar rather
                // than a dim one.
                assert!(
                    hover > idle,
                    "{label}: hover scrollbar thumb must read louder than idle on {name}"
                );
                assert_ne!(
                    colors.track_active, colors.thumb_idle,
                    "{label}: track and thumb must not be the same colour on {name}"
                );

                let is_default = label.starts_with("devmanager-classic");
                if idle < 3.0 {
                    assert!(
                        !is_default,
                        "the DEFAULT palette idles at {idle:.3}:1 on {name} ({label}), under the 3:1 floor -- it may not be pinned"
                    );
                    measured.insert((label.clone(), name, "idle", pair), idle);
                }
                if hover < 6.0 {
                    assert!(
                        !is_default,
                        "the DEFAULT palette hovers at {hover:.3}:1 on {name} ({label}), under the 6:1 floor -- it may not be pinned"
                    );
                    measured.insert((label.clone(), name, "hover", pair), hover);
                }
            }
        }
        // The denominator, asserted rather than assumed, and asserted as
        // DISTINCT keys rather than as a running count: seven built-in themes
        // in two appearances is fourteen palettes, five grounds each, over
        // three density/scale pairs -- 14 * 5 * 3. A count could reach 210 by
        // measuring one subject 210 times; a map's length cannot.
        assert_eq!(
            samples.len(),
            210,
            "the sweep holds {} distinct (palette, ground, pair) measurements, not the 210 it should",
            samples.len()
        );

        // The three pairs agree TODAY, and that is a fact about the token
        // module, not a licence to keep only one of them. Asserted per subject
        // so a scale-dependent colour names itself.
        for ((label, ground, pair), value) in &samples {
            let (first_pair, _, _) = SWEEP_PAIRS[0];
            let reference = samples
                .get(&(label.clone(), *ground, first_pair))
                .expect("every subject is measured at the first pair");
            assert_eq!(
                value, reference,
                "{label} {ground} differs between {first_pair} and {pair}: {reference:?} vs \
                 {value:?} -- the pinned ratios below no longer describe one measurement"
            );
        }

        let measured_rows = measured
            .keys()
            .map(|(label, ground, state, _)| (label.as_str(), *ground, *state))
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(
            measured_rows,
            expected,
            "the shipped-palette scrollbar drift changed -- a repair must shrink the pinned list, and a regression must not be absorbed by it"
        );

        for (label, ground, state, pinned) in SHIPPED_PALETTE_SCROLLBAR_DRIFT {
            // The WORST of the three pairs, not whichever one iterated last.
            // They agree today -- asserted above -- so this is the same number;
            // it stays a minimum so that if they ever diverge the pin is held
            // to the worse reading rather than the luckier one.
            let live = measured
                .iter()
                .filter(
                    |((measured_label, measured_ground, measured_state, _), _)| {
                        measured_label == label
                            && measured_ground == ground
                            && measured_state == state
                    },
                )
                .map(|(_, ratio)| *ratio)
                .fold(f64::INFINITY, f64::min);
            assert!(
                live.is_finite(),
                "{label} {ground} {state} missing from the measured set"
            );
            assert!(
                live >= *pinned - SCROLLBAR_PINNED_RATIO_TOLERANCE,
                "{label} {ground} {state} fell from {pinned:.3}:1 to {live:.3}:1 -- a pinned \
                 defect got worse"
            );
        }
    }

    /// The polarity rule has to actually switch, or the two triples are one
    /// triple with a decoration. The light theme is the case that matters:
    /// its shell is near-white and its terminal is a dark island.
    #[test]
    fn scrollbar_colours_follow_the_ground_they_are_painted_on() {
        let tokens = light(Density::Comfortable, Scale::Scale100);
        let shell = tokens.scrollbar.colors_on(tokens.surfaces.canvas);
        let terminal = tokens.scrollbar.colors_on(tokens.terminal.background);
        assert_eq!(shell, tokens.scrollbar.on_light);
        assert_eq!(terminal, tokens.scrollbar.on_dark);
        assert_ne!(shell, terminal);

        let tokens = dark(Density::Comfortable, Scale::Scale100);
        assert_eq!(
            tokens.scrollbar.colors_on(tokens.surfaces.canvas),
            tokens.scrollbar.on_dark
        );
        assert_eq!(
            tokens.scrollbar.colors_on(tokens.terminal.background),
            tokens.scrollbar.on_dark
        );

        // High contrast has one polarity, so both sides answer the same.
        let tokens = high_contrast(Density::Comfortable, Scale::Scale100);
        assert_eq!(tokens.scrollbar.on_dark, tokens.scrollbar.on_light);
    }

    /// The idle and active states must actually differ, and the gutter must be
    /// wide enough to hold the active thumb -- a spec where they agree paints
    /// a scrollbar that never responds to the pointer.
    #[test]
    fn scrollbar_geometry_expands_on_hover_inside_a_fixed_gutter() {
        for tokens in [
            dark(Density::Comfortable, Scale::Scale100),
            light(Density::Compact, Scale::Scale200),
            high_contrast(Density::Comfortable, Scale::Scale125),
        ] {
            let bar = tokens.scrollbar;
            assert!(
                bar.active_thumb_width > bar.idle_thumb_width,
                "hover must widen the thumb"
            );
            assert!(
                bar.gutter_width >= bar.active_thumb_width,
                "the gutter must hold the active thumb without reflowing its neighbour"
            );
            assert!(bar.min_thumb_length > 0.0);
            assert_eq!(bar.thumb_width(false), bar.idle_thumb_width);
            assert_eq!(bar.thumb_width(true), bar.active_thumb_width);
            assert_eq!(bar.on_dark.thumb(false), bar.on_dark.thumb_idle);
            assert_eq!(bar.on_dark.thumb(true), bar.on_dark.thumb_hover);
            // A pill at both widths, from one ratio.
            assert_eq!(bar.thumb_radius(false), bar.idle_thumb_width / 2.0);
            assert_eq!(bar.thumb_radius(true), bar.active_thumb_width / 2.0);
        }
    }

    /// The redesign spec pins these two numbers by name. Anything else on the
    /// screen is a divergence from the ruling, not a taste difference.
    #[test]
    fn scrollbar_widths_are_the_ruled_four_and_ten() {
        let bar = dark(Density::Comfortable, Scale::Scale100).scrollbar;
        assert_eq!(bar.idle_thumb_width, 4.0);
        assert_eq!(bar.active_thumb_width, 10.0);

        // A pointer target does not change size with the palette, so the whole
        // spec -- geometry AND both colour triples -- is mode-independent. This
        // is what lets `terminal_scrollbar_spec()` read the dark theme without
        // that being a theme choice.
        let light = light(Density::Comfortable, Scale::Scale100);
        assert_eq!(
            dark(Density::Comfortable, Scale::Scale100).scrollbar,
            light.scrollbar
        );
        assert_eq!(
            high_contrast(Density::Comfortable, Scale::Scale100)
                .scrollbar
                .gutter_width,
            light.scrollbar.gutter_width
        );
    }

    /// The board's project stripe reads its hue from here. Pinned so a
    /// reordered palette is a failing test rather than every project on the
    /// board silently changing colour.
    #[test]
    fn project_palette_is_the_spec_in_assignment_order() {
        assert_eq!(PROJECT_PALETTE.len(), 8);
        assert_eq!(PROJECT_PALETTE[0], Color::from_u32(0x5aa3a0));
        assert_eq!(PROJECT_PALETTE[1], Color::from_u32(0x7a86c4));
        assert_eq!(PROJECT_PALETTE[7], Color::from_u32(0xa8806f));
    }
}
