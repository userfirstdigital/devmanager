// Backgrounds — zinc scale
pub const APP_BG: u32 = 0x18181b; // zinc-900
pub const SIDEBAR_BG: u32 = 0x27272a; // zinc-800
pub const PANEL_BG: u32 = 0x18181b; // zinc-900
pub const PANEL_HEADER_BG: u32 = 0x27272a; // zinc-800
pub const PANEL_CARD_BG: u32 = 0x18181b; // zinc-900
pub const TOPBAR_BG: u32 = 0x27272a; // zinc-800
pub const TAB_BAR_BG: u32 = 0x27272a; // zinc-800
pub const TAB_ACTIVE_BG: u32 = 0x18181b; // zinc-900
pub const TAB_HOVER_BG: u32 = 0x323238; // zinc-700/30 approx
pub const STATUS_BAR_BG: u32 = 0x09090b; // zinc-950
pub const TERMINAL_BG: u32 = 0x09090b; // zinc-950 — matches terminal default background

// Row backgrounds
pub const PROJECT_ROW_BG: u32 = 0x3f3f46; // zinc-700
pub const AGENT_ROW_BG: u32 = 0x27272a; // zinc-800

// Borders
pub const BORDER_PRIMARY: u32 = 0x3f3f46; // zinc-700
pub const BORDER_SECONDARY: u32 = 0x27272a; // zinc-800
pub const BORDER_ACCENT: u32 = 0x243040;

// Text — already matches zinc scale
pub const TEXT_PRIMARY: u32 = 0xe4e4e7; // zinc-200
pub const TEXT_MUTED: u32 = 0xa1a1aa; // zinc-400
pub const TEXT_SUBTLE: u32 = 0x71717a; // zinc-500
pub const TEXT_DIM: u32 = 0x52525b; // zinc-600

pub const SELECTION_BG: u32 = 0x22364d;
pub const SELECTION_TEXT: u32 = 0xf8fafc;
pub const PROJECT_DOT: u32 = 0x6366f1;
pub const AI_DOT: u32 = 0xa855f7;
pub const SSH_DOT: u32 = 0x06b6d4;
pub const SUCCESS_BG: u32 = 0x142117;
pub const SUCCESS_TEXT: u32 = 0x4ade80;
pub const WARNING_TEXT: u32 = 0xfacc15;
pub const DANGER_TEXT: u32 = 0xfb7185;

// Primary action color
pub const PRIMARY: u32 = 0x4f46e5; // indigo-600 — primary action
pub const PRIMARY_HOVER: u32 = 0x4338ca; // indigo-700 — primary hover

// Hover variants
pub const ROW_HOVER_BG: u32 = 0x323238; // zinc-700/30 approximation
pub const BUTTON_HOVER_BG: u32 = 0x52525b; // zinc-600

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
