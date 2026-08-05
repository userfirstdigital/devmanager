use std::io::{ErrorKind, Read, Write};

use serde::de::{self, Deserializer};
use serde::ser::{self, Serializer};
use serde::{Deserialize, Serialize};

use crate::domain::snapshot::{MAX_SNAPSHOT_PAGE_ENCODED_BYTES, MAX_SNAPSHOT_PAGE_ITEMS};

pub const MAX_PHYSICAL_FRAME_BYTES: u32 = 1024 * 1024;
pub const MAX_REASSEMBLED_MESSAGE_BYTES: u32 = 16 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameLimitField {
    PhysicalFrameBytes,
    ReassembledMessageBytes,
    PageItems,
    PageEncodedBytes,
}

impl std::fmt::Display for FrameLimitField {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::PhysicalFrameBytes => write!(f, "max_physical_frame_bytes"),
            Self::ReassembledMessageBytes => write!(f, "max_reassembled_message_bytes"),
            Self::PageItems => write!(f, "max_page_items"),
            Self::PageEncodedBytes => write!(f, "max_page_encoded_bytes"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameLimitsError {
    Zero { field: FrameLimitField },
}

impl std::fmt::Display for FrameLimitsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Zero { field } => write!(f, "protocol frame limit {field} must be nonzero"),
        }
    }
}

impl std::error::Error for FrameLimitsError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PhysicalFrameError {
    Empty,
    Oversized { declared: u64, maximum: u32 },
    Allocation { declared: u32 },
    ReadHeader { kind: ErrorKind },
    ReadPayload { declared: u32, kind: ErrorKind },
    WriteHeader { kind: ErrorKind },
    WritePayload { declared: u32, kind: ErrorKind },
}

impl std::fmt::Display for PhysicalFrameError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Empty => write!(f, "protocol frame length must be nonzero"),
            Self::Oversized { declared, maximum } => write!(
                f,
                "protocol frame length {declared} exceeds negotiated maximum {maximum}"
            ),
            Self::Allocation { declared } => write!(
                f,
                "protocol frame payload allocation failed for {declared} bytes"
            ),
            Self::ReadHeader { kind } => {
                write!(f, "protocol frame header read failed: {kind}")
            }
            Self::ReadPayload { declared, kind } => write!(
                f,
                "protocol frame payload read failed for {declared} bytes: {kind}"
            ),
            Self::WriteHeader { kind } => {
                write!(f, "protocol frame header write failed: {kind}")
            }
            Self::WritePayload { declared, kind } => write!(
                f,
                "protocol frame payload write failed for {declared} bytes: {kind}"
            ),
        }
    }
}

impl std::error::Error for PhysicalFrameError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameLimits {
    pub max_physical_frame_bytes: u32,
    pub max_reassembled_message_bytes: u32,
    pub max_page_items: u32,
    pub max_page_encoded_bytes: u32,
}

impl FrameLimits {
    pub const fn v1_default() -> Self {
        Self {
            max_physical_frame_bytes: MAX_PHYSICAL_FRAME_BYTES,
            max_reassembled_message_bytes: MAX_REASSEMBLED_MESSAGE_BYTES,
            max_page_items: MAX_SNAPSHOT_PAGE_ITEMS,
            max_page_encoded_bytes: MAX_SNAPSHOT_PAGE_ENCODED_BYTES,
        }
    }

    pub fn validate_offer(self) -> Result<(), FrameLimitsError> {
        for (field, value) in [
            (
                FrameLimitField::PhysicalFrameBytes,
                self.max_physical_frame_bytes,
            ),
            (
                FrameLimitField::ReassembledMessageBytes,
                self.max_reassembled_message_bytes,
            ),
            (FrameLimitField::PageItems, self.max_page_items),
            (
                FrameLimitField::PageEncodedBytes,
                self.max_page_encoded_bytes,
            ),
        ] {
            if value == 0 {
                return Err(FrameLimitsError::Zero { field });
            }
        }
        Ok(())
    }

    pub fn negotiate(self, peer: Self) -> Result<Self, FrameLimitsError> {
        self.validate_offer()?;
        peer.validate_offer()?;
        Ok(Self {
            max_physical_frame_bytes: self
                .max_physical_frame_bytes
                .min(peer.max_physical_frame_bytes)
                .min(MAX_PHYSICAL_FRAME_BYTES),
            max_reassembled_message_bytes: self
                .max_reassembled_message_bytes
                .min(peer.max_reassembled_message_bytes)
                .min(MAX_REASSEMBLED_MESSAGE_BYTES),
            max_page_items: self
                .max_page_items
                .min(peer.max_page_items)
                .min(MAX_SNAPSHOT_PAGE_ITEMS),
            max_page_encoded_bytes: self
                .max_page_encoded_bytes
                .min(peer.max_page_encoded_bytes)
                .min(MAX_SNAPSHOT_PAGE_ENCODED_BYTES),
        })
    }
}

