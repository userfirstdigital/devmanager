pub const APP_BG: u32 = 0x101114;
pub const SIDEBAR_BG: u32 = 0x191b20;
pub const PANEL_BG: u32 = 0x090a0c;
pub const PANEL_HEADER_BG: u32 = 0x15171c;
pub const PANEL_CARD_BG: u32 = 0x0b0d10;
pub const TOPBAR_BG: u32 = 0x17191e;
pub const TAB_BAR_BG: u32 = 0x17191e;
pub const TAB_ACTIVE_BG: u32 = 0x0b0c10;
pub const TAB_HOVER_BG: u32 = 0x1f2229;
pub const STATUS_BAR_BG: u32 = 0x0a0b0d;
pub const PROJECT_ROW_BG: u32 = 0x1e2128;
pub const AGENT_ROW_BG: u32 = 0x15181f;
pub const BORDER_PRIMARY: u32 = 0x2b2f37;
pub const BORDER_SECONDARY: u32 = 0x20242c;
pub const BORDER_ACCENT: u32 = 0x243040;
pub const TEXT_PRIMARY: u32 = 0xe4e4e7;
pub const TEXT_MUTED: u32 = 0xa1a1aa;
pub const TEXT_SUBTLE: u32 = 0x71717a;
pub const TEXT_DIM: u32 = 0x52525b;
pub const SELECTION_BG: u32 = 0x22364d;
pub const SELECTION_TEXT: u32 = 0xf8fafc;
pub const PROJECT_DOT: u32 = 0x6366f1;
pub const AI_DOT: u32 = 0xa855f7;
pub const SSH_DOT: u32 = 0x06b6d4;
pub const SUCCESS_BG: u32 = 0x142117;
pub const SUCCESS_TEXT: u32 = 0x4ade80;
pub const WARNING_TEXT: u32 = 0xfacc15;
pub const DANGER_TEXT: u32 = 0xfb7185;

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
