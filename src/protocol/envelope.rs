use std::io::Cursor;

use rmp::Marker;
use serde::de::DeserializeOwned;
use serde::Serialize;

use super::frame::MAX_PHYSICAL_FRAME_BYTES;
use super::{FrameLimits, FrameLimitsError};

pub const MAX_MESSAGEPACK_DEPTH: u16 = 32;
pub const MAX_MESSAGEPACK_COLLECTION_ITEMS: u32 = 1_000;
pub const MAX_MESSAGEPACK_VALUES: u32 = 65_536;
const MESSAGEPACK_STACK_SLOTS: usize = MAX_MESSAGEPACK_DEPTH as usize + 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessagePackLengthKind {
    Array,
    Map,
    String,
    Binary,
}

impl std::fmt::Display for MessagePackLengthKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Array => write!(f, "array"),
            Self::Map => write!(f, "map"),
            Self::String => write!(f, "string"),
            Self::Binary => write!(f, "binary"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessagePackError {
    Empty,
    Oversized {
        declared: u64,
        maximum: u32,
    },
    DepthExceeded {
        maximum: u16,
    },
    DeclaredLengthExceeded {
        kind: MessagePackLengthKind,
        declared: u32,
        maximum: u32,
    },
    ValueCountExceeded {
        maximum: u32,
    },
    UnsupportedExtension {
        offset: u32,
    },
    ReservedMarker {
        offset: u32,
    },
    Truncated {
        offset: u32,
    },
    TrailingBytes {
        offset: u32,
    },
    Encode,
    Decode,
}

impl std::fmt::Display for MessagePackError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Empty => write!(f, "MessagePack document must be nonempty"),
            Self::Oversized { declared, maximum } => write!(
                f,
                "MessagePack document length {declared} exceeds maximum {maximum}"
            ),
            Self::DepthExceeded { maximum } => {
                write!(f, "MessagePack nesting exceeds maximum depth {maximum}")
            }
            Self::DeclaredLengthExceeded {
                kind,
                declared,
                maximum,
            } => write!(
                f,
                "MessagePack {kind} length {declared} exceeds maximum {maximum}"
            ),
            Self::ValueCountExceeded { maximum } => {
                write!(f, "MessagePack value count exceeds maximum {maximum}")
            }
            Self::UnsupportedExtension { offset } => {
                write!(
                    f,
                    "MessagePack extension marker at byte {offset} is unsupported"
                )
            }
            Self::ReservedMarker { offset } => {
                write!(f, "reserved MessagePack marker at byte {offset}")
            }
            Self::Truncated { offset } => {
                write!(f, "truncated MessagePack document at byte {offset}")
            }
            Self::TrailingBytes { offset } => {
                write!(f, "trailing MessagePack bytes begin at byte {offset}")
            }
            Self::Encode => write!(f, "MessagePack encoding failed"),
            Self::Decode => write!(f, "MessagePack decoding failed"),
        }
    }
}

impl std::error::Error for MessagePackError {}

/// Immutable MessagePack safety boundary for one negotiated connection.
///
/// Incoming bytes are structurally preflighted without allocation before
/// Serde can observe collection or scalar length declarations. Any violation
/// is connection-fatal; callers must not attempt to resynchronize within the
/// same physical frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MessagePackCodec {
    max_document_bytes: u32,
}

impl MessagePackCodec {
    pub fn from_limits(limits: FrameLimits) -> Result<Self, FrameLimitsError> {
        limits.validate_offer()?;
        Ok(Self {
            max_document_bytes: limits
                .max_physical_frame_bytes
                .min(MAX_PHYSICAL_FRAME_BYTES),
        })
    }

    pub const fn max_document_bytes(self) -> u32 {
        self.max_document_bytes
    }

    pub fn encode<T: Serialize + ?Sized>(&self, value: &T) -> Result<Vec<u8>, MessagePackError> {
        let encoded = rmp_serde::to_vec_named(value).map_err(|_| MessagePackError::Encode)?;
        self.preflight(&encoded)?;
        Ok(encoded)
    }

    pub fn decode<T: DeserializeOwned>(&self, payload: &[u8]) -> Result<T, MessagePackError> {
        self.preflight(payload)?;

        let mut deserializer = rmp_serde::Deserializer::new(Cursor::new(payload));
        // rmp-serde rejects when its counter reaches zero, so N admitted
        // containers require an initial counter of N + 1.
        deserializer.set_max_depth(usize::from(MAX_MESSAGEPACK_DEPTH) + 1);
        let value = serde::Deserialize::deserialize(&mut deserializer)
            .map_err(|_| MessagePackError::Decode)?;
        let position = u32::try_from(deserializer.position()).unwrap_or(u32::MAX);
        if position != u32::try_from(payload.len()).unwrap_or(u32::MAX) {
            return Err(MessagePackError::TrailingBytes { offset: position });
        }
        Ok(value)
    }