impl Serialize for FrameLimits {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.validate_offer().map_err(ser::Error::custom)?;
        #[derive(Serialize)]
        struct FrameLimitsWire {
            max_physical_frame_bytes: u32,
            max_reassembled_message_bytes: u32,
            max_page_items: u32,
            max_page_encoded_bytes: u32,
        }
        FrameLimitsWire {
            max_physical_frame_bytes: self.max_physical_frame_bytes,
            max_reassembled_message_bytes: self.max_reassembled_message_bytes,
            max_page_items: self.max_page_items,
            max_page_encoded_bytes: self.max_page_encoded_bytes,
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for FrameLimits {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct FrameLimitsWire {
            max_physical_frame_bytes: u32,
            max_reassembled_message_bytes: u32,
            max_page_items: u32,
            max_page_encoded_bytes: u32,
        }
        let wire = FrameLimitsWire::deserialize(deserializer)?;
        let limits = Self {
            max_physical_frame_bytes: wire.max_physical_frame_bytes,
            max_reassembled_message_bytes: wire.max_reassembled_message_bytes,
            max_page_items: wire.max_page_items,
            max_page_encoded_bytes: wire.max_page_encoded_bytes,
        };
        limits.validate_offer().map_err(de::Error::custom)?;
        Ok(limits)
    }
}

/// Immutable physical-frame boundary for one negotiated connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PhysicalFrameCodec {
    max_payload_bytes: u32,
}

impl PhysicalFrameCodec {
    pub fn from_limits(limits: FrameLimits) -> Result<Self, FrameLimitsError> {
        limits.validate_offer()?;
        Ok(Self {
            max_payload_bytes: limits
                .max_physical_frame_bytes
                .min(MAX_PHYSICAL_FRAME_BYTES),
        })
    }

    pub const fn max_payload_bytes(self) -> u32 {
        self.max_payload_bytes
    }

    fn validate_declared(self, declared: u32) -> Result<usize, PhysicalFrameError> {
        if declared == 0 {
            return Err(PhysicalFrameError::Empty);
        }
        if declared > self.max_payload_bytes {
            return Err(PhysicalFrameError::Oversized {
                declared: u64::from(declared),
                maximum: self.max_payload_bytes,
            });
        }
        usize::try_from(declared).map_err(|_| PhysicalFrameError::Oversized {
            declared: u64::from(declared),
            maximum: self.max_payload_bytes,
        })
    }

    fn declared_for_payload(self, payload: &[u8]) -> Result<u32, PhysicalFrameError> {
        let declared = u64::try_from(payload.len()).unwrap_or(u64::MAX);
        if declared == 0 {
            return Err(PhysicalFrameError::Empty);
        }
        if declared > u64::from(self.max_payload_bytes) {
            return Err(PhysicalFrameError::Oversized {
                declared,
                maximum: self.max_payload_bytes,
            });
        }
        u32::try_from(declared).map_err(|_| PhysicalFrameError::Oversized {
            declared,
            maximum: self.max_payload_bytes,
        })
    }

    /// Read exactly one length-prefixed physical frame.
    ///
    /// The four-byte header is fully validated before payload bytes are read
    /// or storage is reserved. Invalid input is connection-fatal; this method
    /// never drains or attempts to resynchronize. Additional coalesced frames
    /// remain in `reader` for the next call.
    pub fn read<R: Read + ?Sized>(&self, reader: &mut R) -> Result<Vec<u8>, PhysicalFrameError> {
        let mut header = [0_u8; 4];
        reader
            .read_exact(&mut header)
            .map_err(|error| PhysicalFrameError::ReadHeader { kind: error.kind() })?;
        let declared = u32::from_be_bytes(header);
        let payload_len = self.validate_declared(declared)?;

        let mut payload = Vec::new();
        payload
            .try_reserve_exact(payload_len)
            .map_err(|_| PhysicalFrameError::Allocation { declared })?;
        payload.resize(payload_len, 0);
        reader
            .read_exact(&mut payload)
            .map_err(|error| PhysicalFrameError::ReadPayload {
                declared,
                kind: error.kind(),
            })?;
        Ok(payload)
    }

    /// Write exactly one nonempty length-prefixed physical frame.
    ///
    /// I/O failure can leave a partial frame in the stream and is therefore
    /// connection-fatal. This method deliberately does not flush the writer.
    pub fn write<W: Write + ?Sized>(
        &self,
        writer: &mut W,
        payload: &[u8],
    ) -> Result<(), PhysicalFrameError> {
        let declared = self.declared_for_payload(payload)?;
        writer
            .write_all(&declared.to_be_bytes())
            .map_err(|error| PhysicalFrameError::WriteHeader { kind: error.kind() })?;
        writer
            .write_all(payload)
            .map_err(|error| PhysicalFrameError::WritePayload {
                declared,
                kind: error.kind(),
            })?;
        Ok(())
    }
}
