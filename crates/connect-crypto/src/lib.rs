//! The browser Connect leaf.
//!
//! The protocol implementation is shared with the native server source by
//! path, so the browser cannot accidentally grow a JavaScript crypto dialect.
//! The optional WASM facade exposes only bounded byte-oriented operations and
//! deliberately maps failures to redacted errors.

pub const PROTOCOL_MAJOR: u16 = 1;

pub mod frame {
    /// Native Connect's physical websocket frame limit.
    pub const MAX_PHYSICAL_FRAME_BYTES: u32 = 1024 * 1024;
}

#[path = "../../../src/protocol/crypto.rs"]
mod native_crypto;

pub use native_crypto::*;

#[cfg(feature = "wasm")]
pub mod wasm;

/// The bounded Connect v1 envelope ABI used by the browser transport.
///
/// Keep this behind the WASM feature: the native application continues to use
/// `src/connect/envelope.rs` as its source of truth, while the browser gets a
/// deliberately small, redacted JSON-to-MessagePack boundary.
#[cfg(feature = "wasm")]
pub mod wire;
