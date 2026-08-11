//! Compatibility surface for legacy callers.
//!
//! New native cockpit code imports [`crate::ui::tokens`]. Keeping this module
//! as a pure re-export ensures the legacy GPUI surface and native shell share
//! one canonical token source without a shadow palette.

pub use crate::ui::tokens::*;
