//! Compatibility surface for legacy callers.
//!
//! New UI code should import `crate::ui::tokens`. The base branch does not
//! wire `src/ui` into `lib.rs` until the Phase 5.1 foundation lands, so this
//! adapter includes the same token source here while preserving the existing
//! legacy names for the current application.

#[allow(dead_code)]
#[path = "../ui/tokens.rs"]
mod token_source;

pub use token_source::legacy::*;
pub use token_source::{
    contrast_ratio, parse_hex_color, srgb_luminance, BorderTokens, Color, ContrastPair, Density,
    DensityMetrics, IconTokens, MotionTokens, PhysicalDensityMetrics, RadiiTokens, Scale,
    SemanticColorToken, SpacingTokens, StatusMeaning, StatusTokens, SurfaceTokens, TerminalPalette,
    TextTokens, Theme, ThemeMode, ThemeTokens, TypographyTokens, WindowsScale,
};
