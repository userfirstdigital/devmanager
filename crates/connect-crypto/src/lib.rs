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
mod wasm;
