//! Native composer derivation: closed segments and trigger menus.

pub mod segments;
pub mod trigger;

pub use segments::{ComposerCursor, PromptDocument, PromptSegment};
pub use trigger::{
    apply_suggestion, detect_trigger, filter_suggestions, ActiveTrigger, TriggerKind,
    TriggerMenuState, TriggerSuggestion,
};

/// Honest composer placeholder. `$` skills stay HOLD until a typed native skill
/// projection exists; `@` and `/` are backed by FilesList and the Rust catalog.
pub const NATIVE_COMPOSER_PLACEHOLDER: &str = "Ask anything, @ files/folders, or / for commands";

/// Explicit HOLD: native `$` skill menus require a host-emitted skill catalog.
/// Until that typed authority exists, do not advertise `$use skills`.
pub const SKILL_TRIGGER_HOLD: &str =
    "HOLD: $ skill menu deferred — no typed native skill projection/authority yet";
