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