    fn preflight(self, payload: &[u8]) -> Result<(), MessagePackError> {
        let declared = u64::try_from(payload.len()).unwrap_or(u64::MAX);
        if declared == 0 {
            return Err(MessagePackError::Empty);
        }
        if declared > u64::from(self.max_document_bytes) {
            return Err(MessagePackError::Oversized {
                declared,
                maximum: self.max_document_bytes,
            });
        }

        MessagePackScanner {
            payload,
            offset: 0,
            values: 0,
            max_scalar_bytes: self.max_document_bytes,
        }
        .scan_document()
    }
}

struct MessagePackScanner<'a> {
    payload: &'a [u8],
    offset: usize,
    values: u32,
    max_scalar_bytes: u32,
}

impl MessagePackScanner<'_> {
    fn scan_document(mut self) -> Result<(), MessagePackError> {
        let mut remaining = [0_u32; MESSAGEPACK_STACK_SLOTS];
        let mut depth = 0_usize;
        remaining[0] = 1;

        loop {
            if remaining[depth] == 0 {
                if depth == 0 {
                    break;
                }
                depth -= 1;
                continue;
            }

            remaining[depth] -= 1;
            let current_depth = u16::try_from(depth).unwrap_or(u16::MAX);
            let children = self.scan_one_value(current_depth)?;
            if children != 0 {
                if depth >= usize::from(MAX_MESSAGEPACK_DEPTH) {
                    return Err(MessagePackError::DepthExceeded {
                        maximum: MAX_MESSAGEPACK_DEPTH,
                    });
                }
                depth += 1;
                remaining[depth] = children;
            }
        }

        if self.offset != self.payload.len() {
            return Err(MessagePackError::TrailingBytes {
                offset: self.offset_u32(),
            });
        }
        Ok(())
    }

    fn scan_one_value(&mut self, depth: u16) -> Result<u32, MessagePackError> {
        self.values = self
            .values
            .checked_add(1)
            .ok_or(MessagePackError::ValueCountExceeded {
                maximum: MAX_MESSAGEPACK_VALUES,
            })?;
        if self.values > MAX_MESSAGEPACK_VALUES {
            return Err(MessagePackError::ValueCountExceeded {
                maximum: MAX_MESSAGEPACK_VALUES,
            });
        }

        let marker_offset = self.offset_u32();
        let marker = Marker::from_u8(self.read_u8()?);
        match marker {
            Marker::FixPos(_) | Marker::FixNeg(_) | Marker::Null | Marker::False | Marker::True => {
                Ok(0)
            }
            Marker::Reserved => Err(MessagePackError::ReservedMarker {
                offset: marker_offset,
            }),
            Marker::FixExt1
            | Marker::FixExt2
            | Marker::FixExt4
            | Marker::FixExt8
            | Marker::FixExt16
            | Marker::Ext8
            | Marker::Ext16
            | Marker::Ext32 => Err(MessagePackError::UnsupportedExtension {
                offset: marker_offset,
            }),
            Marker::F32 | Marker::U32 | Marker::I32 => self.skip(4).map(|_| 0),
            Marker::F64 | Marker::U64 | Marker::I64 => self.skip(8).map(|_| 0),
            Marker::U8 | Marker::I8 => self.skip(1).map(|_| 0),
            Marker::U16 | Marker::I16 => self.skip(2).map(|_| 0),
            Marker::FixStr(length) => self
                .scan_scalar(MessagePackLengthKind::String, u32::from(length))
                .map(|_| 0),
            Marker::Str8 => {
                let length = u32::from(self.read_u8()?);
                self.scan_scalar(MessagePackLengthKind::String, length)
                    .map(|_| 0)
            }
            Marker::Str16 => {
                let length = u32::from(self.read_u16()?);
                self.scan_scalar(MessagePackLengthKind::String, length)
                    .map(|_| 0)
            }
            Marker::Str32 => {
                let length = self.read_u32()?;
                self.scan_scalar(MessagePackLengthKind::String, length)
                    .map(|_| 0)
            }
            Marker::Bin8 => {
                let length = u32::from(self.read_u8()?);
                self.scan_scalar(MessagePackLengthKind::Binary, length)
                    .map(|_| 0)
            }
            Marker::Bin16 => {
                let length = u32::from(self.read_u16()?);
                self.scan_scalar(MessagePackLengthKind::Binary, length)
                    .map(|_| 0)
            }
            Marker::Bin32 => {
                let length = self.read_u32()?;
                self.scan_scalar(MessagePackLengthKind::Binary, length)
                    .map(|_| 0)
            }
            Marker::FixArray(length) => {
                self.collection_children(MessagePackLengthKind::Array, u32::from(length), depth)
            }
            Marker::Array16 => {
                let length = u32::from(self.read_u16()?);
                self.collection_children(MessagePackLengthKind::Array, length, depth)
            }
            Marker::Array32 => {
                let length = self.read_u32()?;
                self.collection_children(MessagePackLengthKind::Array, length, depth)
            }
            Marker::FixMap(length) => self.map_children(u32::from(length), depth),
            Marker::Map16 => {
                let length = u32::from(self.read_u16()?);
                self.map_children(length, depth)
            }
            Marker::Map32 => {
                let length = self.read_u32()?;
                self.map_children(length, depth)
            }
        }
    }

    fn collection_children(
        &self,
        kind: MessagePackLengthKind,
        length: u32,
        depth: u16,
    ) -> Result<u32, MessagePackError> {
        self.check_container_depth(depth)?;
        self.check_collection(kind, length)?;
        Ok(length)
    }

    fn map_children(&self, length: u32, depth: u16) -> Result<u32, MessagePackError> {
        self.check_container_depth(depth)?;
        self.check_collection(MessagePackLengthKind::Map, length)?;
        length
            .checked_mul(2)
            .ok_or(MessagePackError::DeclaredLengthExceeded {
                kind: MessagePackLengthKind::Map,
                declared: length,
                maximum: MAX_MESSAGEPACK_COLLECTION_ITEMS,
            })
    }

    fn check_container_depth(&self, depth: u16) -> Result<(), MessagePackError> {
        if depth >= MAX_MESSAGEPACK_DEPTH {
            return Err(MessagePackError::DepthExceeded {
                maximum: MAX_MESSAGEPACK_DEPTH,
            });
        }
        Ok(())
    }

    fn check_collection(
        &self,
        kind: MessagePackLengthKind,
        length: u32,
    ) -> Result<(), MessagePackError> {
        if length > MAX_MESSAGEPACK_COLLECTION_ITEMS {
            return Err(MessagePackError::DeclaredLengthExceeded {
                kind,
                declared: length,
                maximum: MAX_MESSAGEPACK_COLLECTION_ITEMS,
            });
        }
        Ok(())
    }

    fn scan_scalar(
        &mut self,
        kind: MessagePackLengthKind,
        length: u32,
    ) -> Result<(), MessagePackError> {
        if length > self.max_scalar_bytes {
            return Err(MessagePackError::DeclaredLengthExceeded {
                kind,
                declared: length,
                maximum: self.max_scalar_bytes,
            });
        }
        let length = usize::try_from(length).map_err(|_| MessagePackError::Truncated {
            offset: self.offset_u32(),
        })?;
        self.skip(length)
    }

    fn read_u8(&mut self) -> Result<u8, MessagePackError> {
        let bytes = self.take(1)?;
        Ok(bytes[0])
    }

    fn read_u16(&mut self) -> Result<u16, MessagePackError> {
        let bytes = self.take(2)?;
        Ok(u16::from_be_bytes([bytes[0], bytes[1]]))
    }

    fn read_u32(&mut self) -> Result<u32, MessagePackError> {
        let bytes = self.take(4)?;
        Ok(u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    fn skip(&mut self, length: usize) -> Result<(), MessagePackError> {
        self.take(length).map(|_| ())
    }

    fn take(&mut self, length: usize) -> Result<&[u8], MessagePackError> {
        let end = self
            .offset
            .checked_add(length)
            .ok_or(MessagePackError::Truncated {
                offset: self.offset_u32(),
            })?;
        if end > self.payload.len() {
            return Err(MessagePackError::Truncated {
                offset: self.offset_u32(),
            });
        }
        let bytes = &self.payload[self.offset..end];
        self.offset = end;
        Ok(bytes)
    }

    fn offset_u32(&self) -> u32 {
        u32::try_from(self.offset).unwrap_or(u32::MAX)
    }
}
