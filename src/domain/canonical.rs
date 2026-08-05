//! Shared helpers for durable identity, label, and recipe strings.
//!
//! Constructors and deserializers produce trimmed canonical values (or reject).
//! Validation of forged facts requires values that are already canonical.
//! Do not use these helpers for InlineUtf8 artifact bodies or arbitrary workspace paths.

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
