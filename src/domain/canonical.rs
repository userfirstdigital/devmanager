//! Shared helpers for durable identity, label, and recipe strings.
//!
//! Constructors and deserializers produce trimmed canonical values (or reject).
//! Validation of forged facts requires values that are already canonical.
//! Do not use these helpers for InlineUtf8 artifact bodies or arbitrary workspace paths.

/// Maximum UTF-8 bytes for specialist purpose and result text.
pub const MAX_SPECIALIST_TEXT_BYTES: usize = 256;

/// Maximum artifact/resource id refs on one specialist request or result.
pub const MAX_SPECIALIST_ID_REFS: usize = 16;

/// Trim surrounding whitespace and reject blank results.
pub fn canonicalize(value: impl AsRef<str>) -> Option<String> {
    let trimmed = value.as_ref().trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// True when `value` is non-empty and already trimmed (no surrounding whitespace).
pub fn is_canonical(value: &str) -> bool {
    !value.is_empty() && value == value.trim()
}

/// Trim and accept only non-empty text within [`MAX_SPECIALIST_TEXT_BYTES`].
/// Length is inspected on the trimmed view before any allocation.
pub fn bounded_canonical(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() || trimmed.len() > MAX_SPECIALIST_TEXT_BYTES {
        None
    } else {
        Some(trimmed.to_string())
    }
}

pub fn is_bounded_canonical(value: &str) -> bool {
    is_canonical(value) && value.len() <= MAX_SPECIALIST_TEXT_BYTES
}

pub fn optional_bounded_canonical(value: Option<&str>) -> bool {
    match value {
        None => true,
        Some(text) => is_bounded_canonical(text),
    }
}

pub fn specialist_id_refs_ok(count: usize) -> bool {
    count <= MAX_SPECIALIST_ID_REFS
}
